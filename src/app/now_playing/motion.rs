//! The full player's animated artwork: found and downloaded on Tokio, then
//! decoded on a thread of its own and drawn frame by frame from a timer, as
//! Apple Music does, freezing while the song is paused.

use std::{
    path::PathBuf,
    rc::Rc,
    sync::mpsc::{Receiver, TryRecvError},
    time::{Duration, Instant},
};

use slint::{ComponentHandle, Image, TimerMode};

use super::super::App;
use crate::{Api as ApiGlobal, motion::video};

/// While waiting on the decoder for a frame.
const FRAME_TICK: Duration = Duration::from_millis(8);
/// While paused or minimised, only to notice the change.
const IDLE_TICK: Duration = Duration::from_millis(250);
/// A tick held up longer than this, as by a stall, doesn't skip the clip on.
const MAX_STEP: Duration = Duration::from_millis(100);

/// A clip being drawn.
pub(in crate::app) struct Playing {
    clip: PathBuf,
    /// None while the window is hidden, so nothing decodes.
    frames: Option<Receiver<video::Frame>>,
    next: Option<video::Frame>,
    /// Time the clip has played, which stops while the song is paused.
    played: Duration,
    last_tick: Instant,
    shown: bool,
}

impl App {
    /// Stops any clip, then finds the playing song's while the full player is
    /// open with animated artwork on.
    pub(in crate::app) fn refresh_motion(self: &Rc<Self>) {
        self.stop_motion();
        let ui = self.ui();
        let track = {
            let state = self.state.borrow();
            if !state.settings.animated_art || !ui.get_full_player() {
                return;
            }
            state.queue.current().cloned()
        };
        let Some(track) = track.filter(|t| !t.is_local()) else {
            return;
        };
        let generation = self.motion_generation.get();
        let motion = self.motion.clone();
        self.spawn(
            async move { motion.clip(&track).await },
            move |app, found| {
                if app.motion_generation.get() != generation {
                    return;
                }
                // Spotify may have taken its sign-in back.
                app.ui()
                    .set_spotify_signed_in(app.motion.spotify_signed_in());
                match found {
                    Ok(Some(clip)) => app.start_motion(clip),
                    Ok(None) => {}
                    Err(e) => log::warn!("{e}"),
                }
            },
        );
    }

    pub(in crate::app) fn stop_motion(&self) {
        self.motion_generation.set(self.motion_generation.get() + 1);
        self.motion_timer.stop();
        // Dropping the receiver ends the decoding thread.
        if self.motion_playing.borrow_mut().take().is_some() {
            self.ui().set_np_motion(Image::default());
        }
    }

    fn start_motion(self: &Rc<Self>, clip: PathBuf) {
        *self.motion_playing.borrow_mut() = Some(Playing {
            clip,
            frames: None,
            next: None,
            played: Duration::ZERO,
            last_tick: Instant::now(),
            shown: false,
        });
        self.motion_tick();
    }

    /// Draws the frame due, if any, then sets the timer for the next one.
    fn motion_tick(self: &Rc<Self>) {
        let Some(wait) = self.motion_frame() else {
            return;
        };
        let weak = Rc::downgrade(self);
        self.motion_timer
            .start(TimerMode::SingleShot, wait, move || {
                if let Some(app) = weak.upgrade() {
                    app.motion_tick();
                }
            });
    }

    /// Draws the frame due and says how long until the next, or None once
    /// the clip has stopped. Decodes only while the window is shown.
    fn motion_frame(&self) -> Option<Duration> {
        let ui = self.ui();
        let shown = super::super::window::is_shown(&ui);
        let playing = ui.global::<ApiGlobal>().get_is_playing();
        let mut slot = self.motion_playing.borrow_mut();
        let clip = slot.as_mut()?;
        let now = Instant::now();
        let step = now.duration_since(clip.last_tick).min(MAX_STEP);
        clip.last_tick = now;
        if !shown {
            // Started again from its start once shown; the last frame stays.
            clip.frames = None;
            clip.next = None;
            clip.played = Duration::ZERO;
            return Some(IDLE_TICK);
        }
        // The first frame is drawn even while paused, then held.
        let first = !clip.shown;
        if !playing && !first {
            return Some(IDLE_TICK);
        }
        if !first {
            clip.played += step;
        }
        let frames = clip
            .frames
            .get_or_insert_with(|| video::play(clip.clip.clone()));
        // Late frames are skipped, keeping the latest due.
        let mut due = None;
        loop {
            if clip.next.is_none() {
                match frames.try_recv() {
                    Ok(frame) => clip.next = Some(frame),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        // It won't decode here, so the still cover stays.
                        drop(slot);
                        self.stop_motion();
                        return None;
                    }
                }
            }
            let Some(frame) = clip.next.take_if(|f| first || f.at <= clip.played) else {
                break;
            };
            due = Some(frame);
            if first {
                break;
            }
        }
        if let Some(frame) = due {
            if first {
                clip.played = frame.at;
            }
            clip.shown = true;
            ui.set_np_motion(Image::from_rgba8(frame.pixels));
        }
        Some(match &clip.next {
            Some(next) => next.at.saturating_sub(clip.played).min(MAX_STEP),
            None => FRAME_TICK,
        })
    }
}
