//! The state `App` keeps behind its `RefCell`: the page and its history, the
//! queue, and the playing track's progress, which every part of the app reads.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::Instant,
};

use super::{account, now_playing, playback, playback::song_version::SongVersion, session};
use crate::{
    api::{self, Card, Page, Route, Track},
    downloads::Downloads,
    queue::Queue,
    settings::{self, Settings},
    streams::ResolvedStream,
};

pub(super) struct HistoryEntry {
    pub(super) route: Route,
    pub(super) scroll: f32,
}

#[derive(Default)]
pub(super) struct Playback {
    pub(super) generation: u64,
    pub(super) started: bool,
    pub(super) duration: f64,
    pub(super) failures: u32,
    pub(super) prefetched: HashMap<String, (ResolvedStream, Instant)>,
    pub(super) prefetching: HashSet<String>,
    pub(super) extending: bool,
    /// When the current track was asked for, to time the first sound.
    pub(super) requested_at: Option<Instant>,
    pub(super) current_premium: bool,
    /// The current track's stream and when it was resolved, for a replay to
    /// take from the player's memory (`App::held_stream`).
    pub(super) playing: Option<(String, ResolvedStream, Instant)>,
    /// A track whose Premium stream failed and plays the free one instead.
    pub(super) force_free: Option<String>,
    /// Where a track is to start: restored from the last run, sought to
    /// while stopped, or where a Premium retry or dropped connection left
    /// off. Kept until it sounds, so a load that fails starts there again.
    pub(super) resume: Option<(String, f64)>,
    /// The generation whose play is already recorded: a free-stream retry
    /// carrying on a song, so its start is not a second play.
    pub(super) play_recorded: Option<u64>,
    pub(super) listening: session::Listening,
    /// The next track, queued in the player for gapless playback, and when
    /// its stream was resolved.
    pub(super) queued: Option<(String, ResolvedStream, Instant)>,
    /// The player could not open the queued stream, so `unqueue` drops it. Only
    /// a flag: the event may be late, and a track queued since just resolves again.
    pub(super) queued_failed: bool,
    /// The track last tried for queueing, so a failure is not retried
    /// every tick; it then loads as usual when the current one ends.
    pub(super) queue_tried: Option<String>,
    /// A song whose connection dropped, and the tries at opening it again.
    pub(super) reopen: Option<(String, u32)>,
}

impl Playback {
    /// Forgets the track queued in the player, so the next one is queued
    /// afresh. The player's own copy is dropped with `Player::clear_next`.
    pub(super) fn forget_queued(&mut self) {
        self.queued = None;
        self.queued_failed = false;
        self.queue_tried = None;
    }

    /// `forget_queued`, but keeps the stream as prefetched so it isn't asked for
    /// again. Not for a quality change or a Premium retry, which must drop it.
    pub(super) fn unqueue(&mut self) {
        if let Some((id, stream, at)) = self.queued.take()
            && !self.queued_failed
        {
            self.prefetched.insert(id, (stream, at));
        }
        self.forget_queued();
    }

    /// Makes `id` the playing stream, keeping the one it replaces as
    /// prefetched, so Previous plays it without asking YouTube again.
    pub(super) fn set_playing(&mut self, id: String, stream: ResolvedStream, at: Instant) {
        if let Some((old, mut old_stream, old_at)) = self.playing.replace((id, stream, at))
            && self.playing.as_ref().is_some_and(|(id, ..)| *id != old)
            // Its URL may have been refused past the first megabyte.
            && !old_stream.client.is_some_and(crate::streams::is_cut_off)
        {
            // Its first bytes stay only with the newest prefetch.
            old_stream.head = Default::default();
            self.prefetched.insert(old, (old_stream, old_at));
        }
    }

    pub(super) fn has_prefetched(&self, id: &str) -> bool {
        self.prefetched
            .get(id)
            .is_some_and(|(_, at)| at.elapsed() < playback::PREFETCH_FRESH)
    }

    pub(super) fn take_prefetched(&mut self, id: &str) -> Option<(ResolvedStream, Instant)> {
        self.prefetched
            .remove(id)
            .filter(|(_, at)| at.elapsed() < playback::PREFETCH_FRESH)
    }

    /// Counts a try at opening `id` again after its connection dropped,
    /// where the sound got to should it have started, and returns the tries.
    pub(super) fn reopen_try(&mut self, id: &str, position: f64) -> u32 {
        if self.started {
            self.resume = Some((id.to_owned(), position));
            self.play_recorded = Some(self.generation);
        }
        let (_, tries) = match &mut self.reopen {
            Some(reopen) if reopen.0 == id => reopen,
            slot => slot.insert((id.to_owned(), 0)),
        };
        *tries += 1;
        *tries
    }

    pub(super) fn reopening(&self, id: &str) -> bool {
        self.reopen.as_ref().is_some_and(|(r, _)| r == id)
    }

