//! Gapless and crossfaded moves to the queued track: opening it ahead, holding
//! back the current track's tail, mixing the fade and switching the decoder.

use std::{
    collections::VecDeque,
    sync::{
        Arc,
        mpsc::{self, Receiver},
    },
    thread,
};

use super::{
    BUFFER_AHEAD_SECS, Engine, Track,
    automix::{self, Analysis, Mixer, Style, Voices},
    varispeed,
};
use crate::audio::{
    Opening, PlayerEvent, SILENCE_TRIM_SECS, Sung, Transition,
    decode::{Decoded, Decoder},
    loudness_gain,
    opus::MAX_FRAME,
    output::{Output, Varispeed, Warp},
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
    /// A crossfade's length in samples; Automix plans its own.
    fade: usize,
    sung: Sung,
}

pub(super) struct Opened {
    decoder: Decoder,
    /// The first samples after the leading silence cut for a crossfade, and
    /// for Automix its whole intro.
    lead: Vec<f32>,
    skipped_secs: f64,
    /// The intro analysed, for Automix only.
    intro: Option<Analysis>,
}

/// The end of the track fading out, mixed under the start of the next.
pub(super) struct Fade {
    tail: Vec<f32>,
    mixed: usize,
    /// The fading track's loudness correction over the next one's, as the
    /// device applies the next one's to the mix.
    old_gain: f32,
    mixer: Mixer,
    /// The tail as decoded from where Automix's varispeed starts, which an
    /// undone switch restores; none without one, or once the switch is heard.
    original: Option<VecDeque<f32>>,
}

impl Fade {
    pub(super) fn remaining(&self) -> usize {
        self.tail.len() - self.mixed
    }

    fn done(&self) -> bool {
        self.remaining() == 0
    }

    /// Mixes the tail under `samples` in place.
    fn mix(&mut self, samples: &mut [f32]) {
        let n = samples.len().min(self.remaining());
        let tail = &self.tail[self.mixed..self.mixed + n];
        self.mixer.mix(&mut samples[..n], tail, self.old_gain);
        self.mixed += n;
    }
}

/// A fade planned: the tail mixed under the next track, and for Automix's
/// varispeed its curve and the tail as decoded from where it starts.
struct Planned {
    tail: Vec<f32>,
    mixer: Mixer,
    warp: Option<(Varispeed, VecDeque<f32>)>,
}

/// Interleaved samples in `secs` of audio, in whole frames.
fn samples_in(secs: f64, rate: u32, channels: usize) -> usize {
    (secs * f64::from(rate)) as usize * channels
}

/// How many `held` samples move on to a device queue of `queued`: those past
/// the `hold`, and enough to reach `low`, never past `ahead`, as a dropped
/// track's whole hold let go at once would grow the queue for good.
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

/// Cuts the trailing silence from `held`, at most `trim` samples of it.
fn trim_silence(held: &mut VecDeque<f32>, channels: usize, trim: usize) {
    let silent = held
        .make_contiguous()
        .rchunks_exact(channels)
        .take(trim / channels)
        .take_while(|f| is_silent(f))
        .count();
    held.truncate(held.len() - silent * channels);
}

