//! The engine thread: decodes the current track into the sample queue, and
//! moves on to the queued one without a gap, with a crossfade or by Automix.

mod automix;
mod device;
mod gapless;
mod varispeed;

pub(super) use automix::TAIL_SECS as AUTOMIX_TAIL_SECS;

use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::Ordering,
        mpsc::{self, Receiver, RecvTimeoutError},
    },
    time::{Duration, Instant},
};

use gapless::{Fade, Next};

use super::{
    Command, Opening, PlayerEvent, Shared,
    decode::{Decoded, Decoder},
    loudness_gain,
    source::Download,
};

const BUFFER_AHEAD_SECS: f64 = 2.0;
const IDLE_POLL: Duration = Duration::from_millis(20);

struct Track {
    decoder: Decoder,
    download: Arc<Download>,
    finished: bool,
    started: bool,
}

pub(super) struct Engine {
    rx: Receiver<Command>,
    shared: Arc<Shared>,
    on_event: Box<dyn Fn(PlayerEvent) + Send>,
    stream: Option<(cpal::Stream, u32, usize)>,
    /// The device the open stream plays on, by its ID then, as the default's
    /// changes under it.
    playing_on: Option<cpal::DeviceId>,
    track: Option<Track>,
    next: Option<Next>,
    /// The track still audible after the queued one took over decoding,
    /// kept so a seek before the switch can return to it.
    previous: Option<Track>,
    /// The current track's samples held back from the device while the
    /// queued one may fade in over them.
    held: VecDeque<f32>,
    fade: Option<Fade>,
    scratch: Vec<f32>,
    /// When to stop the device after a pause.
    pause_at: Option<Instant>,
    /// When the chosen device was last looked for, while the default stands in.
    fallback_since: Option<Instant>,
    /// When the stream on the chosen device started, while it plays.
    chosen_since: Option<Instant>,
    /// Times in a row the chosen device failed to start, spacing out its rechecks.
    chosen_failures: u32,
    /// When to try again to open a device, after none would open.
    retry_at: Option<Instant>,
    /// The pause last applied. The shared flag runs ahead of its command, so a
    /// loss found as Play wakes the engine was heard while paused, and one
    /// found as Pause wakes it came after the user's pause.
    paused: bool,
    /// When the open stream, on whichever device, started.
    started_at: Option<Instant>,
}

impl Engine {
    pub(super) fn new(
        rx: Receiver<Command>,
        shared: Arc<Shared>,
        on_event: Box<dyn Fn(PlayerEvent) + Send>,
    ) -> Self {
        Self {
            rx,
            shared,
            on_event,
            stream: None,
            playing_on: None,
            track: None,
            next: None,
            previous: None,
            held: VecDeque::new(),
            fade: None,
            scratch: Vec::new(),
            pause_at: None,
            fallback_since: None,
            chosen_since: None,
            chosen_failures: 0,
            retry_at: None,
            paused: false,
            started_at: None,
        }
    }

