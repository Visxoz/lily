//! Shuffle reorders only what follows the current track, as YouTube Music
//! does, and turning it off restores the original order.

use std::time::{SystemTime, UNIX_EPOCH};

mod edit;
mod shuffle;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Repeat {
    #[default]
    Off,
    All,
    One,
}

impl Repeat {
    pub fn next(self) -> Self {
        match self {
            Repeat::Off => Repeat::All,
            Repeat::All => Repeat::One,
            Repeat::One => Repeat::Off,
        }
    }

    pub fn index(self) -> i32 {
        self as i32
    }

    pub fn from_index(i: i32) -> Self {
        match i {
            1 => Repeat::All,
            2 => Repeat::One,
            _ => Repeat::Off,
        }
    }
}

/// What shuffle spreads apart, so one artist's or album's songs don't bunch.
pub trait Spread {
    fn artist(&self) -> &str;
    fn album(&self) -> &str;
}

#[derive(Debug, Clone)]
struct Entry<T> {
    uid: u64,
    item: T,
    /// Mixed in by Smart Shuffle rather than chosen by the user.
    recommended: bool,
}

#[derive(Debug, Clone)]
pub struct Queue<T> {
    entries: Vec<Entry<T>>,
    current: Option<usize>,
    /// Entry order before shuffling, by uid.
    unshuffled: Option<Vec<u64>>,
    pub repeat: Repeat,
    next_uid: u64,
    rng: u64,
    /// Bumped whenever entries are added, removed or reordered.
    revision: u64,
}

impl<T: Clone + Spread> Default for Queue<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Clone + Spread> Queue<T> {
    pub fn new() -> Self {
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0x9E37_79B9, |d| d.as_nanos() as u64);
        Self::with_seed(seed)
    }

    pub fn with_seed(seed: u64) -> Self {
        Self {
            entries: Vec::new(),
            current: None,
            unshuffled: None,
            repeat: Repeat::Off,
            next_uid: 0,
            rng: seed | 1,
            revision: 0,
        }
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    fn entry(&mut self, item: T) -> Entry<T> {
        self.revision += 1;
        self.next_uid += 1;
        Entry {
            uid: self.next_uid,
            item,
            recommended: false,
        }
    }

    pub fn set(&mut self, items: Vec<T>, start: usize, shuffle: bool) {
        self.revision += 1;
        self.entries = items.into_iter().map(|item| self.entry(item)).collect();
        self.current = (!self.entries.is_empty()).then(|| start.min(self.entries.len() - 1));
        self.unshuffled = shuffle.then(|| self.entries.iter().map(|e| e.uid).collect());
        if shuffle {
            // Nothing has played yet, so those before the start are shuffled in too.
            if let Some(start) = self.current {
                self.entries[..=start].rotate_right(1);
                self.current = Some(0);
            }
            self.shuffle_after_current();
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn current_index(&self) -> Option<usize> {
        self.current
    }

    pub fn current(&self) -> Option<&T> {
        self.current.map(|c| &self.entries[c].item)
    }

    pub fn items(&self) -> impl Iterator<Item = &T> {
        self.entries.iter().map(|e| &e.item)
    }

    /// Every item, with whether Smart Shuffle put it there.
    pub fn entries(&self) -> impl Iterator<Item = (&T, bool)> {
        self.entries.iter().map(|e| (&e.item, e.recommended))
    }

    pub fn get(&self, index: usize) -> Option<&T> {
        self.entries.get(index).map(|e| &e.item)
    }

    pub fn is_recommended(&self, index: usize) -> bool {
        self.entries.get(index).is_some_and(|e| e.recommended)
    }

    pub fn recommended(&self) -> impl Iterator<Item = usize> {
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| e.recommended.then_some(i))
    }

    pub fn peek_next(&self) -> Option<&T> {
        self.next_index(false).map(|i| &self.entries[i].item)
    }

    /// Where the user's Next goes, past a repeat of the current one.
    pub fn peek_skip(&self) -> Option<&T> {
        self.next_index(true).map(|i| &self.entries[i].item)
    }

    fn next_index(&self, user: bool) -> Option<usize> {
        let current = self.current?;
        if self.repeat == Repeat::One && !user {
            return Some(current);
        }
        if current + 1 < self.entries.len() {
            Some(current + 1)
        } else if self.repeat != Repeat::Off {
            Some(0)
        } else {
            None
        }
    }

    /// `user` skips ignore repeat-one. Returns `None` at the end of the queue.
    pub fn advance(&mut self, user: bool) -> Option<&T> {
        let next = self.next_index(user)?;
        self.current = Some(next);
        self.current()
    }

    pub fn back(&mut self) -> Option<&T> {
        let current = self.current?;
        let prev = if current > 0 {
            current - 1
        } else if self.repeat == Repeat::All {
            self.entries.len() - 1
        } else {
            0
        };
        self.current = Some(prev);
        self.current()
    }

    pub fn jump(&mut self, index: usize) -> Option<&T> {
        if index >= self.entries.len() {
            return None;
        }
        self.current = Some(index);
        self.current()
    }
}

#[cfg(test)]
mod tests;
