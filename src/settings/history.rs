//! The songs played, which the History page shows signed out and Smart
//! Shuffle reads.

use std::{
    collections::{HashSet, VecDeque},
    path::PathBuf,
};

use serde::{Deserialize, Serialize};

use super::{HISTORY_SAVES, data_dir, read_json};

/// The songs played, newest first, one entry per song. Kept in memory until
/// `save`, so a song's start never waits on the disk.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct History {
    tracks: VecDeque<crate::api::Track>,
    /// Songs whose latest play was too short to count.
    #[serde(default, skip_serializing_if = "HashSet::is_empty")]
    skipped: HashSet<String>,
    /// Bumped with each play, and how far a save is known to have got: a
    /// copy handed to a background save counts only once it is written, so
    /// the save on exit still writes plays that one never did.
    #[serde(skip)]
    revision: u64,
    #[serde(skip)]
    saved: u64,
}

const HISTORY_LIMIT: usize = 500;

fn history_path() -> PathBuf {
    data_dir().join("history.json")
}

impl History {
    pub fn load() -> Self {
        read_json(&history_path()).unwrap_or_default()
    }

    /// Newest first.
    pub fn tracks(&self) -> impl Iterator<Item = &crate::api::Track> {
        self.tracks.iter()
    }

    /// Those heard long enough to count, newest first.
    pub fn heard(&self) -> impl Iterator<Item = &crate::api::Track> {
        self.tracks.iter().filter(|t| !self.skipped.contains(&t.id))
    }

    /// Puts `track` first, dropping an earlier play of it; it is left out of
    /// `heard` until `mark_heard`.
    pub fn record(&mut self, track: &crate::api::Track) {
        self.tracks.retain(|t| t.id != track.id);
        self.skipped.insert(track.id.clone());
        self.tracks.push_front(track.clone());
        if self.tracks.len() > HISTORY_LIMIT
            && let Some(dropped) = self.tracks.pop_back()
        {
            self.skipped.remove(&dropped.id);
        }
        self.revision += 1;
    }

    pub fn mark_heard(&mut self, id: &str) {
        if self.skipped.remove(id) {
            self.revision += 1;
        }
    }

    /// A copy to save and the revision it holds, if there are plays not
    /// yet known to be saved.
    pub fn unsaved(&self) -> Option<(Self, u64)> {
        (self.revision != self.saved).then(|| {
            let copy = Self {
                tracks: self.tracks.clone(),
                skipped: self.skipped.clone(),
                ..Self::default()
            };
            (copy, self.revision)
        })
    }

    /// Records that a copy from `unsaved` was written.
    pub fn saved_up_to(&mut self, revision: u64) {
        self.saved = self.saved.max(revision);
    }

    /// Returns whether it was saved.
    pub fn save(&self, ticket: u64) -> bool {
        HISTORY_SAVES.save(ticket, &history_path(), self, "the history")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_stays_unsaved_until_its_latest_play_is_written() {
        let track = |id: &str| crate::api::Track {
            id: id.into(),
            ..Default::default()
        };
        let mut history = History::default();
        assert!(history.unsaved().is_none());
        history.record(&track("a"));
        let (_, first) = history.unsaved().unwrap();
        // A play arriving while that copy is being written.
        history.record(&track("b"));
        history.saved_up_to(first);
        let (copy, second) = history.unsaved().unwrap();
        assert_eq!(copy.tracks().count(), 2);
        history.saved_up_to(second);
        history.saved_up_to(first);
        assert!(history.unsaved().is_none());
    }

    #[test]
    fn only_songs_heard_long_enough_reach_listen_again() {
        let track = |id: &str| crate::api::Track {
            id: id.into(),
            ..Default::default()
        };
        let mut history = History::default();
        history.record(&track("a"));
        history.record(&track("b"));
        history.mark_heard("a");
        let heard = |h: &History| h.heard().map(|t| t.id.clone()).collect::<Vec<_>>();
        assert_eq!(heard(&history), ["a"]);
        assert_eq!(history.tracks().count(), 2, "both are in the history");
        history.record(&track("a"));
        assert!(heard(&history).is_empty(), "a skipped replay");
    }
}
