//! Shuffling, which spreads each artist's and album's songs apart, and
//! turning it off again.

use std::collections::{HashMap, HashSet};

use super::{Queue, Spread};

impl<T: Clone + Spread> Queue<T> {
    fn random(&mut self) -> u64 {
        // xorshift64
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.rng
    }

    pub fn random_below(&mut self, n: usize) -> usize {
        (self.random() % n.max(1) as u64) as usize
    }

    /// Uniform in [0, 1).
    fn unit(&mut self) -> f64 {
        (self.random() >> 11) as f64 / (1u64 << 53) as f64
    }

    pub(super) fn shuffle_after_current(&mut self) {
        let start = self.current.map_or(0, |c| c + 1);
        let rest = self.entries.split_off(start);
        let items: Vec<&T> = rest.iter().map(|e| &e.item).collect();
        let order = self.spread(&items, &[T::artist, T::album]);
        let mut rest: Vec<_> = rest.into_iter().map(Some).collect();
        self.entries
            .extend(order.into_iter().filter_map(|i| rest[i].take()));
        // The spread doesn't see the current entry, so its artist may follow it.
        if let Some(current) = self.current {
            let artist = self.entries[current].item.artist();
            if let Some(other) = self.entries[start..]
                .iter()
                .position(|e| e.item.artist() != artist)
            {
                self.entries[start..=start + other].rotate_right(1);
            }
        }
    }

    /// A random order of `items` in which each key's items sit evenly apart
    /// from a random offset, with a little jitter, as a plain shuffle bunches
    /// them by chance; within a key, the next key is spread the same way.
    fn spread(&mut self, items: &[&T], keys: &[fn(&T) -> &str]) -> Vec<usize> {
        let Some((key, keys)) = keys.split_first() else {
            let mut order: Vec<usize> = (0..items.len()).collect();
            for i in (1..order.len()).rev() {
                order.swap(i, self.random_below(i + 1));
            }
            return order;
        };
        // Groups in order of first appearance, as a map's order would make the
        // draws below differ from run to run.
        let mut index = HashMap::new();
        let mut groups: Vec<Vec<usize>> = Vec::new();
        for (i, item) in items.iter().enumerate() {
            let g = *index.entry(key(item)).or_insert_with(|| {
                groups.push(Vec::new());
                groups.len() - 1
            });
            groups[g].push(i);
        }
        let mut placed = Vec::with_capacity(items.len());
        for group in groups {
            let members: Vec<&T> = group.iter().map(|&i| items[i]).collect();
            let order = self.spread(&members, keys);
            let gap = 1.0 / group.len() as f64;
            let offset = self.unit();
            for (n, m) in order.into_iter().enumerate() {
                let jitter = 0.4 * self.unit() - 0.2;
                placed.push(((n as f64 + offset + jitter) * gap, group[m]));
            }
        }
        placed.sort_by(|a, b| a.0.total_cmp(&b.0));
        placed.into_iter().map(|(_, i)| i).collect()
    }

    pub fn set_shuffle(&mut self, on: bool) {
        match (on, self.unshuffled.is_some()) {
            (true, false) => {
                self.revision += 1;
                self.unshuffled = Some(self.entries.iter().map(|e| e.uid).collect());
                self.shuffle_after_current();
            }
            (false, true) => {
                self.revision += 1;
                let order = self.unshuffled.take().unwrap_or_default();
                let current_uid = self.current_uid();
                let rank: HashMap<u64, usize> = order
                    .into_iter()
                    .enumerate()
                    .map(|(i, uid)| (uid, i))
                    .collect();
                // Entries pushed, played next or recommended while shuffled,
                // which the order doesn't know, stay behind the entry they
                // followed, so the one playing never jumps to the end.
                let mut anchor = None;
                let mut keyed: Vec<_> = std::mem::take(&mut self.entries)
                    .into_iter()
                    .enumerate()
                    .map(|(i, e)| {
                        if let Some(&r) = rank.get(&e.uid) {
                            anchor = Some(r);
                        }
                        ((anchor, i), e)
                    })
                    .collect();
                keyed.sort_by_key(|(key, _)| *key);
                self.entries = keyed.into_iter().map(|(_, e)| e).collect();
                self.current = current_uid.and_then(|uid| self.find_uid(uid));
            }
            _ => {}
        }
    }

    /// While shuffled, each item goes in at random among the entries still to
    /// play, and goes last should shuffle be turned off.
    pub fn extend(&mut self, items: impl IntoIterator<Item = T>) {
        // Behind the entries played next or recommended right after the
        // current, which the order doesn't know, so they still play next.
        let start = self.unshuffled.as_ref().map(|order| {
            let known: HashSet<u64> = order.iter().copied().collect();
            let after = self.current.map_or(0, |c| c + 1);
            after
                + self.entries[after..]
                    .iter()
                    .take_while(|e| !known.contains(&e.uid))
                    .count()
        });
        for item in items {
            let entry = self.entry(item);
            let at = match (start, &mut self.unshuffled) {
                (Some(start), Some(order)) => {
                    order.push(entry.uid);
                    // A few draws for a place not beside the same artist, as
                    // spreading again would reorder what is already shown.
                    let mut at = start;
                    for _ in 0..4 {
                        at = start + self.random_below(self.entries.len() - start + 1);
                        if !self.beside(at, entry.item.artist()) {
                            break;
                        }
                    }
                    at
                }
                _ => self.entries.len(),
            };
            self.entries.insert(at, entry);
            self.current.get_or_insert(at);
        }
    }

    /// Whether an item put in at `at` would sit beside one by `artist`.
    fn beside(&self, at: usize, artist: &str) -> bool {
        let same = |i: usize| {
            self.entries
                .get(i)
                .is_some_and(|e| e.item.artist() == artist)
        };
        (at > 0 && same(at - 1)) || same(at)
    }
}