    /// Keeps the stream for `id`, dropping stale ones such as tracks skipped past.
    /// Only the newest keeps its first bytes; an older one's warm-up still
    /// running fills a cell nothing else holds.
    pub(super) fn store_prefetched(&mut self, id: String, stream: ResolvedStream) {
        self.prefetched
            .retain(|_, (_, at)| at.elapsed() < playback::PREFETCH_FRESH);
        for (old, _) in self.prefetched.values_mut() {
            old.head = Default::default();
        }
        self.prefetched.insert(id, (stream, Instant::now()));
    }
}

pub(super) struct State {
    pub(super) settings: Settings,
    pub(super) signed_in: bool,
    /// Bumped on every sign-in and sign-out, so a personal feed fetched for
    /// one account is never cached or saved under the next.
    pub(super) account: u64,
    pub(super) route: Route,
    pub(super) back: Vec<HistoryEntry>,
    pub(super) forward: Vec<HistoryEntry>,
    pub(super) page: Page,
    pub(super) page_generation: u64,
    /// Bumped by each edit to a playlist's page, by its route, so a refresh
    /// fetched from before one doesn't put back what it changed.
    pub(super) page_edits: HashMap<Route, u64>,
    pub(super) restore_scroll: Option<f32>,
    pub(super) queue: Queue<Track>,
    /// Bumped as the queue is replaced or cleared, so tracks fetched for one
    /// never join the next.
    pub(super) queue_generation: u64,
    pub(super) playback: Playback,
    pub(super) lyrics: now_playing::lyrics::Lyrics,
    pub(super) suggest_generation: u64,
    pub(super) page_cache: HashMap<Route, (Page, Instant)>,
    /// Music videos looked up for a song version, by video ID.
    pub(super) song_versions: HashMap<String, SongVersion>,
    pub(super) history: settings::History,
    pub(super) searches: settings::Searches,
    pub(super) smart: playback::smart_shuffle::Smart,
    /// Bumped as the playing song changes or is rated, so a rating fetched
    /// before then is not shown.
    pub(super) rating_generation: u64,
    /// The song being saved to a playlist, and the playlists offered.
    pub(super) saving: Option<(String, Vec<api::PlaylistOption>)>,
    pub(super) artwork_accent: Option<slint::Color>,
    pub(super) scrobbler: account::scrobble::Scrobbler,
    pub(super) downloads: Downloads,
    /// Songs waiting to download; the first is downloading while `downloading`.
    pub(super) download_queue: VecDeque<Track>,
    pub(super) downloading: bool,
    /// The sidebar's playlists in YouTube's order; `list_library` puts the pinned first.
    pub(super) library: Vec<Card>,
    /// IDs whose save or subscribe is still being answered, so a second click
    /// is ignored.
    pub(super) library_edits: HashSet<String>,
    /// Bumped by each save or subscribe shown, so a page fetched before one
    /// keeps what it set.
    pub(super) library_revision: u64,
    /// Bumped by each `load_library`, so only the latest answer is shown.
    pub(super) library_generation: u64,
    pub(super) sleep: Option<playback::sleep::Sleep>,
}

impl State {
    /// Replaces the queue, dropping Smart Shuffle's batch for the old one.
    pub(super) fn set_queue(&mut self, tracks: Vec<Track>, start: usize, shuffle: bool) {
        self.playback.failures = 0;
        // Chosen afresh, a song that dropped starts over.
        if self.playback.reopen.take().is_some() {
            self.playback.resume = None;
        }
        self.queue.set(tracks, start, shuffle);
        self.queue_generation += 1;
        self.smart.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dropped_song_reopens_where_it_first_stopped() {
        let mut playback = Playback {
            started: true,
            ..Playback::default()
        };
        assert_eq!(playback.reopen_try("a", 42.0), 1);
        assert_eq!(playback.play_recorded, Some(playback.generation));
        playback.started = false;
        playback.generation += 1;
        assert_eq!(playback.reopen_try("a", 0.0), 2, "the reopen failed too");
        assert_eq!(
            playback.resume,
            Some(("a".into(), 42.0)),
            "not where the stopped player is"
        );
        assert!(playback.reopening("a"));

        assert_eq!(playback.reopen_try("b", 9.0), 1, "another song starts over");
        assert!(!playback.reopening("a"));
        assert_eq!(
            playback.resume,
            Some(("a".into(), 42.0)),
            "b never sounded, so has no place to carry on from"
        );
    }

    #[test]
    fn the_song_moved_on_from_stays_for_previous() {
        let mut playback = Playback::default();
        let stream = ResolvedStream::default();
        playback.set_playing("a".into(), stream.clone(), Instant::now());
        playback.set_playing("a".into(), stream.clone(), Instant::now());
        assert!(!playback.has_prefetched("a"), "a replay sets none aside");
        playback.set_playing("b".into(), stream, Instant::now());
        assert!(playback.has_prefetched("a"));
        assert!(!playback.has_prefetched("b"));
    }
}
