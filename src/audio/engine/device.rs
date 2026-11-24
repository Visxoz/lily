//! The output device: opening it at the track's format, falling back to the
//! default and back again, reopening a lost one, and stopping it while paused.

use std::{
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use super::Engine;
use crate::audio::{PlayerEvent, output::fill};

/// Long enough for the device to play out its buffer as silence.
const PAUSE_AFTER: Duration = Duration::from_millis(250);
/// How often to look for the chosen device while the default stands in.
const DEVICE_RECHECK: Duration = Duration::from_secs(3);
/// How soon after starting a lost chosen device counts as one that won't start.
const START_GRACE: Duration = Duration::from_secs(1);
/// How often to try opening a device while none will open.
const DEVICE_RETRY: Duration = Duration::from_secs(1);

/// The output device with this cpal ID, if it is plugged in.
fn find_device(host: &cpal::Host, id: &str) -> Option<cpal::Device> {
    host.device_by_id(&id.parse().ok()?)
}

/// Wait before trying the chosen device again, doubled per failed start in a row,
/// as each try drops the sound.
fn recheck_after(failures: u32) -> Duration {
    DEVICE_RECHECK * 2u32.pow(failures.min(5))
}

impl Engine {
    /// Opens the device at the track's format; Windows resamples if needed.
    pub(super) fn ensure_stream(&mut self, rate: u32, channels: usize) -> Result<(), String> {
        let lost = self.shared.device_lost.load(Ordering::Relaxed);
        // cpal starts a stream on its own thread and reports a failure there,
        // so a chosen device lost this soon is taken as broken, not unplugged.
        let played = self.chosen_since.map(|at| at.elapsed());
        let broken = lost && played.is_some_and(|p| p < START_GRACE);
        // A device open at another format closes before the format changes
        // under its callback, and a lost one closes to be opened afresh.
        if lost || !matches!(&self.stream, Some((_, r, c)) if *r == rate && *c == channels) {
            // A load or a format change can be the first to see the loss.
            if lost {
                self.pause_if_unplugged();
            }
            if broken {
                self.chosen_failures = self.chosen_failures.saturating_add(1);
            } else if played.is_some_and(|p| p >= START_GRACE) {
                self.chosen_failures = 0;
            }
            self.stream = None;
            self.chosen_since = None;
            // Closing waits on its callbacks, so it can't report itself lost after.
            self.shared.device_lost.store(false, Ordering::Relaxed);
        }
        {
            let mut output = self.shared.output.lock().unwrap();
            output.rate = rate;
            output.channels = channels;
            // A device opened mid-track, as after losing one, fades in too.
            if self.stream.is_none() {
                output.gain = 0.0;
            }
        }
        if self.stream.is_some() {
            return Ok(());
        }

        let host = cpal::default_host();
        let chosen = self.shared.device.lock().unwrap().clone();
        if let Some(device) = chosen
            .as_deref()
            .filter(|_| !broken)
            .and_then(|id| find_device(&host, id))
        {
            match self
                .build(&device, rate, channels)
                .and_then(|stream| self.start(&device, stream, rate, channels))
            {
                Ok(()) => {
                    self.fallback_since = None;
                    self.chosen_since = Some(Instant::now());
                    return Ok(());
                }
                Err(e) => log::warn!("{e}; playing on the default device"),
            }
        }
        // The default stands in until `recheck_device` finds the chosen one working.
        self.fallback_since = chosen.is_some().then(Instant::now);
        let device = host
            .default_output_device()
            .ok_or("no audio output device")?;
        let stream = self.build(&device, rate, channels)?;
        self.start(&device, stream, rate, channels)
    }

    /// An unstarted stream on `device`, its rate scaled by the speed.
    fn build(
        &self,
        device: &cpal::Device,
        rate: u32,
        channels: usize,
    ) -> Result<cpal::Stream, String> {
        let speed = f32::from_bits(self.shared.speed.load(Ordering::Relaxed));
        // Windows' resampler takes 8 to 384 kHz.
        let device_rate = ((rate as f32 * speed).round() as u32).clamp(8_000, 384_000);
        let config = cpal::StreamConfig {
            channels: channels as u16,
            sample_rate: device_rate,
            buffer_size: cpal::BufferSize::Default,
        };
        let shared = Arc::clone(&self.shared);
        let lost = Arc::clone(&self.shared);
        device
            .build_output_stream::<f32, _, _>(
                config,
                move |data, _| fill(&shared, data, device_rate),
                move |e| {
                    log::warn!("audio output error: {e}");
                    // The device went or another became the default. Other
                    // kinds can't be told from a refused priority boost; a
                    // stream they did end is found when it next fails to start.
                    if matches!(
                        e.kind(),
                        cpal::ErrorKind::DeviceNotAvailable
                            | cpal::ErrorKind::StreamInvalidated
                            | cpal::ErrorKind::HostUnavailable
                    ) {
                        lost.device_lost.store(true, Ordering::Relaxed);
                    }
                },
                None,
            )
            .map_err(|e| format!("could not open audio output: {e}"))
    }

    fn start(
        &mut self,
        device: &cpal::Device,
        stream: cpal::Stream,
        rate: u32,
        channels: usize,
    ) -> Result<(), String> {
        stream
            .play()
            .map_err(|e| format!("could not start audio output: {e}"))?;
        self.stream = Some((stream, rate, channels));
        self.playing_on = device.id().ok();
        self.started_at = Some(Instant::now());
        // A format change reached while paused opens a stream that must not run.
        self.apply_paused();
        Ok(())
    }

    /// Opens the device again once its stream is lost (unplugged, or another
    /// became the default, which cpal doesn't follow); the queue carries on.
    pub(super) fn reopen_lost_device(&mut self) {
        // An open stream reopens at once; with none open, the last try failed.
        if !self.shared.device_lost.load(Ordering::Relaxed)
            || (self.stream.is_none() && self.retry_at.is_some_and(|at| Instant::now() < at))
        {
            return;
        }
        self.pause_if_unplugged();
        // The default failing to start leaves nothing to fall back on, so it
        // closes and waits as if there were no device; a track load still tries.
        if self.stream.is_some()
            && self.chosen_since.is_none()
            && self.started_at.is_some_and(|at| at.elapsed() < START_GRACE)
        {
            self.stream = None;
            self.retry_at = Some(Instant::now() + DEVICE_RETRY);
            return;
        }
        let (rate, channels) = {
            let output = self.shared.output.lock().unwrap();
            (output.rate, output.channels)
        };
        if let Err(e) = self.ensure_stream(rate, channels) {
            log::debug!("{e}");
            self.retry_at = Some(Instant::now() + DEVICE_RETRY);
            self.shared.device_lost.store(true, Ordering::Relaxed);
        }
    }

    /// Moves back to the chosen device once it opens again, as cpal reports no
    /// device arriving.
    pub(super) fn recheck_device(&mut self) {
        let Some((_, rate, channels)) = self.stream else {
            return;
        };
        if self
            .fallback_since
            .is_none_or(|at| at.elapsed() < recheck_after(self.chosen_failures))
        {
            return;
        }
        self.fallback_since = Some(Instant::now());
        let chosen = self.shared.device.lock().unwrap().clone();
        let Some(device) = chosen.and_then(|id| find_device(&cpal::default_host(), &id)) else {
            return;
        };
        // Built while the default plays, so a failure leaves the sound alone.
        let Ok(stream) = self.build(&device, rate, channels) else {
            return;
        };
        // Closed first, as both would drain the queue, and before the loss is
        // cleared, as closing waits on its callbacks.
        self.stream = None;
        self.shared.device_lost.store(false, Ordering::Relaxed);
        self.shared.output.lock().unwrap().gain = 0.0;
        self.fallback_since = None;
        self.chosen_since = Some(Instant::now());
        if let Err(e) = self.start(&device, stream, rate, channels) {
            log::warn!("{e}");
            self.shared.device_lost.store(true, Ordering::Relaxed);
        }
    }

    /// Pauses before a lost stream reopens, should its device be gone, so
    /// nothing meant for headphones plays through the speakers; Play carries
    /// on from the queue. A device only no longer the default plays on there.
    fn pause_if_unplugged(&mut self) {
        if self.stream.is_none()
            || !self.shared.pause_on_unplug.load(Ordering::Relaxed)
            || self.paused
            || self.shared.paused.load(Ordering::Relaxed)
        {
            return;
        }
        // Unreadable, it counts as gone, pausing being the safe side.
        let host = cpal::default_host();
        if !(self.playing_on.as_ref()).is_some_and(|id| host.device_by_id(id).is_some()) {
            self.paused = true;
            self.shared.paused.store(true, Ordering::Relaxed);
            (self.on_event)(PlayerEvent::Unplugged);
        }
    }

    /// Stops the device while paused, as a running stream holds a power request
    /// that keeps Windows from sleeping. The stop waits `PAUSE_AFTER`, as WASAPI
    /// would replay what is left in its buffer on the next start.
    pub(super) fn apply_paused(&mut self) {
        self.paused = self.shared.paused.load(Ordering::Relaxed);
        let Some((stream, ..)) = &self.stream else {
            return;
        };
        if self.shared.paused.load(Ordering::Relaxed) {
            self.pause_at = Some(Instant::now() + PAUSE_AFTER);
            return;
        }
        self.pause_at = None;
        if let Err(e) = stream.play() {
            log::warn!("could not resume audio output: {e}");
            // Its thread has ended, whatever the error that ended it.
            self.shared.device_lost.store(true, Ordering::Relaxed);
        }
    }

    /// Stops the device once it has played out silence since the pause, and
    /// only with nothing left to decode: a decode can block on a slow
    /// download, and the resume that restarts the device would wait behind it.
    pub(super) fn pause_when_due(&mut self) {
        if self.pause_at.is_none_or(|at| Instant::now() < at) || self.wants_samples() {
            return;
        }
        self.pause_at = None;
        if let Some((stream, ..)) = &self.stream
            && self.shared.paused.load(Ordering::Relaxed)
            && let Err(e) = stream.pause()
        {
            log::warn!("could not pause audio output: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rechecks_a_failing_device_ever_more_rarely() {
        assert_eq!(recheck_after(0), DEVICE_RECHECK);
        assert_eq!(recheck_after(1), DEVICE_RECHECK * 2);
        assert_eq!(recheck_after(5), DEVICE_RECHECK * 32);
        assert_eq!(recheck_after(u32::MAX), recheck_after(5));
    }
}
