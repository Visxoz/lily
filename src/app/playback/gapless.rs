use std::{rc::Rc, time::Instant};

use crate::app::{App, State, playback::sleep::Sleep};
use crate::{api::Track, audio::Transition, settings::Settings};

/// Time for the next stream to start downloading and its decoder to open. A
/// crossfade or Automix adds what the player holds back for it.
const GAPLESS_LEAD_SECS: f64 = 15.0;

/// Whether `next` follows `current` on its album, which is often written to
/// run straight on, so it is never faded into.
fn in_album_order(current: Option<&Track>, next: &Track) -> bool {
    current.is_some_and(|c| match (&c.album, &next.album) {
        (Some((a, _)), Some((b, _))) if a == b => match (c.track_nr, next.track_nr) {
            (Some(a), Some(b)) => a.checked_add(1) == Some(b),
            _ => true,
        },
        _ => false,
    })
}

/// How the next track comes in, read again as its stream arrives, so a
/// toggle meanwhile counts.
fn next_transition(state: &State) -> Transition {
    match state.queue.peek_next() {
        Some(next) if !in_album_order(state.queue.current(), next) => state.settings.transition(),
        _ => Transition::Gapless,
    }
}

impl App {
    pub(in crate::app) fn change_transition(&self, change: impl FnOnce(&mut Settings)) {
        self.update_settings(change);
        // A track queued with the old setting is queued again.
        self.unqueue_next();
    }

    /// Drops the track queued in the player, keeping its stream as prefetched.
    pub(in crate::app) fn unqueue_next(&self) {
        self.state.borrow_mut().playback.unqueue();
        self.player.clear_next();
    }

    /// Runs every tick, so it copies nothing until a track is due.
    pub(in crate::app) fn update_gapless(self: &Rc<Self>, position: f64) {
        let (stale, due, generation) = {
            let state = self.state.borrow();
            let playback = &state.playback;
            let next = state.queue.peek_next();
            let transition = next_transition(&state);
            // Opening takes clock time; what is held is in song time.
            let lead = GAPLESS_LEAD_SECS * f64::from(state.settings.speed) + transition.hold_secs();
            let next = next.map(|t| t.id.as_str());
            let stale = playback
                .queued
                .as_ref()
                .is_some_and(|(id, ..)| next != Some(id.as_str()));
            let near_end = playback.started
                && state.sleep != Some(Sleep::EndOfSong)
                && playback.duration > 0.0
                && playback.duration - position <= lead
                && (playback.queued.is_none() || stale);
            // One still prefetching is taken from `prefetched` once it lands.
            let due = next
                .filter(|id| {
                    near_end
                        && playback.queue_tried.as_deref() != Some(*id)
                        && !playback.prefetching.contains(*id)
                })
                .map(str::to_owned);
            (stale, due, playback.generation)
        };
        if stale {
            self.unqueue_next();
        }
        let Some(next_id) = due else {
            return;
        };
        // On repeat-one, the player's download plays again.
        let held = self.held_stream(&next_id);
        let prefetched = {
            let playback = &mut self.state.borrow_mut().playback;
            playback.queue_tried = Some(next_id.clone());
            held.or_else(|| playback.take_prefetched(&next_id))
        };
        // Kept with the stream, so one put back stays only as fresh as it was.
        let resolved_at = prefetched.as_ref().map_or_else(Instant::now, |(_, at)| *at);
        let resolve = self.resolve_future(next_id.clone(), prefetched.map(|(stream, _)| stream));
        self.spawn(resolve, move |app, result| {
            let (current, still_next) = {
                let state = app.state.borrow();
                let current = state.playback.generation == generation;
                let next = current
                    && state.sleep != Some(Sleep::EndOfSong)
                    && state.queue.peek_next().is_some_and(|t| t.id == next_id);
                (current, next)
            };
            match result {
                Ok(stream) if still_next => {
                    let transition = next_transition(&app.state.borrow());
                    app.player
                        .queue_next(app.stream_spec(&next_id, stream.clone(), 0.0), transition);
                    let playback = &mut app.state.borrow_mut().playback;
                    playback.queued = Some((next_id, stream, resolved_at));
                    playback.queued_failed = false;
                }
                // Kept, so Play after the sleep timer's stop doesn't ask again.
                Ok(stream) if current => {
                    let playback = &mut app.state.borrow_mut().playback;
                    playback.prefetched.insert(next_id, (stream, resolved_at));
                }
                Ok(_) => {}
                Err(e) => log::debug!("could not queue the next track: {e}"),
            }
        });
    }

    /// The queued track became audible: move the queue on and show it,
    /// without loading anything.
    pub(in crate::app) fn advance_gapless(self: &Rc<Self>) {
        let Some((id, stream, resolved_at)) = self.state.borrow_mut().playback.queued.take() else {
            return;
        };
        let current = self.state.borrow_mut().queue.advance(false).cloned();
        let Some(track) = current.filter(|t| t.id == id) else {
            // The queue changed under the player; play what it says now.
            self.start_current();
            return;
        };
        {
            let mut state = self.state.borrow_mut();
            let playback = &mut state.playback;
            playback.generation += 1;
            playback.started = true;
            playback.failures = 0;
            playback.duration = track.seconds();
            playback.requested_at = None;
            playback.queue_tried = None;
            playback.resume = None;
            playback.reopen = None;
        }
        self.present(&track, 0.0);
        self.record_play();
        self.show_stream(&stream);
        (self.state.borrow_mut().playback).set_playing(id, stream, resolved_at);
        self.ui().set_np_loading(false);
        self.update_media_state();
        self.extend_with_radio_if_ending();
        self.prefetch_next();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(album: Option<&str>, track_nr: Option<u16>) -> Track {
        Track {
            album: album.map(|id| (id.into(), "Album".into())),
            track_nr,
            ..Default::default()
        }
    }

    #[test]
    fn runs_an_album_on_without_fading() {
        let first = track(Some("a"), Some(1));
        assert!(in_album_order(Some(&first), &track(Some("a"), Some(2))));
        assert!(
            in_album_order(Some(&first), &track(Some("a"), None)),
            "numbers unknown, assumed in order"
        );
    }

    #[test]
    fn fades_between_anything_else() {
        let first = track(Some("a"), Some(1));
        assert!(
            !in_album_order(Some(&first), &track(Some("a"), Some(5))),
            "shuffled"
        );
        assert!(!in_album_order(Some(&first), &track(Some("b"), Some(2))));
        assert!(!in_album_order(
            Some(&track(None, None)),
            &track(None, None)
        ));
        assert!(!in_album_order(None, &first));
        assert!(!in_album_order(
            Some(&track(Some("a"), Some(u16::MAX))),
            &track(Some("a"), Some(0))
        ));
    }
}
