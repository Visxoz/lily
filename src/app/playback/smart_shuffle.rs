//! Smart Shuffle mixes recommendations, from the radio of one of the queue's
//! tracks, in after every few of the user's own, marked so they can be taken out.

use std::{
    collections::{HashSet, VecDeque},
    rc::Rc,
};

use crate::app::{App, State};
use crate::{api::Track, queue::Queue, settings::Blocked};

/// The user's own tracks between two recommendations.
const GAP_MIN: usize = 3;
const GAP_MAX: usize = 4;
/// How many of the user's own tracks ahead get their recommendations; the
/// rest follow as the queue runs, so a long playlist never asks for all.
const LOOKAHEAD: usize = 8;
/// Recently played songs that are not recommended again.
const RECENT: usize = 100;

#[derive(Default)]
pub(in crate::app) struct Smart {
    /// Bumped when the queue is replaced or Smart Shuffle turned off, so a
    /// batch asked for an earlier queue is dropped as it arrives.
    generation: u64,
    fetching: bool,
    /// Recommendations fetched and not yet placed.
    pool: VecDeque<Track>,
    /// Own tracks to pass before the next recommendation; zero picks anew.
    gap: usize,
    /// IDs in the queue or recently played, rebuilt when the queue changed
    /// under it, so each check is a lookup.
    known: HashSet<String>,
    known_revision: Option<u64>,
}

impl Smart {
    pub(in crate::app) fn reset(&mut self) {
        self.generation += 1;
        self.fetching = false;
        self.pool.clear();
        self.gap = 0;
    }

    /// Checks `blocked` as each is placed, so one blocked since its batch
    /// arrived is left out too.
    fn fresh(&mut self, blocked: &Blocked) -> Option<Track> {
        while let Some(track) = self.pool.pop_front() {
            if !self.known.contains(&track.id) && !blocked.has(&track) {
                return Some(track);
            }
        }
        None
    }

    fn fill_known(&mut self, queue: &Queue<Track>, recent: impl Iterator<Item = String>) {
        if self.known_revision == Some(queue.revision()) {
            return;
        }
        self.known = queue.items().map(|t| t.id.clone()).chain(recent).collect();
        self.known_revision = Some(queue.revision());
    }
}

/// One of the user's own tracks from YouTube, as local files have no radio,
/// and never a blocked artist's, whose radio would be mostly theirs.
fn seed(queue: &mut Queue<Track>, blocked: &Blocked) -> Option<String> {
    let seeds = |(t, r): &(&Track, bool)| !r && !t.is_local() && !blocked.has(t);
    let own = queue.entries().filter(seeds).count();
    let pick = queue.random_below(own);
    queue
        .entries()
        .filter(seeds)
        .nth(pick)
        .map(|(t, _)| t.id.clone())
}

impl App {
    /// Puts recommendations among the upcoming tracks where they are due,
    /// and asks for a batch when the pool runs dry.
    pub(in crate::app) fn smart_top_up(self: &Rc<Self>) {
        let (before, inserted, fetch) = {
            let mut state = self.state.borrow_mut();
            if !state.settings.smart_shuffle {
                return;
            }
            let State {
                queue,
                smart,
                history,
                settings,
                ..
            } = &mut *state;
            let blocked = &settings.blocked_artists;
            let Some(current) = queue.current_index() else {
                return;
            };
            if smart.fetching {
                return;
            }
            smart.fill_known(queue, history.tracks().take(RECENT).map(|t| t.id.clone()));
            let mut run = usize::from(!queue.is_recommended(current));
            let mut ahead = 0;
            let mut index = current + 1;
            let before = queue.revision();
            // Ascending, each counting the ones before it, so the panel can
            // put them into its rows in this order.
            let mut inserted = Vec::new();
            let mut dry = false;
            // Only before one of the user's own tracks, never after the last:
            // a cleared queue stays clear, and autoplay takes over at the end.
            while index < queue.len() && ahead < LOOKAHEAD {
                if queue.is_recommended(index) {
                    run = 0;
                    index += 1;
                    continue;
                }
                if smart.gap == 0 {
                    smart.gap = GAP_MIN + queue.random_below(GAP_MAX - GAP_MIN + 1);
                }
                if run >= smart.gap {
                    let Some(track) = smart.fresh(blocked) else {
                        dry = true;
                        break;
                    };
                    smart.known.insert(track.id.clone());
                    queue.insert_recommended(index, track);
                    inserted.push(index);
                    smart.gap = 0;
                    run = 0;
                    index += 1;
                    continue;
                }
                run += 1;
                ahead += 1;
                index += 1;
            }
            smart.known_revision = Some(queue.revision());
            let fetch = dry.then(|| seed(queue, blocked)).flatten().map(|seed| {
                smart.fetching = true;
                (seed, smart.generation)
            });
            (before, inserted, fetch)
        };
        if !inserted.is_empty() {
            self.queue_recommended(before, &inserted);
        }
        if let Some((seed, generation)) = fetch {
            self.smart_fetch(seed, generation);
        }
    }

    fn smart_fetch(self: &Rc<Self>, seed: String, generation: u64) {
        let api = self.api.clone();
        self.spawn(
            async move { api.track_radio(&seed).await },
            move |app, result| {
                let fresh = {
                    let mut state = app.state.borrow_mut();
                    let State {
                        smart, settings, ..
                    } = &mut *state;
                    if smart.generation != generation {
                        return;
                    }
                    smart.fetching = false;
                    match result {
                        // A batch of blocked songs alone brings nothing new.
                        Ok(tracks) => {
                            let (known, blocked) = (&smart.known, &settings.blocked_artists);
                            smart.pool.extend(
                                tracks
                                    .into_iter()
                                    .filter(|t| !known.contains(&t.id) && !blocked.has(t)),
                            );
                            !smart.pool.is_empty()
                        }
                        Err(e) => {
                            log::debug!("smart shuffle could not fetch recommendations: {e}");
                            false
                        }
                    }
                };
                // Nothing new means nothing to place; the next track start
                // asks again from another seed.
                if fresh {
                    app.prefetch_next();
                }
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ArtistRef;

    fn song(id: &str) -> Track {
        Track {
            id: id.into(),
            artists: vec![ArtistRef {
                id: Some(format!("UC{id}")),
                name: format!("Artist {id}"),
            }],
            ..Default::default()
        }
    }

    fn blocking(ids: impl IntoIterator<Item = String>) -> Blocked {
        ids.into_iter()
            .map(|id| ArtistRef {
                id: Some(format!("UC{id}")),
                name: format!("Artist {id}"),
            })
            .collect()
    }

    #[test]
    fn leaves_blocked_artists_out_of_seeds_and_picks() {
        let mut queue = Queue::with_seed(7);
        queue.set((0..20).map(|i| song(&i.to_string())).collect(), 0, false);
        let blocked = blocking((0..20).filter(|&i| i != 3).map(|i| i.to_string()));
        for _ in 0..10 {
            assert_eq!(seed(&mut queue, &blocked).as_deref(), Some("3"));
        }
        assert_eq!(
            seed(&mut queue, &blocking((0..20).map(|i| i.to_string()))),
            None
        );

        let mut smart = Smart::default();
        smart.pool.extend([song("x"), song("y")]);
        let blocked = blocking(["x".to_owned()]);
        assert_eq!(smart.fresh(&blocked).map(|t| t.id).as_deref(), Some("y"));
    }
}