/// Opens the queued track off the engine thread and, unless gapless, decodes
/// up to its first sound, so the blend brings in music rather than silence;
/// Automix decodes on through the intro and analyses it.
fn open_next(reader: StreamReader, mime: &str, transition: Transition) -> Result<Opened, String> {
    let mut decoder = Decoder::open(reader, mime)?;
    let (rate, channels) = (decoder.rate, decoder.channels);
    let wanted = samples_in(automix::INTRO_SECS, rate, channels);
    let mut lead = Vec::new();
    if transition == Transition::Automix {
        // Sized once, as doubling would leave up to half of it unused.
        lead.reserve_exact(wanted + MAX_FRAME * channels);
    }
    let mut skipped = 0;
    if transition != Transition::Gapless {
        let limit = (SILENCE_TRIM_SECS * f64::from(rate)) as usize;
        while lead.is_empty() && skipped < limit {
            if let Decoded::EndOfStream = decoder.decode_next(&mut lead)? {
                break;
            }
            let silent = lead
                .chunks_exact(channels)
                .take(limit - skipped)
                .take_while(|f| is_silent(f))
                .count();
            lead.drain(..silent * channels);
            skipped += silent;
        }
    }
    let intro = if transition == Transition::Automix {
        while lead.len() < wanted {
            if let Decoded::EndOfStream = decoder.decode_next(&mut lead)? {
                break;
            }
        }
        Some(automix::analyse(&lead, rate, channels))
    } else {
        None
    };
    Ok(Opened {
        skipped_secs: skipped as f64 / f64::from(rate),
        decoder,
        lead,
        intro,
    })
}

