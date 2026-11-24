//! The sleep timer: pauses after a number of minutes, or once the song
//! playing ends.

use std::{
    rc::Rc,
    time::{Duration, Instant},
};

use slint::{ComponentHandle, SharedString};

use crate::{Api as ApiGlobal, app::App};

/// What the player bar sends for "End of song"; 0 turns the timer off.
const END_OF_SONG: i32 = -1;
const PAUSED: &str = "Paused by the sleep timer";

#[derive(Clone, Copy)]
pub(in crate::app) enum Sleep {
    At(Instant),
    EndOfSong,
}

impl App {
    /// Sets the timer to `minutes` from now, `END_OF_SONG`, or off for 0.
    pub(in crate::app) fn set_sleep(&self, minutes: i32) {
        let sleep = match minutes {
            END_OF_SONG => Some(Sleep::EndOfSong),
            1.. => Some(Sleep::At(
                Instant::now() + Duration::from_mins(u64::from(minutes.unsigned_abs())),
            )),
            _ => None,
        };
        self.state.borrow_mut().sleep = sleep;
        if matches!(sleep, Some(Sleep::EndOfSong)) {
            // The song must end rather than run into the next.
            self.unqueue_next();
        }
        self.show_sleep(if sleep.is_some() { minutes } else { 0 });
    }

    /// Runs every tick: counts the minutes down and pauses at the end.
    pub(in crate::app) fn check_sleep(self: &Rc<Self>) {
        let Some(Sleep::At(at)) = self.state.borrow().sleep else {
            return;
        };
        let left = at.saturating_duration_since(Instant::now());
        if !left.is_zero() {
            self.show_sleep(minutes_left(left));
            return;
        }
        self.state.borrow_mut().sleep = None;
        self.show_sleep(0);
        if self.ui().global::<ApiGlobal>().get_is_playing() {
            self.set_paused(true);
            self.toast(PAUSED);
        }
    }

    /// At a song's end, whether the timer was waiting for it. It then stops,
    /// showing the next song for Play to start.
    pub(in crate::app) fn sleep_at_end(self: &Rc<Self>) -> bool {
        if !matches!(self.state.borrow().sleep, Some(Sleep::EndOfSong)) {
            return false;
        }
        self.state.borrow_mut().sleep = None;
        self.show_sleep(0);
        self.stop();
        let next = self.state.borrow_mut().queue.advance(false).cloned();
        if let Some(track) = next {
            self.state.borrow_mut().playback.duration = track.seconds();
            self.present(&track, 0.0);
            self.show_playing(false);
            // Its stream's quality shows once it plays.
            let ui = self.ui();
            ui.set_np_quality(SharedString::new());
            ui.set_np_premium(false);
            self.report_playback_at(0.0);
        }
        self.toast(PAUSED);
        true
    }

    /// Minutes left, `END_OF_SONG`, or 0 for off; Slint skips an unchanged set.
    fn show_sleep(&self, minutes: i32) {
        self.ui().set_sleep_minutes(minutes);
    }
}

/// Rounded up, so the last minute reads as 1 rather than off.
fn minutes_left(left: Duration) -> i32 {
    i32::try_from(left.as_secs().div_ceil(60).max(1)).unwrap_or(i32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_minutes_up() {
        let secs = Duration::from_secs;
        assert_eq!(minutes_left(Duration::ZERO), 1);
        assert_eq!(minutes_left(secs(60)), 1);
        assert_eq!(minutes_left(secs(61)), 2);
        assert_eq!(minutes_left(secs(90 * 60)), 90);
    }
}
