//! Sleep timer: pauses after some minutes or at the song's end.

use std::{
    rc::Rc,
    time::{Duration, Instant},
};

use slint::SharedString;

use crate::app::App;

/// What the player bar sends for "End of song"; 0 turns the timer off.
const END_OF_SONG: i32 = -1;
const PAUSED: &str = "Paused by the sleep timer";

#[derive(Clone, Copy, PartialEq)]
pub(in crate::app) enum Sleep {
    At(Instant),
    EndOfSong,
}

impl App {
    pub(in crate::app) fn set_sleep(&self, minutes: i32) {
        let sleep = match minutes {
            END_OF_SONG => Some(Sleep::EndOfSong),
            1.. => Some(Sleep::At(
                Instant::now() + Duration::from_mins(minutes as u64),
            )),
            _ => None,
        };
        self.state.borrow_mut().sleep = sleep;
        if sleep == Some(Sleep::EndOfSong) {
            // Let the song end rather than run on.
            self.unqueue_next();
        }
        self.show_sleep(if sleep.is_some() { minutes } else { 0 });
    }

    /// Every tick: shows the minutes left, pausing once due.
    pub(in crate::app) fn check_sleep(self: &Rc<Self>) {
        let Some(Sleep::At(at)) = self.state.borrow().sleep else {
            return;
        };
        let left = at.saturating_duration_since(Instant::now());
        if !left.is_zero() {
            self.show_sleep(minutes_left(left));
            return;
        }
        self.set_sleep(0);
        if !self.player.is_paused() && !self.stopped() {
            self.set_paused(true);
            self.toast(PAUSED);
        }
    }

    /// Whether it stopped at the song's end, showing the next song for Play.
    pub(in crate::app) fn sleep_at_end(self: &Rc<Self>) -> bool {
        if self.state.borrow().sleep != Some(Sleep::EndOfSong) {
            return false;
        }
        self.set_sleep(0);
        self.stop();
        let next = self.state.borrow_mut().queue.advance(false).cloned();
        if let Some(track) = next {
            self.state.borrow_mut().playback.duration = track.seconds();
            self.present(&track, 0.0);
            self.show_playing(false);
            // Unknown until it plays.
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
