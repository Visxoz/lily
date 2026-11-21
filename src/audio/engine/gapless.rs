//! Gapless and crossfaded moves to the queued track: opening it ahead, holding
//! back the current track's tail, mixing the fade and switching the decoder.

use std::{
    collections::VecDeque,
    f32::consts::FRAC_PI_2,
    sync::{
        Arc,
        mpsc::{self, Receiver},
    },
    thread,
};

use super::{BUFFER_AHEAD_SECS, Engine, Track};
use crate::audio::{
    Opening, PlayerEvent, SILENCE_TRIM_SECS,
    decode::{Decoded, Decoder},
    loudness_gain,
    opus::MAX_FRAME,
    output::Output,
    source::{Download, StreamReader},
};

/// Audio the device is always given, out of a crossfade's hold if need be,
/// so a slow download shortens the fade rather than breaking up the sound.
const LOW_WATER_SECS: f64 = 1.0;
/// About -60 dBFS: quieter than this, a frame counts as silence.
const SILENCE: f32 = 0.001;

/// The queued track, whose decoder opens on a side thread, as opening reads
/// the first of the download and would block the engine.
pub(super) struct Next {
    download: Arc<Download>,
    loudness: f32,
    opened: Receiver<Result<Opened, String>>,
    pub(super) ready: Option<Opened>,
    /// Samples of the current track held back for the fade and its trailing
    /// silence: none for gapless, or a format that cannot be mixed in.
    hold: usize,
    fade: usize,
}

pub(super) struct Opened {
    decoder: Decoder,
    /// The first samples after the leading silence cut for a crossfade.
    lead: Vec<f32>,
    skipped_secs: f64,
}

/// The end of the track fading out, mixed under the start of the next.
pub(super) struct Fade {
    tail: Vec<f32>,
    channels: usize,
    mixed: usize,
    /// The fading track's loudness correction over the next one's, as the
    /// device applies the next one's to the mix.
    old_gain: f32,
}

impl Fade {
    pub(super) fn remaining(&self) -> usize {
        self.tail.len() - self.mixed
    }

    fn done(&self) -> bool {
        self.remaining() == 0
    }

    /// Mixes the tail under `samples` in place. Equal-power curves keep the
    /// loudness steady through the overlap, where linear ones dip midway.
    fn mix(&mut self, samples: &mut [f32]) {
        let n = samples.len().min(self.remaining());
        let len = self.tail.len() as f32;
        let tail = &self.tail[self.mixed..self.mixed + n];
        let frames = samples[..n]
            .chunks_exact_mut(self.channels)
            .zip(tail.chunks_exact(self.channels));
        for (i, (new, old)) in frames.enumerate() {
            let t = (self.mixed + i * self.channels) as f32 / len;
            let (fade_in, fade_out) = (t * FRAC_PI_2).sin_cos();
            let fade_out = fade_out * self.old_gain;
            for (sample, old) in new.iter_mut().zip(old) {
                *sample = *sample * fade_in + old * fade_out;
            }
        }
        self.mixed += n;
    }
}

/// Interleaved samples in `secs` of audio, in whole frames.
fn samples_in(secs: f64, rate: u32, channels: usize) -> usize {
    (secs * f64::from(rate)) as usize * channels
}

/// How many of `held` samples move on to a device queue of `queued`: those
/// past the `hold`, and enough to bring the queue up to `low`, but never
/// past `ahead`. A whole crossfade hold let go at once, as when its queued
/// track is dropped, would otherwise grow the queue for the rest of the run.
fn to_spill(held: usize, hold: usize, queued: usize, low: usize, ahead: usize) -> usize {
    let short = low.saturating_sub(queued);
    let room = ahead.saturating_sub(queued).max(short);
    held.saturating_sub(hold.saturating_sub(short)).min(room)
}

fn same_format(decoder: &Decoder, output: &Output) -> bool {
    decoder.rate == output.rate && decoder.channels == output.channels
}

fn is_silent(frame: &[f32]) -> bool {
    frame.iter().all(|s| s.abs() < SILENCE)
}

/// Splits off the last `fade` samples before the trailing silence (at most
/// `trim` of it cut) and moves the rest to `out`. The tail keeps the held
/// buffer, so nothing is allocated. With no fade, nothing is cut.
fn take_tail(
    held: &mut VecDeque<f32>,
    out: &mut VecDeque<f32>,
    channels: usize,
    fade: usize,
    trim: usize,
) -> VecDeque<f32> {
    if fade > 0 {
        let silent = held
            .make_contiguous()
            .rchunks_exact(channels)
            .take(trim / channels)
            .take_while(|f| is_silent(f))
            .count();
        held.truncate(held.len() - silent * channels);
    }
    let start = held.len().saturating_sub(fade);
    out.extend(held.drain(..start));
    std::mem::take(held)
}