    pub(super) fn run(mut self) {
        // Whether a pass since the last command or decode found nothing to do.
        let mut settled = false;
        loop {
            let command = if self.wants_samples() {
                match self.rx.try_recv() {
                    Ok(command) => Some(command),
                    Err(mpsc::TryRecvError::Empty) => None,
                    Err(mpsc::TryRecvError::Disconnected) => return,
                }
            } else if settled && self.idle() {
                match self.rx.recv() {
                    Ok(command) => Some(command),
                    Err(_) => return,
                }
            } else {
                match self.rx.recv_timeout(IDLE_POLL) {
                    Ok(command) => Some(command),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            };
            settled = false;
            // Ahead of the command, so a resume starts the new device.
            self.reopen_lost_device();
            match command {
                Some(Command::Load(opening, download)) => self.load(opening, download),
                Some(Command::Queue(opening, download, transition, sung)) => {
                    self.queue(opening, download, transition, sung)
                }
                Some(Command::ClearNext) => self.drop_next(),
                Some(Command::Seek(secs)) => self.seek(secs),
                Some(Command::Stop) => self.clear(),
                Some(Command::Pause) => self.apply_paused(),
                Some(Command::Reopen) => {
                    // Asked for, so not a failure to start however soon after one.
                    self.chosen_since = None;
                    self.chosen_failures = 0;
                    self.started_at = None;
                    if self.stream.is_some() {
                        self.shared.device_lost.store(true, Ordering::Relaxed);
                        self.reopen_lost_device();
                    }
                }
                None => {
                    self.recheck_device();
                    self.pause_when_due();
                    self.poll_next();
                    self.check_advanced();
                    self.try_switch();
                    self.spill();
                    if self.wants_samples() {
                        self.decode_some();
                    } else {
                        self.check_finished();
                        settled = true;
                    }
                }
            }
        }
    }

    /// Whether only a command can change anything, so the thread may block on
    /// one: paused or stopped, the device stopped, no queued track opening.
    /// Holds only while everything done in these states arrives as a command.
    fn idle(&self) -> bool {
        let opening = self.next.as_ref().is_some_and(|n| n.ready.is_none());
        let stopped = self.track.is_none() && self.previous.is_none();
        self.pause_at.is_none()
            && !opening
            && (stopped || self.shared.paused.load(Ordering::Relaxed))
    }

    fn clear(&mut self) {
        self.track = None;
        self.next = None;
        self.previous = None;
        // Freed rather than emptied, as a crossfade's hold is megabytes.
        self.held = VecDeque::new();
        self.fade = None;
        self.shared.output.lock().unwrap().reset(0.0);
    }

    fn wants_samples(&self) -> bool {
        let Some(track) = &self.track else {
            return false;
        };
        if track.finished {
            return false;
        }
        let output = self.shared.output.lock().unwrap();
        let ahead =
            output.samples.len() as f64 / (output.rate.max(1) as usize * output.channels) as f64;
        ahead < BUFFER_AHEAD_SECS || self.held.len() < self.hold()
    }

    fn load(&mut self, opening: Opening, download: Arc<Download>) {
        self.clear();
        self.shared.set_loudness(loudness_gain(opening.loudness_db));

        let mut decoder = match Decoder::open(download.reader(), &opening.mime) {
            Ok(decoder) => decoder,
            Err(e) => {
                // A newer load cancelled this one; stay quiet.
                if !self.is_current(&download) {
                    return;
                }
                (self.on_event)(failed(&download, e));
                return;
            }
        };
        if let Err(e) = self.ensure_stream(decoder.rate, decoder.channels) {
            (self.on_event)(PlayerEvent::Error(e));
            return;
        }
        log::debug!(
            "playing {} at {} Hz, {} ch",
            opening.mime,
            decoder.rate,
            decoder.channels
        );
        if opening.start_at > 0.0 {
            match decoder.seek(opening.start_at) {
                Ok(reached) => self.shared.output.lock().unwrap().reset(reached),
                Err(e) => log::warn!("could not resume at {:.0} s: {e}", opening.start_at),
            }
        }
        self.track = Some(Track {
            decoder,
            download,
            finished: false,
            started: false,
        });
    }

    fn is_current(&self, download: &Arc<Download>) -> bool {
        self.shared
            .download
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|d| Arc::ptr_eq(d, download))
    }

    fn seek(&mut self, secs: f64) {
        // A seek lands in the track still audible, so a queued track that
        // already took over decoding is dropped, and the app queues it again.
        if self.undo_switch() {
            (self.on_event)(PlayerEvent::NextDropped);
        }
        let Some(track) = &mut self.track else {
            return;
        };
        match track.decoder.seek(secs) {
            Ok(reached) => {
                track.finished = false;
                self.held.clear();
                self.fade = None;
                self.shared.output.lock().unwrap().reset(reached);
            }
            Err(e) => log::warn!("{e}"),
        }
    }

    fn decode_some(&mut self) {
        let Some(track) = &mut self.track else {
            return;
        };
        let mut scratch = std::mem::take(&mut self.scratch);
        scratch.clear();
        match track.decoder.decode_next(&mut scratch) {
            Ok(Decoded::Samples) => {
                // A seek's pre-roll decodes to nothing, which is not a sound.
                let first = !scratch.is_empty() && !std::mem::replace(&mut track.started, true);
                self.push(&mut scratch);
                if first {
                    (self.on_event)(PlayerEvent::Started);
                }
            }
            Ok(Decoded::EndOfStream) => {
                track.finished = true;
                // A track shorter than the fade ends inside it, and the
                // rest of the tail fades out over silence.
                if let Some(remaining) = self.fade.as_ref().map(Fade::remaining) {
                    scratch.resize(remaining, 0.0);
                    self.push(&mut scratch);
                }
            }
            Err(e) => {
                track.finished = true;
                // Errors from a download cancelled by a newer load are expected.
                let download = Arc::clone(&track.download);
                if self.is_current(&download) {
                    // Dropped, or as a finished track it would also report
                    // that it ended, skipping a second track.
                    self.track = None;
                    (self.on_event)(failed(&download, e));
                }
            }
        }
        self.scratch = scratch;
    }

    fn check_finished(&mut self) {
        let Some(track) = &self.track else {
            return;
        };
        // A queued track still opening takes over once it can.
        if self.next.is_some() {
            return;
        }
        if track.finished && self.shared.output.lock().unwrap().samples.is_empty() {
            // A stream that ends without a sound is broken, not finished: as
            // "ended" it would skip through the queue fetching every track.
            let event = if track.started {
                PlayerEvent::Ended
            } else {
                PlayerEvent::Error("the stream ended before playing".into())
            };
            self.track = None;
            (self.on_event)(event);
        }
    }
}

/// A track's failure, as a dropped connection when its download says so.
fn failed(download: &Download, error: String) -> PlayerEvent {
    if download.dropped() {
        PlayerEvent::Dropped(error)
    } else {
        PlayerEvent::Error(error)
    }
}
