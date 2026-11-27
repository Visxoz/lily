//! The transport controls: play, pause, seek, skip, volume, speed, shuffle
//! and repeat, each a press of the player bar's or a shortcut's.

use std::rc::Rc;

use crate::app::{App, session::Flush};

/// Pressing previous later than this restarts the track instead.
const RESTART_AFTER_SECS: f64 = 3.0;

impl App {
    /// The user's Next: the song is chosen, so earlier failures no longer
    /// count towards the run that stops playback.
    pub(in crate::app) fn skip(self: &Rc<Self>) {
        self.state.borrow_mut().playback.failures = 0;
        self.next(true);
    }

    /// Skips a song disliked or blocked, stopping on it at the queue's end
    /// or a repeat of it alone, rather than restarting it or playing on into
    /// a radio seeded by it, and drops the radio autoplay queued from it.
    pub(in crate::app) fn skip_past(self: &Rc<Self>, id: &str) {
        self.drop_autoplay_radio(id);
        let skips = (self.state.borrow().queue.peek_skip()).is_some_and(|t| t.id != id);
        if skips {
            self.skip();
            return;
        }
        // A radio still on its way would play on from it, as would the song
        // still resolving.
        self.state.borrow_mut().playback.generation += 1;
        self.cancel_extending();
        self.stop();
    }

    pub(in crate::app) fn next(self: &Rc<Self>, user: bool) {
        let moved = self.state.borrow_mut().queue.advance(user).is_some();
        if moved {
            self.start_current();
            return;
        }
        if self.state.borrow().settings.autoplay && !self.state.borrow().queue.is_empty() {
            // The radio may still be loading; queue it and move on.
            let (seed, generation) = {
                let state = self.state.borrow();
                (
                    state
                        .queue
                        .current()
                        .filter(|t| !t.is_local())
                        .map(|t| t.id.clone()),
                    state.playback.generation,
                )
            };
            if let Some(seed) = seed {
                let api = self.api.clone();
                self.spawn(
                    async move { api.track_radio(&seed).await },
                    move |app, result| {
                        // Something else started meanwhile. Not the queue's
                        // revision: the radio queued as the last track started
                        // may have just extended it.
                        if app.state.borrow().playback.generation != generation {
                            return;
                        }
                        // Failed, it may still move on to what that radio added.
                        match result {
                            Ok(tracks) => app.append_unique(tracks),
                            Err(e) => log::warn!("autoplay radio failed: {e}"),
                        }
                        if app.state.borrow_mut().queue.advance(true).is_some() {
                            app.start_current();
                            return;
                        }
                        app.stop();
                    },
                );
                return;
            }
        }
        if !user {
            self.stop();
        }
    }

    pub(in crate::app) fn previous(self: &Rc<Self>) {
        if self.player.position() > RESTART_AFTER_SECS {
            self.seek_to(0.0);
            // Starting over is a new play, as a replay through repeat-one is.
            if self.state.borrow().playback.started {
                self.record_play();
            }
            return;
        }
        if self.state.borrow_mut().queue.back().is_some() {
            self.state.borrow_mut().playback.failures = 0;
            self.start_current();
        }
    }

    pub(in crate::app) fn stop(self: &Rc<Self>) {
        self.state.borrow_mut().playback.started = false;
        self.player.stop();
        self.player.set_paused(true);
        let ui = self.ui();
        self.show_playing(false);
        ui.set_np_loading(false);
        self.report_playback();
        // Play carries on a song stopped on a dropped connection as the same play.
        let reopening = {
            let state = self.state.borrow();
            let current = state.queue.current();
            current.is_some_and(|t| state.playback.reopening(&t.id))
        };
        self.report_watchtime(if reopening { Flush::Now } else { Flush::Last });
    }