/// Opens the queued track and, for a crossfade, decodes up to its first
/// sound, so the fade brings in music rather than silence.
fn open_next(reader: StreamReader, mime: &str, trim: bool) -> Result<Opened, String> {
    let mut decoder = Decoder::open(reader, mime)?;
    let mut lead = Vec::new();
    let mut skipped = 0;
    if trim {
        let limit = (SILENCE_TRIM_SECS * f64::from(decoder.rate)) as usize;
        while lead.is_empty() && skipped < limit {
            if let Decoded::EndOfStream = decoder.decode_next(&mut lead)? {
                break;
            }
            let silent = lead
                .chunks_exact(decoder.channels)
                .take(limit - skipped)
                .take_while(|f| is_silent(f))
                .count();
            lead.drain(..silent * decoder.channels);
            skipped += silent;
        }
    }
    Ok(Opened {
        skipped_secs: skipped as f64 / f64::from(decoder.rate),
        decoder,
        lead,
    })
}

impl Engine {
    pub(super) fn queue(&mut self, opening: Opening, download: Arc<Download>, crossfade_secs: f64) {
        self.drop_next();
        let (rate, channels) = {
            let output = self.shared.output.lock().unwrap();
            (output.rate, output.channels)
        };
        let fade = samples_in(crossfade_secs, rate, channels);
        let hold = if fade > 0 {
            fade + samples_in(SILENCE_TRIM_SECS, rate, channels)
        } else {
            0
        };
        // Sized once, as growing by doubling would overshoot by up to half,
        // with room for the packet that lands before the excess spills.
        let room = if hold > 0 {
            hold + MAX_FRAME * channels
        } else {
            0
        };
        self.held
            .reserve_exact(room.saturating_sub(self.held.len()));
        let (tx, opened) = mpsc::channel();
        let reader = download.reader();
        let mime = opening.mime;
        let spawned = thread::Builder::new()
            .name("next-track".into())
            .spawn(move || {
                let _ = tx.send(open_next(reader, &mime, fade > 0));
            });
        if let Err(e) = spawned {
            log::warn!("could not open the next track: {e}");
            return;
        }
        self.next = Some(Next {
            download,
            loudness: loudness_gain(opening.loudness_db),
            opened,
            ready: None,
            hold,
            fade,
        });
    }

    /// Drops the queued track, which may already have taken over decoding.
    pub(super) fn drop_next(&mut self) {
        self.undo_switch();
        self.next = None;
    }

    pub(super) fn poll_next(&mut self) {
        let Some(next) = &mut self.next else {
            return;
        };
        if next.ready.is_some() {
            return;
        }
        let error = match next.opened.try_recv() {
            Ok(Ok(opened)) => {
                if !same_format(&opened.decoder, &self.shared.output.lock().unwrap()) {
                    next.hold = 0;
                }
                next.ready = Some(opened);
                return;
            }
            Ok(Err(e)) => e,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => "its opener stopped".to_owned(),
        };
        // The track then loads as usual once the current one ends.
        log::debug!("the next track did not open: {error}");
        let dropped = next.download.dropped();
        self.next = None;
        (self.on_event)(PlayerEvent::NextFailed { dropped });
    }

    /// Moves decoding on to the queued track once the current one is fully
    /// decoded. Of the same format, its samples follow straight on, mixed
    /// over the current track's held tail for a crossfade; otherwise the
    /// device reopens once the current track has played out.
    pub(super) fn try_switch(&mut self) {
        let finished = self.track.as_ref().is_some_and(|t| t.finished);
        let ready = self.next.as_ref().is_some_and(|n| n.ready.is_some());
        if !finished || !ready || self.previous.is_some() {
            return;
        }
        // Lined up before the output is locked, which the device callback
        // waits on; only what plays before the fade still moves under it.
        self.held.make_contiguous();
        let mut output = self.shared.output.lock().unwrap();
        let opened = self.next.as_ref().and_then(|n| n.ready.as_ref());
        let same_format = opened.is_some_and(|o| same_format(&o.decoder, &output));
        if !same_format && !(output.samples.is_empty() && self.held.is_empty()) {
            return;
        }
        let Some(Next {
            download,
            loudness,
            fade,
            ready:
                Some(Opened {
                    decoder,
                    mut lead,
                    skipped_secs,
                }),
            ..
        }) = self.next.take()
        else {
            return;
        };
        let (rate, channels) = (decoder.rate, decoder.channels);
        let track = Track {
            decoder,
            download,
            finished: false,
            started: true,
        };
        if same_format {
            let trim = samples_in(SILENCE_TRIM_SECS, rate, channels);
            let tail = take_tail(&mut self.held, &mut output.samples, channels, fade, trim);
            let queued = (output.samples.len() / channels) as u64;
            output.switch_at = Some(output.frames_played + queued);
            output.next_loudness = loudness;
            output.next_base_secs = skipped_secs;
            drop(output);
            if !tail.is_empty() {
                self.fade = Some(Fade {
                    tail: tail.into(),
                    channels,
                    mixed: 0,
                    old_gain: self.shared.loudness() / self.shared.correction(loudness),
                });
            }
            self.previous = self.track.replace(track);
        } else {
            drop(output);
            if let Err(e) = self.ensure_stream(rate, channels) {
                // Dropped, or the finished track would then report it ended.
                self.track = None;
                (self.on_event)(PlayerEvent::Error(e));
                return;
            }
            self.shared.output.lock().unwrap().reset(skipped_secs);
            self.shared.set_loudness(loudness);
            self.track = Some(track);
            self.became_audible();
        }
        self.push(&mut lead);
    }

