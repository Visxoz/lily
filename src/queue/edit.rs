//! Edits from the queue panel and the player: songs put in, taken out and
//! moved, the current index following the song playing.

use super::{Queue, Spread};

impl<T: Clone + Spread> Queue<T> {
    /// The uid the next entry put in gets, so two reads bound the entries
    /// put in between them.
    pub fn uid_mark(&self) -> u64 {
        self.next_uid + 1
    }

    /// The current entry's uid, which stays its own as entries move.
    pub fn current_uid(&self) -> Option<u64> {
        self.current.map(|c| self.entries[c].uid)
    }

    pub fn find_uid(&self, uid: u64) -> Option<usize> {
        self.entries.iter().position(|e| e.uid == uid)
    }

    /// The entries put in within `uids`, with Smart Shuffle's put in since
    /// its start.
    pub fn added(&self, uids: &std::ops::Range<u64>) -> Vec<usize> {
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| uids.contains(&e.uid) || (e.recommended && e.uid >= uids.start))
            .map(|(i, _)| i)
            .collect()
    }

    pub fn mark_recommended(&mut self, indices: impl IntoIterator<Item = usize>) {
        for index in indices {
            if let Some(entry) = self.entries.get_mut(index) {
                entry.recommended = true;
            }
        }
    }

    /// Does nothing unless `index` is after the current track.
    pub fn insert_recommended(&mut self, index: usize, item: T) {
        let Some(current) = self.current else { return };
        if index <= current || index > self.entries.len() {
            return;
        }
        let mut entry = self.entry(item);
        entry.recommended = true;
        self.entries.insert(index, entry);
    }

    /// Keeps the one playing and the user's own order. Returns whether any was removed.
    pub fn remove_recommended(&mut self) -> bool {
        let Some(current) = self.current else {
            return false;
        };
        let before = self.entries.len();
        let removed_before = self.entries[..current]
            .iter()
            .filter(|e| e.recommended)
            .count();
        let mut index = 0;
        self.entries.retain(|e| {
            let keep = !e.recommended || index == current;
            index += 1;
            keep
        });
        if self.entries.len() == before {
            return false;
        }
        self.revision += 1;
        self.current = Some(current - removed_before);
        true
    }

    pub fn play_next(&mut self, item: T) {
        let entry = self.entry(item);
        let at = self.current.map_or(self.entries.len(), |c| c + 1);
        self.entries.insert(at, entry);
        if self.current.is_none() {
            self.current = Some(at);
        }
    }

    pub fn push(&mut self, item: T) {
        let entry = self.entry(item);
        self.entries.push(entry);
        if self.current.is_none() {
            self.current = Some(self.entries.len() - 1);
        }
    }

    /// Never removes the current entry. Returns the indices removed, in order.
    pub fn remove_many(&mut self, indices: impl IntoIterator<Item = usize>) -> Vec<usize> {
        let Some(current) = self.current else {
            return Vec::new();
        };
        let mut drop = vec![false; self.entries.len()];
        for index in indices {
            if index != current && index < drop.len() {
                drop[index] = true;
            }
        }
        let removed: Vec<usize> = (0..drop.len()).filter(|&i| drop[i]).collect();
        if removed.is_empty() {
            return removed;
        }
        self.revision += 1;
        let mut index = 0;
        self.entries.retain(|_| {
            let keep = !drop[index];
            index += 1;
            keep
        });
        let before = removed.iter().take_while(|&&i| i < current).count();
        self.current = Some(current - before);
        removed
    }

    /// The current index follows the current track, which may itself move.
    pub fn move_to(&mut self, from: usize, to: usize) -> bool {
        if from == to || from >= self.entries.len() || to >= self.entries.len() {
            return false;
        }
        self.revision += 1;
        if from < to {
            self.entries[from..=to].rotate_left(1);
        } else {
            self.entries[to..=from].rotate_right(1);
        }
        if let Some(current) = self.current {
            self.current = Some(if current == from {
                to
            } else if from < current && current <= to {
                current - 1
            } else if to <= current && current < from {
                current + 1
            } else {
                current
            });
        }
        true
    }

    /// Keeps the entries' queue order. Returns whether anything moved.
    pub fn play_next_many(&mut self, indices: impl IntoIterator<Item = usize>) -> bool {
        let Some(current) = self.current else {
            return false;
        };
        let mut pick = vec![false; self.entries.len()];
        for index in indices {
            if index != current && index < pick.len() {
                pick[index] = true;
            }
        }
        let picked = pick.iter().filter(|&&p| p).count();
        let already_next = pick[..current].iter().all(|&p| !p)
            && pick[current + 1..].iter().take(picked).all(|&p| p);
        if picked == 0 || already_next {
            return false;
        }
        self.revision += 1;
        let mut kept = Vec::with_capacity(self.entries.len());
        let mut moved = Vec::with_capacity(picked);
        let mut after = Vec::with_capacity(self.entries.len() - current - 1);
        for (index, entry) in std::mem::take(&mut self.entries).into_iter().enumerate() {
            if pick[index] {
                moved.push(entry);
            } else if index <= current {
                kept.push(entry);
            } else {
                after.push(entry);
            }
        }
        self.current = Some(kept.len() - 1);
        kept.append(&mut moved);
        kept.append(&mut after);
        self.entries = kept;
        true
    }

    /// Puts `item` in place of every entry `matches` picks, keeping their
    /// places, so the revision stays. Returns the indices replaced.
    pub fn replace(&mut self, matches: impl Fn(&T) -> bool, item: &T) -> Vec<usize> {
        let mut replaced = Vec::new();
        for (index, entry) in self.entries.iter_mut().enumerate() {
            if matches(&entry.item) {
                entry.item = item.clone();
                replaced.push(index);
            }
        }
        replaced
    }

    pub fn clear_upcoming(&mut self) {
        if let Some(current) = self.current {
            self.revision += 1;
            self.entries.truncate(current + 1);
        }
    }
}