    pub(in crate::app) fn toggle_play(self: &Rc<Self>) {
        if !self.ui().get_has_track() {
            return;
        }
        if self.stopped() {
            // Replay the current track after it stopped at the queue's end.
            self.state.borrow_mut().playback.failures = 0;
            self.start_current();
            return;
        }
        self.set_paused(!self.player.is_paused());
    }

    /// Whether nothing is loaded for the track shown, as after a restart or
    /// once the queue has played out.
    pub(in crate::app) fn stopped(&self) -> bool {
        !self.state.borrow().playback.started && !self.ui().get_np_loading()
    }

    pub(in crate::app) fn set_paused(self: &Rc<Self>, paused: bool) {
        self.player.set_paused(paused);
        self.show_playing(!paused);
        self.report_playback();
        if paused {
            self.report_watchtime(Flush::Now);
        }
    }

    pub(in crate::app) fn seek_to(self: &Rc<Self>, secs: f64) {
        let duration = self.state.borrow().playback.duration;
        let secs = if duration > 0.0 {
            secs.clamp(0.0, duration - 0.5)
        } else {
            secs.max(0.0)
        };
        let stopped = self.stopped();
        {
            // With nothing to seek in, or a song being opened again, the
            // track starts here.
            let mut state = self.state.borrow_mut();
            if let Some(id) = state.queue.current().map(|t| t.id.clone())
                && (stopped || state.playback.reopening(&id))
            {
                state.playback.resume = Some((id, secs));
            }
        }
        if !stopped {
            self.player.seek(secs);
        }
        self.ui().set_np_position(secs as f32);
        self.show_lyric_line(secs);
        self.report_playback_at(secs);
    }

    /// Plays at the speed Settings lists at `index`.
    pub(in crate::app) fn set_speed(&self, index: usize) {
        let Some(&speed) = crate::app::SPEEDS.get(index) else {
            return;
        };
        self.update_settings(|s| s.speed = speed);
        self.player.set_speed(speed);
        // The overlay and Discord show clock time, which the speed scales.
        let track = self.state.borrow().queue.current().cloned();
        if let Some(track) = track {
            self.show_media_track(&track);
        }
        self.report_playback();
    }

    pub(in crate::app) fn set_volume(&self, volume: f32) {
        self.player.set_volume(volume);
        let mut state = self.state.borrow_mut();
        state.settings.volume = volume;
        state.settings.muted = false;
        drop(state);
        let ui = self.ui();
        ui.set_volume(volume);
        ui.set_muted(false);
    }

    pub(in crate::app) fn toggle_mute(&self) {
        let (muted, volume) = {
            let mut state = self.state.borrow_mut();
            state.settings.muted = !state.settings.muted;
            (state.settings.muted, state.settings.volume)
        };
        self.player.set_volume(if muted { 0.0 } else { volume });
        self.ui().set_muted(muted);
    }

    /// Cycles the shuffle button: off, shuffle, Smart Shuffle.
    pub(in crate::app) fn toggle_shuffle(self: &Rc<Self>) {
        let (shuffle, smart) = {
            let settings = &self.state.borrow().settings;
            match (settings.shuffle, settings.smart_shuffle) {
                (false, _) => (true, false),
                (true, false) => (true, true),
                (true, true) => (false, false),
            }
        };
        {
            let mut state = self.state.borrow_mut();
            if !smart {
                state.queue.remove_recommended();
                state.smart.reset();
            }
            state.queue.set_shuffle(shuffle);
        }
        self.update_settings(|s| {
            s.set_shuffle(shuffle);
            s.smart_shuffle = smart;
        });
        self.refresh_queue();
        self.prefetch_next();
    }

    pub(in crate::app) fn cycle_repeat(self: &Rc<Self>) {
        let repeat = {
            let mut state = self.state.borrow_mut();
            state.queue.repeat = state.queue.repeat.next();
            state.queue.repeat
        };
        self.update_settings(|s| s.repeat = repeat.index());
        self.prefetch_next();
    }
}