    /// Undoes a switch the listener has not heard yet: the queued track's
    /// samples leave the queue and the previous track plays out, its tail
    /// held back again as it was before any fade.
    pub(super) fn undo_switch(&mut self) -> bool {
        if self.previous.is_none() {
            return false;
        }
        let mut output = self.shared.output.lock().unwrap();
        // Crossed already, which `check_advanced` has yet to see: the
        // switched track is the one heard, so it stays.
        if output.advanced {
            return false;
        }
        if let Some(switched) = std::mem::replace(&mut self.track, self.previous.take())
            // A song queued to play again shares the restored one's download.
            && !self
                .track
                .as_ref()
                .is_some_and(|t| Arc::ptr_eq(&t.download, &switched.download))
        {
            switched.download.cancel();
        }
        if let Some(at) = output.switch_at.take() {
            let frames = at.saturating_sub(output.frames_played) as usize;
            let keep = frames * output.channels;
            output.samples.truncate(keep);
        }
        self.held = self.fade.take().map(|f| f.tail.into()).unwrap_or_default();
        true
    }

    pub(super) fn check_advanced(&mut self) {
        let advanced = std::mem::take(&mut self.shared.output.lock().unwrap().advanced);
        if advanced {
            self.previous = None;
            if self.fade.as_ref().is_some_and(Fade::done) {
                self.fade = None;
            }
            self.became_audible();
        }
    }

    pub(super) fn hold(&self) -> usize {
        self.next.as_ref().map_or(0, |n| n.hold)
    }

    /// Passes decoded samples on to the device, mixed under the tail of the
    /// track fading out, and held back while the queued track may fade in.
    pub(super) fn push(&mut self, samples: &mut [f32]) {
        if let Some(fade) = &mut self.fade {
            fade.mix(samples);
            // Kept until heard, as undoing the switch restores the tail.
            if fade.done() && self.previous.is_none() {
                self.fade = None;
            }
        }
        if self.held.is_empty() && self.hold() == 0 {
            let mut output = self.shared.output.lock().unwrap();
            output.samples.extend(samples.iter());
        } else {
            self.held.extend(samples.iter());
            self.spill();
        }
    }

    /// Moves held samples on to the device beyond what the crossfade needs,
    /// and whatever the device needs to stay above its low water mark.
    pub(super) fn spill(&mut self) {
        if self.held.is_empty() {
            return;
        }
        let mut output = self.shared.output.lock().unwrap();
        let (rate, channels) = (output.rate, output.channels);
        let n = to_spill(
            self.held.len(),
            self.hold(),
            output.samples.len(),
            samples_in(LOW_WATER_SECS, rate, channels),
            samples_in(BUFFER_AHEAD_SECS, rate, channels),
        );
        output.samples.extend(self.held.drain(..n));
    }

