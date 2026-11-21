//! The recent searches the search box offers before anything is typed.

use std::{collections::VecDeque, path::PathBuf};

use serde::{Deserialize, Serialize};

use super::{data_dir, read_json, save_json};

/// The last few searches, newest first. Saved as soon as they change, being
/// tiny, and again on exit should that write have failed.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Searches {
    queries: VecDeque<String>,
    #[serde(skip)]
    changed: bool,
}

const SEARCHES_LIMIT: usize = 5;

fn searches_path() -> PathBuf {
    data_dir().join("searches.json")
}

impl Searches {
    pub fn load() -> Self {
        read_json::<Self>(&searches_path())
            .unwrap_or_default()
            .cleaned()
    }

    /// Replays the searches oldest first through `record`, so a file edited
    /// by hand cannot bring in blanks, repeats or more than the limit.
    fn cleaned(self) -> Self {
        let mut searches = Self::default();
        for query in self.queries.iter().rev() {
            searches.record(query);
        }
        searches.changed = false;
        searches
    }

    /// Newest first.
    pub fn queries(&self) -> impl Iterator<Item = &str> {
        self.queries.iter().map(String::as_str)
    }

    /// Puts `query` first, dropping an earlier search for it whatever its
    /// capitals. Returns whether the searches changed.
    pub fn record(&mut self, query: &str) -> bool {
        let query = query.trim();
        if query.is_empty() || self.queries.front().is_some_and(|newest| newest == query) {
            return false;
        }
        self.remove(query);
        self.queries.push_front(query.to_owned());
        self.queries.truncate(SEARCHES_LIMIT);
        self.changed = true;
        true
    }

    /// Drops `query`, whatever its capitals, returning whether it was there.
    pub fn remove(&mut self, query: &str) -> bool {
        let before = self.queries.len();
        let query = query.trim().to_lowercase();
        self.queries.retain(|q| q.to_lowercase() != query);
        let removed = self.queries.len() != before;
        self.changed |= removed;
        removed
    }

    pub fn save(&mut self) {
        if self.changed && save_json(&searches_path(), self, "the recent searches") {
            self.changed = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn searches_keep_the_newest_five_once_each() {
        let mut searches = Searches::default();
        for q in ["a", "b", "c", "d", "e", "f"] {
            searches.record(q);
        }
        assert_eq!(
            searches.queries().collect::<Vec<_>>(),
            ["f", "e", "d", "c", "b"]
        );
        searches.record("  C ");
        assert_eq!(
            searches.queries().collect::<Vec<_>>(),
            ["C", "f", "e", "d", "b"]
        );
        assert!(!searches.record("C"), "searching again changes nothing");
        assert!(!searches.record("   "), "blank searches are not kept");
        assert!(!searches.remove("missing"));
        assert!(searches.remove("f"));
        assert_eq!(searches.queries().collect::<Vec<_>>(), ["C", "e", "d", "b"]);
        let json = serde_json::to_vec(&searches).unwrap();
        let back: Searches = serde_json::from_slice(&json).unwrap();
        assert_eq!(back.queries().collect::<Vec<_>>(), ["C", "e", "d", "b"]);

        let edited: Searches =
            serde_json::from_str(r#"{"queries":["x","  ","X","y ","a","b","c","d"]}"#).unwrap();
        assert_eq!(
            edited.cleaned().queries().collect::<Vec<_>>(),
            ["x", "y", "a", "b", "c"],
            "trimmed, without blanks or repeats, the newest five"
        );
    }
}
