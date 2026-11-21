//! The queue and position kept from one run to the next.

use std::{ops::Range, path::PathBuf};

use serde::{Deserialize, Serialize};

use super::{SESSION_SAVES, data_dir, read_json};

/// The queue and position at exit, restored paused on the next start.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Session {
    pub queue: Vec<crate::api::Track>,
    pub index: usize,
    pub position: f64,
    /// Length of the current track, which feed cards do not carry.
    #[serde(default)]
    pub duration: f64,
    /// Queue entries Smart Shuffle put in.
    #[serde(default)]
    pub recommended: Vec<usize>,
}

const SESSION_QUEUE_LIMIT: usize = 500;

fn session_path() -> PathBuf {
    data_dir().join("session.json")
}

impl Session {
    pub fn load() -> Option<Self> {
        read_json::<Self>(&session_path()).filter(|s| s.index < s.queue.len())
    }

    /// The part of a queue `len` long that is kept: all of it, or the
    /// window around the track at `index` if it is long.
    pub fn window(len: usize, index: usize) -> Range<usize> {
        if len <= SESSION_QUEUE_LIMIT {
            return 0..len;
        }
        let start = index.saturating_sub(SESSION_QUEUE_LIMIT / 2);
        start..(start + SESSION_QUEUE_LIMIT).min(len)
    }

    pub fn save(&self, ticket: u64) {
        SESSION_SAVES.save(ticket, &session_path(), self, "the session");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_keeps_short_queues_whole_and_long_ones_around_the_track() {
        assert_eq!(Session::window(400, 350), 0..400);
        assert_eq!(Session::window(1000, 100), 0..500);
        assert_eq!(Session::window(1000, 600), 350..850);
        assert_eq!(Session::window(1000, 990), 740..1000);
    }
}