    /// The current track's download becomes the audible one, for progress.
    fn became_audible(&self) {
        let Some(download) = self.track.as_ref().map(|t| Arc::clone(&t.download)) else {
            return;
        };
        {
            let mut next = self.shared.next_download.lock().unwrap();
            if next.as_ref().is_some_and(|n| Arc::ptr_eq(n, &download)) {
                *next = None;
            }
        }
        *self.shared.download.lock().unwrap() = Some(download);
        (self.on_event)(PlayerEvent::Advanced);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fade(tail: Vec<f32>, old_gain: f32) -> Fade {
        Fade {
            tail,
            channels: 2,
            mixed: 0,
            old_gain,
        }
    }

    #[test]
    fn fades_with_equal_power() {
        let mut fade = fade(vec![1.0; 2_000], 1.0);
        let mut samples = vec![1.0; 2_000];
        fade.mix(&mut samples);
        assert!(fade.done());
        assert_eq!(samples[..2], [1.0, 1.0], "starts on the old track alone");
        for [left, right] in samples.as_chunks::<2>().0 {
            assert_eq!(left, right, "channels fade together");
        }
        // Uncorrelated tracks sum by power, so the curves' squares sum to 1.
        let i = 500 * 2;
        let t = 500.0 / 1_000.0 * FRAC_PI_2;
        let (fade_in, fade_out) = t.sin_cos();
        assert!((fade_in * fade_in + fade_out * fade_out - 1.0).abs() < 1e-6);
        assert!((samples[i] - (fade_in + fade_out)).abs() < 1e-6);
        assert!(
            samples[1_998] > 0.99 && samples[1_998] < 1.01,
            "ends near the new track alone"
        );
    }

    #[test]
    fn mixes_across_packets_as_in_one() {
        let tail: Vec<f32> = (0..960).map(|i| (i as f32 * 0.01).sin()).collect();
        let new: Vec<f32> = (0..960).map(|i| (i as f32 * 0.03).cos()).collect();
        let mut whole = new.clone();
        fade(tail.clone(), 0.5).mix(&mut whole);

        let mut split = fade(tail, 0.5);
        let mut parts = new;
        for part in parts.chunks_mut(122) {
            split.mix(part);
        }
        assert_eq!(parts, whole);
    }

    #[test]
    fn leaves_samples_past_the_fade_alone() {
        let mut fade = fade(vec![0.5; 4], 2.0);
        let mut samples = vec![0.25; 10];
        fade.mix(&mut samples);
        assert!(fade.done());
        assert_eq!(samples[0], 1.0, "the old track at its relative loudness");
        assert_eq!(samples[4..], [0.25; 6]);
    }

    #[test]
    fn splits_the_tail_before_trailing_silence() {
        let mut held: VecDeque<f32> = [0.25; 4]
            .into_iter()
            .chain([0.5; 4])
            .chain([0.0; 6])
            .collect();
        let mut out = VecDeque::new();
        let tail = take_tail(&mut held, &mut out, 2, 4, 20);
        assert_eq!(tail, [0.5; 4]);
        assert_eq!(out, [0.25; 4], "the rest plays before the fade");
        assert!(held.is_empty());
    }

    #[test]
    fn cuts_no_more_silence_than_allowed() {
        let mut held: VecDeque<f32> = [0.5; 4].into_iter().chain([0.0; 8]).collect();
        let mut out = VecDeque::new();
        let tail = take_tail(&mut held, &mut out, 2, 4, 2);
        assert_eq!(tail, [0.0; 4], "a long silence is the music's, not the end");
        assert_eq!(out.len(), 6);
    }

    #[test]
    fn gapless_takes_no_tail() {
        let mut held: VecDeque<f32> = [0.5, 0.5, 0.0, 0.0].into_iter().collect();
        let mut out = VecDeque::new();
        assert!(take_tail(&mut held, &mut out, 2, 0, 20).is_empty());
        assert_eq!(
            out,
            [0.5, 0.5, 0.0, 0.0],
            "gapless keeps the silence, exactly"
        );
    }

    #[test]
    fn a_fade_longer_than_the_track_takes_all_of_it() {
        let mut held: VecDeque<f32> = [0.5; 6].into_iter().collect();
        let mut out = VecDeque::new();
        assert_eq!(take_tail(&mut held, &mut out, 2, 200, 20).len(), 6);
        assert!(out.is_empty());
    }

    #[test]
    fn spills_no_further_than_the_device_needs() {
        let (low, ahead) = (100, 200);
        assert_eq!(to_spill(1_000, 0, 0, low, ahead), ahead, "a dropped hold");
        assert_eq!(to_spill(1_000, 0, 150, low, ahead), 50, "tops up to ahead");
        assert_eq!(to_spill(50, 0, 0, low, ahead), 50, "all there is");
        assert_eq!(to_spill(900, 800, 150, low, ahead), 50, "past the hold");
        assert_eq!(
            to_spill(800, 800, 40, low, ahead),
            60,
            "up to low, from the hold"
        );
        assert_eq!(
            to_spill(300, 800, 0, low, ahead),
            0,
            "still filling the hold"
        );
    }

    #[test]
    fn hears_silence_below_the_threshold_only() {
        assert!(is_silent(&[0.0, -0.000_5]));
        assert!(!is_silent(&[0.0, 0.01]));
    }
}