impl Engine {
    pub(super) fn queue(
        &mut self,
        opening: Opening,
        download: Arc<Download>,
        transition: Transition,
        sung: Sung,
    ) {
        self.drop_next();
        let (rate, channels) = {
            let output = self.shared.output.lock().unwrap();
            (output.rate, output.channels)
        };
        let hold = samples_in(transition.hold_secs(), rate, channels);
        // Sized once, as doubling overshoots by up to half: room for a packet
        // landing before the excess spills, and Automix's intro, held at the switch.
        let (fade, room) = match transition {
            Transition::Gapless => (0, 0),
            Transition::Crossfade(secs) => (
                samples_in(secs, rate, channels),
                hold + MAX_FRAME * channels,
            ),
            Transition::Automix => (0, hold + samples_in(automix::INTRO_SECS, rate, channels)),
        };
        self.held
            .reserve_exact(room.saturating_sub(self.held.len()));
        let (tx, opened) = mpsc::channel();
        let reader = download.reader();
        let mime = opening.mime;
        let spawned = thread::Builder::new()
            .name("next-track".into())
            .spawn(move || {
                let _ = tx.send(open_next(reader, &mime, transition));
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
            sung,
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

    /// Moves decoding on to the queued track once the current one is decoded:
    /// of the same format, straight on, mixed over the held tail for a blend;
    /// otherwise the device reopens once the current track has played out.
    pub(super) fn try_switch(&mut self) {
        let finished = self.track.as_ref().is_some_and(|t| t.finished);
        let ready = self.next.as_ref().is_some_and(|n| n.ready.is_some());
        if !finished || !ready || self.previous.is_some() {
            return;
        }
        let (same_format, held_from) = {
            let output = self.shared.output.lock().unwrap();
            // An undone switch's varispeed is still to be heard and counted.
            if output.warp.is_some() {
                return;
            }
            let opened = self.next.as_ref().and_then(|n| n.ready.as_ref());
            let same_format = opened.is_some_and(|o| same_format(&o.decoder, &output));
            if !same_format && !(output.samples.is_empty() && self.held.is_empty()) {
                return;
            }
            // Where the held samples start, in song time.
            let queued = output.samples.len() / output.channels.max(1);
            let held_from = output.position() + queued as f64 / f64::from(output.rate.max(1));
            (same_format, held_from)
        };
        let Some(Next {
            download,
            loudness,
            fade,
            sung,
            ready:
                Some(Opened {
                    decoder,
                    mut lead,
                    skipped_secs,
                    intro,
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
            // Planned before locking the output, which the device callback
            // waits on. What plays before the fade stays held, to spill.
            let voices = |tail_end| Voices::new(&sung, tail_end, skipped_secs, rate);
            let planned = self.plan_fade(fade, intro.as_ref(), held_from, voices, rate, channels);
            let mut output = self.shared.output.lock().unwrap();
            let queued = ((output.samples.len() + self.held.len()) / channels) as u64;
            let switch_at = output.frames_played + queued;
            output.switch_at = Some(switch_at);
            output.next_loudness = loudness;
            output.next_base_secs = skipped_secs;
            // The ramp, the last of what is held, ends at the switch.
            output.warp = planned
                .as_ref()
                .and_then(|p| p.warp.as_ref())
                .map(|&(curve, _)| Warp {
                    from: switch_at - curve.ramp as u64,
                    until: switch_at,
                    curve,
                });
            drop(output);
            if let Some(Planned { tail, mixer, warp }) = planned {
                self.fade = Some(Fade {
                    tail,
                    mixed: 0,
                    old_gain: self.shared.loudness() / self.shared.correction(loudness),
                    mixer,
                    original: warp.map(|(_, original)| original),
                });
            }
            self.previous = self.track.replace(track);
        } else {
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
        // Behind the hold and through `spill`, as Automix's whole intro
        // pushed at once would grow the device queue for good.
        self.mix_fade(&mut lead);
        self.held.extend(lead.iter());
        self.spill();
    }

    /// Trims the held tail's trailing silence and splits the fade off it: a
    /// crossfade's own, or the one Automix plans from the held samples, which
    /// start at song second `held_from`, and where the songs are sung by the
    /// tail's end. Varispeed plays its ramp from what stays held.
    fn plan_fade(
        &mut self,
        fade: usize,
        intro: Option<&Analysis>,
        held_from: f64,
        voices: impl FnOnce(f64) -> Voices,
        rate: u32,
        channels: usize,
    ) -> Option<Planned> {
        if fade == 0 && intro.is_none() {
            return None;
        }
        let trim = samples_in(SILENCE_TRIM_SECS, rate, channels);
        trim_silence(&mut self.held, channels, trim);
        let Some(intro) = intro else {
            let tail: Vec<f32> = self
                .held
                .split_off(self.held.len().saturating_sub(fade))
                .into();
            return (!tail.is_empty()).then(|| Planned {
                mixer: Mixer::new(tail.len() / channels, Style::CROSSFADE, rate, channels),
                tail,
                warp: None,
            });
        };
        let held = self.held.make_contiguous();
        let tail_end = held_from + (held.len() / channels) as f64 / f64::from(rate);
        let mix = automix::plan(
            &automix::analyse(held, rate, channels),
            intro,
            rate,
            voices(tail_end),
        );
        if mix.frames == 0 {
            return None;
        }
        let mixer = Mixer::new(mix.frames, mix.style, rate, channels);
        let Some((start, curve)) = mix.warp else {
            let tail = self.held.split_off(self.held.len() - mix.frames * channels);
            return Some(Planned {
                tail: tail.into(),
                mixer,
                warp: None,
            });
        };
        // Split off from as far back as the ramp's taps reach, so it is
        // resampled straight into `held` as from the whole tail.
        let from = start.saturating_sub(varispeed::REACH);
        let lead = (start - from) * channels;
        let mut original = self.held.split_off(from * channels);
        let decoded: &[f32] = original.make_contiguous();
        let (ramp, frames) = (curve.ramp as usize, mix.frames);
        let resample = |range| varispeed::resample(decoded, channels, start - from, curve, range);
        self.held.extend(&decoded[..lead]);
        self.held.extend(resample(0..ramp));
        let mut tail = Vec::with_capacity(frames * channels);
        tail.extend(resample(ramp..ramp + frames));
        original.drain(..lead);
        Some(Planned {
            tail,
            mixer,
            warp: Some((curve, original)),
        })
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
        // The previous track's samples before the switch are queued, then
        // held; its tail goes back after them.
        let channels = output.channels;
        // Song frames of the varispeed already queued, whose originals the
        // rest carries on from.
        let mut resume = 0;
        if let Some(at) = output.switch_at.take() {
            let frames = at.saturating_sub(output.frames_played) as usize;
            let keep = frames * channels;
            self.held
                .truncate(keep.saturating_sub(output.samples.len()));
            output.samples.truncate(keep);
            let queued = output.frames_played + (output.samples.len() / channels) as u64;
            if let Some(warp) = output.warp {
                if queued > warp.from {
                    // Kept rather than cut, which could starve the device;
                    // the pitch steps back to the song's, by up to 6%.
                    self.held.clear();
                    resume = warp.curve.song_frames((queued - warp.from) as f64).round() as usize;
                    output.warp = Some(Warp {
                        until: queued,
                        ..warp
                    });
                } else {
                    self.held.truncate((warp.from - queued) as usize * channels);
                    output.warp = None;
                }
            }
        }
        // Unlocked, as the tail is megabytes to copy.
        drop(output);
        if let Some(fade) = self.fade.take() {
            let mut restored = fade.original.unwrap_or_else(|| fade.tail.into());
            restored.drain(..(resume * channels).min(restored.len()));
            if self.held.is_empty() {
                self.held = restored;
            } else {
                self.held.extend(restored);
            }
        }
        true
    }

    pub(super) fn check_advanced(&mut self) {
        let advanced = std::mem::take(&mut self.shared.output.lock().unwrap().advanced);
        if advanced {
            self.previous = None;
            if self.fade.as_ref().is_some_and(Fade::done) {
                self.fade = None;
            } else if let Some(fade) = &mut self.fade {
                // Heard, so never restored.
                fade.original = None;
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
        self.mix_fade(samples);
        if self.held.is_empty() && self.hold() == 0 {
            let mut output = self.shared.output.lock().unwrap();
            output.samples.extend(samples.iter());
        } else {
            self.held.extend(samples.iter());
            self.spill();
        }
    }

    fn mix_fade(&mut self, samples: &mut [f32]) {
        if let Some(fade) = &mut self.fade {
            fade.mix(samples);
            // Kept until heard, as undoing the switch restores the tail.
            if fade.done() && self.previous.is_none() {
                self.fade = None;
            }
        }
    }

    /// Moves held samples on to the device beyond what the hold needs, and
    /// whatever the device needs to stay above its low water mark.
    pub(super) fn spill(&mut self) {
        if !self.held.is_empty() {
            // Decoded to its end, the track can't fill the hold, so the device
            // draws on it: a shorter blend rather than a gap.
            let hold = match &self.track {
                Some(t) if t.finished => self.hold().min(self.held.len()),
                _ => self.hold(),
            };
            let mut output = self.shared.output.lock().unwrap();
            let (rate, channels) = (output.rate, output.channels);
            let n = to_spill(
                self.held.len(),
                hold,
                output.samples.len(),
                samples_in(LOW_WATER_SECS, rate, channels),
                samples_in(BUFFER_AHEAD_SECS, rate, channels),
            );
            output.samples.extend(self.held.drain(..n));
        }
        // Freed however it emptied (a seek clears it), as a hold is megabytes,
        // and unlocked, so the device never waits on the free.
        if self.held.is_empty() && self.hold() == 0 {
            self.held = VecDeque::new();
        }
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
    use std::{
        f32::consts::FRAC_PI_2,
        path::{Path, PathBuf},
        sync::Mutex,
        time::Duration,
    };

    use super::*;
    use crate::audio::{Shared, output::fill};

    /// Seeded mono 8 kHz noise `secs` long, written as a WAV, and the samples
    /// it decodes to.
    fn track(name: &str, mut seed: u32, secs: f64) -> (PathBuf, Vec<f32>) {
        let samples: Vec<f32> = (0..(secs * 8_000.0) as usize)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (seed >> 8) as f32 / (1 << 24) as f32 - 0.5
            })
            .collect();
        written(name, &samples, 1)
    }

    /// Stereo 8 kHz clicks at `bpm`, `secs` long, the right half as loud, as
    /// `track` writes them, over a floor too loud to trim as silence.
    fn clicks(name: &str, bpm: f64, secs: f64) -> (PathBuf, Vec<f32>) {
        let period = 60.0 / bpm * 8_000.0;
        let samples: Vec<f32> = (0..(secs * 8_000.0) as usize)
            .flat_map(|i| {
                let s = if (i as f64 - 1_000.0).rem_euclid(period) < 40.0 {
                    0.8
                } else {
                    0.002
                };
                [s, s / 2.0]
            })
            .collect();
        written(name, &samples, 2)
    }

    fn written(name: &str, samples: &[f32], channels: u16) -> (PathBuf, Vec<f32>) {
        let path = std::env::temp_dir().join(format!("lily-{name}-{}.wav", std::process::id()));
        crate::local::tests::write_wav(&path, samples, 8_000, channels);
        let mut decoder = Decoder::open(Download::open(path.clone()).reader(), "").unwrap();
        let mut decoded = Vec::new();
        while let Ok(Decoded::Samples) = decoder.decode_next(&mut decoded) {}
        (path, decoded)
    }

    /// The engine driven as its thread drives it, with the device drained
    /// 10 ms at a time by its own callback, and everything it heard.
    struct Run {
        engine: Engine,
        shared: Arc<Shared>,
        events: Arc<Mutex<Vec<String>>>,
        heard: Vec<f32>,
    }

    impl Run {
        fn new(first: &Path) -> Self {
            let download = Download::open(first.to_owned());
            let decoder = Decoder::open(download.reader(), "").unwrap();
            let shared = Arc::new(Shared::default());
            {
                let mut output = shared.output.lock().unwrap();
                (output.rate, output.channels, output.gain) = (decoder.rate, decoder.channels, 1.0);
            }
            let events = Arc::new(Mutex::new(Vec::new()));
            let log = Arc::clone(&events);
            let on_event = move |e: PlayerEvent| log.lock().unwrap().push(format!("{e:?}"));
            let (_, rx) = mpsc::channel();
            let mut engine = Engine::new(rx, Arc::clone(&shared), Box::new(on_event));
            *shared.download.lock().unwrap() = Some(Arc::clone(&download));
            engine.track = Some(Track {
                decoder,
                download,
                finished: false,
                started: false,
            });
            Self {
                engine,
                shared,
                events,
                heard: Vec::new(),
            }
        }

        fn queue(&mut self, next: &Path, transition: Transition, loudness_db: Option<f32>) {
            self.start_opening(next, transition, loudness_db);
            self.open();
        }

        fn start_opening(&mut self, next: &Path, transition: Transition, loudness_db: Option<f32>) {
            let opening = Opening {
                mime: String::new(),
                loudness_db,
                start_at: 0.0,
            };
            self.engine.queue(
                opening,
                Download::open(next.to_owned()),
                transition,
                Sung::default(),
            );
        }

        /// Waits for the queued track to open.
        fn open(&mut self) {
            while self.engine.next.as_ref().is_some_and(|n| n.ready.is_none()) {
                self.engine.poll_next();
                thread::sleep(Duration::from_millis(1));
            }
        }

        /// Plays until `stop` holds or nothing is left to play.
        fn play(&mut self, stop: impl Fn(&Engine, usize) -> bool) {
            let (rate, channels) = {
                let output = self.shared.output.lock().unwrap();
                (output.rate, output.channels)
            };
            // 10 ms at a time.
            let mut data = vec![0.0; rate as usize / 100 * channels];
            for _ in 0..1_000_000 {
                if stop(&self.engine, self.heard.len()) {
                    return;
                }
                self.engine.check_advanced();
                self.engine.try_switch();
                self.engine.spill();
                if self.engine.wants_samples() {
                    self.engine.decode_some();
                    continue;
                }
                self.engine.check_finished();
                let queued = self.shared.output.lock().unwrap().samples.len();
                // A packet over what the engine buffers ahead, never a hold.
                assert!(
                    queued <= samples_in(BUFFER_AHEAD_SECS + 0.5, rate, channels),
                    "device queue grew to {queued}"
                );
                let n = queued.min(data.len());
                if n < data.len() {
                    let done = self.engine.track.as_ref().is_none_or(|t| t.finished);
                    assert!(done && self.engine.held.is_empty(), "starved");
                }
                if n == 0 && self.engine.track.is_none() {
                    return;
                }
                fill(&self.shared, &mut data, rate);
                self.heard.extend(&data[..n]);
            }
            panic!("never finished");
        }

        fn events(&self) -> Vec<String> {
            self.events.lock().unwrap().clone()
        }
    }

    /// Plays `first` into a `second` 6 dB louder, checking what is heard is
    /// `first` up to the `fade` frames, then `second` turned down, so a switch
    /// the device reached early or late would show.
    fn plays_through(name: &str, transition: Transition, second_secs: f64, fade: usize) {
        let (a, first) = track(&format!("{name}-a"), 1, 30.0);
        let (b, second) = track(&format!("{name}-b"), 2, second_secs);
        let mut run = Run::new(&a);
        run.queue(&b, transition, Some(6.0));
        run.play(|_, _| false);
        let _ = (std::fs::remove_file(a), std::fs::remove_file(b));
        let start = first.len() - fade;
        assert_eq!(run.heard.len(), start + second.len());
        assert!(
            run.heard[..start] == first[..start],
            "the first track, whole up to the fade"
        );
        let gain = run
            .shared
            .correction(crate::audio::loudness_gain(Some(6.0)));
        let rest = run.heard[first.len()..].iter().zip(&second[fade..]);
        assert!(
            rest.into_iter()
                .all(|(heard, s)| (heard - s * gain).abs() < 1e-6),
            "then the second, whole at its loudness"
        );
        assert_eq!(run.events(), ["Started", "Advanced", "Ended"]);
    }

    #[test]
    fn runs_gapless_straight_on() {
        plays_through("gapless", Transition::Gapless, 15.0, 0);
    }

    #[test]
    fn crossfades_over_the_length_set() {
        plays_through("crossfade", Transition::Crossfade(1.0), 15.0, 8_000);
    }

    #[test]
    fn automixes_hard_ends_over_the_shortest_blend() {
        plays_through("automix", Transition::Automix, 15.0, 3 * 8_000);
    }

    #[test]
    fn automixes_into_a_track_shorter_than_its_intro() {
        // Over half the track, under the shortest blend, decoded whole ahead.
        plays_through("automix-short", Transition::Automix, 4.0, 2 * 8_000);
    }

    #[test]
    fn plays_the_hold_on_while_the_next_track_opens() {
        // Decoded to its end, under the hold, with the next yet to open.
        let (a, first) = track("opening-a", 5, 10.0);
        let (b, _) = track("opening-b", 6, 15.0);
        let mut run = Run::new(&a);
        run.start_opening(&b, Transition::Automix, None);
        run.play(|_, heard| heard >= 4 * 8_000);
        assert!(
            run.heard[..] == first[..run.heard.len()],
            "the first track, without a gap"
        );
        run.open();
        run.play(|_, _| false);
        let _ = (std::fs::remove_file(a), std::fs::remove_file(b));
        assert_eq!(run.events(), ["Started", "Advanced", "Ended"]);
    }

    /// Undoes a switch a second on, as a seek would land, or once the device
    /// queue reaches into the blend (`late`), and checks only the first track
    /// is heard.
    fn undoes(name: &str, transition: Transition, late: bool) {
        let (a, first) = track(&format!("{name}-a"), 3, 30.0);
        let (b, _) = track(&format!("{name}-b"), 4, 15.0);
        let mut run = Run::new(&a);
        run.queue(&b, transition, None);
        run.play(|engine, _| engine.previous.is_some());
        let switched = run.heard.len();
        run.play(|engine, heard| {
            if !late {
                return heard >= switched + 8_000;
            }
            // Mono, so samples are frames.
            let o = engine.shared.output.lock().unwrap();
            o.switch_at
                .is_some_and(|at| o.frames_played + o.samples.len() as u64 > at + 800)
        });
        assert!(run.engine.undo_switch(), "{name}: undone");
        run.play(|_, _| false);
        let _ = (std::fs::remove_file(a), std::fs::remove_file(b));
        assert!(
            run.heard == first,
            "{name}: the first track whole, nothing of the second"
        );
        assert_eq!(run.events(), ["Started", "Ended"], "{name}");
    }

    #[test]
    fn undoing_a_crossfade_plays_the_first_track_out() {
        undoes("undo-crossfade", Transition::Crossfade(1.0), false);
    }

    #[test]
    fn undoing_an_automix_switch_plays_the_first_track_out() {
        undoes("undo-automix", Transition::Automix, false);
    }

    #[test]
    fn undoing_a_switch_already_queued_drops_the_blend() {
        undoes("undo-late", Transition::Automix, true);
    }

    /// Where a switch is undone whose tail is sped up to the next track's
    /// tempo: before its ramp reaches the device queue, part way up it, or
    /// once the queue reaches into the blend after it.
    #[derive(Debug, Clone, Copy)]
    enum Undo {
        Before,
        During,
        After,
    }

    /// Checks what is heard is the first track as decoded but for the
    /// varispeed queued before the undo, carrying on from the song time it
    /// reached, and that the position ends at the track's end. In stereo, so
    /// samples and frames differ.
    fn undoes_varispeed(at: Undo) {
        let (a, first) = clicks(&format!("vs-{at:?}-a"), 120.0, 30.0);
        let (b, _) = clicks(&format!("vs-{at:?}-b"), 123.0, 15.0);
        let mut run = Run::new(&a);
        run.queue(&b, Transition::Automix, None);
        run.play(|engine, _| engine.previous.is_some());
        let warp = run.shared.output.lock().unwrap().warp.expect("varispeed");
        run.play(|engine, _| {
            let o = engine.shared.output.lock().unwrap();
            let queued = o.frames_played + (o.samples.len() / 2) as u64;
            match at {
                Undo::Before => true,
                Undo::During => queued > warp.from + 8_000,
                Undo::After => queued > warp.until + 800,
            }
        });
        assert!(run.engine.undo_switch(), "{at:?}: undone");
        let kept = run.shared.output.lock().unwrap().warp;
        run.play(|_, _| false);
        let position = run.shared.output.lock().unwrap().position();
        let _ = (std::fs::remove_file(a), std::fs::remove_file(b));
        let end = first.len() / 2;
        let (from, until, resume) = kept.map_or((end, end, end), |w| {
            let song = w.curve.song_frames((w.until - w.from) as f64).round() as usize;
            (w.from as usize, w.until as usize, w.from as usize + song)
        });
        assert!(
            matches!(at, Undo::Before) == kept.is_none(),
            "{at:?}: {kept:?}"
        );
        assert!(
            run.heard[..from * 2] == first[..from * 2],
            "{at:?}: whole up to it"
        );
        assert!(
            run.heard[until * 2..] == first[resume * 2..],
            "{at:?}: then on from where it left off"
        );
        let secs = end as f64 / 8_000.0;
        assert!(
            (position - secs).abs() < 1e-3,
            "{at:?}: ends at {position} s"
        );
        assert_eq!(run.events(), ["Started", "Ended"], "{at:?}");
    }

    #[test]
    fn undoing_varispeed_restores_the_tail_as_decoded() {
        for at in [Undo::Before, Undo::During, Undo::After] {
            undoes_varispeed(at);
        }
    }

    /// Mixes each audio file in `LILY_MIX_DIR`, else the app's downloads,
    /// into the next, or a lone one into itself, from 40 s before its end to
    /// about 20 s into the next, and writes what is heard to WAVs to hear.
    #[test]
    #[ignore = "renders real tracks, to listen to"]
    fn renders_automixes() {
        struct Print;
        impl log::Log for Print {
            fn enabled(&self, metadata: &log::Metadata) -> bool {
                metadata.target().ends_with("automix")
            }
            fn log(&self, record: &log::Record) {
                if self.enabled(record.metadata()) {
                    println!("  {}", record.args());
                }
            }
            fn flush(&self) {}
        }
        let _ = log::set_logger(&Print);
        log::set_max_level(log::LevelFilter::Debug);
        let dir = std::env::var_os("LILY_MIX_DIR").map_or_else(
            || crate::settings::data_dir().join("downloads"),
            PathBuf::from,
        );
        let audio = ["webm", "m4a", "mp4", "opus", "ogg", "mp3", "flac", "wav"];
        let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| Some(e.ok()?.path()))
            .filter(|p| {
                p.extension()
                    .is_some_and(|e| audio.iter().any(|a| e.eq_ignore_ascii_case(a)))
            })
            .collect();
        files.sort();
        assert!(!files.is_empty(), "no audio in {}", dir.display());
        let pairs: Vec<(usize, usize)> = if files.len() == 1 {
            vec![(0, 0)]
        } else {
            (1..files.len()).map(|i| (i - 1, i)).collect()
        };
        for (a, b) in pairs {
            println!("{} into {}", files[a].display(), files[b].display());
            let mut run = Run::new(&files[a]);
            let secs = run.engine.track.as_ref().and_then(|t| t.decoder.secs());
            run.engine.seek((secs.expect("its length") - 40.0).max(0.0));
            run.queue(&files[b], Transition::Automix, None);
            let (rate, channels) = {
                let output = run.shared.output.lock().unwrap();
                (output.rate, output.channels)
            };
            let wanted = samples_in(60.0, rate, channels);
            run.play(|_, heard| heard >= wanted);
            let path = std::env::temp_dir().join(format!("lily-automix-{a}-{b}.wav"));
            crate::local::tests::write_wav(&path, &run.heard, rate, channels as u16);
            println!("  wrote {}", path.display());
        }
    }

    fn fade(tail: Vec<f32>, old_gain: f32) -> Fade {
        Fade {
            mixer: Mixer::new(tail.len() / 2, Style::CROSSFADE, 48_000, 2),
            tail,
            mixed: 0,
            old_gain,
            original: None,
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
    fn trades_bass_across_packets_as_in_one() {
        // Both bass, about 150 and 120 Hz, so it trades.
        let tail: Vec<f32> = (0..9_600).map(|i| (i as f32 * 0.01).sin()).collect();
        let new: Vec<f32> = (0..9_600).map(|i| (i as f32 * 0.008).cos()).collect();
        let plan = || {
            let analyse = |s: &[f32]| automix::analyse(s, 48_000, 2);
            let mix = automix::plan(&analyse(&tail), &analyse(&new), 48_000, Voices::default());
            Fade {
                mixer: Mixer::new(mix.frames, mix.style, 48_000, 2),
                ..fade(tail.clone(), 0.5)
            }
        };
        let mut whole = new.clone();
        plan().mix(&mut whole);
        let mut split = plan();
        let mut parts = new.clone();
        for part in parts.chunks_mut(122) {
            split.mix(part);
        }
        assert!(split.done());
        assert_eq!(parts, whole);
        assert_ne!(parts, new, "mixed");
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
    fn trims_trailing_silence() {
        let mut held: VecDeque<f32> = [0.25; 4]
            .into_iter()
            .chain([0.5; 4])
            .chain([0.0; 6])
            .collect();
        trim_silence(&mut held, 2, 20);
        assert_eq!(held, [0.25, 0.25, 0.25, 0.25, 0.5, 0.5, 0.5, 0.5]);
    }

    #[test]
    fn cuts_no_more_silence_than_allowed() {
        let mut held: VecDeque<f32> = [0.5; 4].into_iter().chain([0.0; 8]).collect();
        trim_silence(&mut held, 2, 2);
        assert_eq!(held.len(), 10, "a long silence is the music's, not the end");
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
