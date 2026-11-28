//! Blocking artists from the menus, which leaves their songs out of what
//! plays unless chosen, and unblocking them from Settings; likewise the
//! artists the AI music list names.

use std::{rc::Rc, time::Duration};

use slint::{ModelRc, SharedString, TimerMode, VecModel};

use super::pins::find_card;
use crate::api::{ArtistRef, CardKind};
use crate::app::{App, browse::actions::find_song};
use crate::settings::Blocked;

/// The AI music list is asked for again this long after a failure, doubling
/// for each in a row up to a day.
const AI_ARTISTS_RETRY: Duration = Duration::from_secs(15 * 60);

impl App {
    /// The artist a menu blocks: a song's first, an artist card's, pinned
    /// ones too, or the artist page's from `"header"`. Song rows go by
    /// `track_artist`.
    pub(in crate::app) fn artist_of(&self, kind: &str, id: &str) -> Option<ArtistRef> {
        let state = self.state.borrow();
        match kind {
            "header" => state.page.header.as_ref().and_then(|h| {
                Some(ArtistRef {
                    id: Some(h.artist_id.clone()?),
                    name: h.title.clone(),
                })
            }),
            "artist" => find_card(&state, CardKind::Artist, id).map(|c| ArtistRef {
                id: Some(c.id.clone()),
                name: c.title.clone(),
            }),
            "song" => find_song(&state, id).and_then(|t| t.artists.first().cloned()),
            _ => None,
        }
    }

    /// The menu entry for the artist a "block" acts on, or none.
    pub(in crate::app) fn block_label(&self, artist: Option<ArtistRef>) -> SharedString {
        let Some(artist) = artist else {
            return SharedString::new();
        };
        let blocked = self
            .state
            .borrow()
            .settings
            .blocked_artists
            .matches(&artist);
        let verb = if blocked { "Unblock" } else { "Block" };
        format!("{verb} {}", artist.name).into()
    }

    pub(in crate::app) fn toggle_block(self: &Rc<Self>, artist: Option<ArtistRef>) {
        let Some(artist) = artist else { return };
        let mut blocked = false;
        self.update_settings(|s| blocked = s.blocked_artists.toggle(artist.clone()));
        self.show_blocked();
        if blocked {
            self.toast(format!("Blocked {}", artist.name));
            self.leave_out_blocked(artist);
        } else {
            self.toast(format!("Unblocked {}", artist.name));
        }
    }

    /// From Settings, without a toast, as the row going says as much.
    pub(in crate::app) fn unblock_at(&self, index: usize) {
        let artist = (self.state.borrow().settings.blocked_artists.artists())
            .get(index)
            .cloned();
        if let Some(artist) = artist {
            self.update_settings(|s| {
                s.blocked_artists.toggle(artist);
            });
            self.show_blocked();
        }
    }

    pub(in crate::app) fn show_blocked(&self) {
        let names: Vec<SharedString> = (self.state.borrow().settings.blocked_artists.artists())
            .iter()
            .map(|a| a.name.as_str().into())
            .collect();
        self.ui()
            .set_blocked_artists(ModelRc::new(VecModel::from(names)));
    }

    /// From Settings: on once its popup is confirmed, off at once.
    pub(in crate::app) fn set_block_ai(self: &Rc<Self>, on: bool) {
        self.update_settings(|s| {
            s.block_ai = on;
            if !on {
                s.blocked_artists.set_listed(None);
            }
        });
        self.ai_artists_failures.set(0);
        if on {
            self.load_ai_artists(true);
        } else {
            self.ai_artists_timer.stop();
            if let Some(load) = self.ai_artists_load.take() {
                load.abort();
            }
        }
    }

    /// Leaves out the AI music list's artists while the setting is on, and
    /// arms its next fetch. `asked`, from the switch, takes the list's songs
    /// out of the queue and toasts a failure, as the user is waiting.
    pub(in crate::app) fn load_ai_artists(self: &Rc<Self>, asked: bool) {
        let held = {
            let settings = &self.state.borrow().settings;
            if !settings.block_ai {
                return;
            }
            settings.blocked_artists.is_listed()
        };
        let load = self.spawn(crate::ai_artists::load(held), move |app, listed| {
            // An abort misses a load already done whose `done` waits its turn.
            if !app.state.borrow().settings.block_ai {
                return;
            }
            let again = match listed {
                Ok((listed, due)) => {
                    // First, so Smart Shuffle's top-up after the purge honours it.
                    (app.state.borrow_mut().settings.blocked_artists)
                        .set_listed(Some(listed.clone()));
                    // Only from the switch, by the list alone; refreshes leave the queue as it is.
                    if asked {
                        let mut only = Blocked::default();
                        only.set_listed(Some(listed));
                        app.leave_out_of_queue(&only);
                    }
                    app.ai_artists_failures.set(0);
                    due
                }
                Err(e) => {
                    log::warn!("{e}");
                    if asked {
                        app.toast("Couldn't get the AI music list. It will be tried again.");
                    }
                    let failures = app.ai_artists_failures.get();
                    app.ai_artists_failures.set(failures + 1);
                    (AI_ARTISTS_RETRY * 2u32.pow(failures.min(7))).min(crate::ai_artists::FRESH_FOR)
                }
            };
            let weak = Rc::downgrade(app);
            app.ai_artists_timer
                .start(TimerMode::SingleShot, again, move || {
                    if let Some(app) = weak.upgrade() {
                        app.load_ai_artists(false);
                    }
                });
        });
        if let Some(old) = self.ai_artists_load.replace(Some(load)) {
            old.abort();
        }
    }

    /// Takes `blocked`'s songs out of the queue, never the current one.
    fn leave_out_of_queue(self: &Rc<Self>, blocked: &Blocked) {
        let rows = {
            let queue = &self.state.borrow().queue;
            let current = queue.current_index();
            (queue.items().enumerate())
                .filter(|&(i, t)| Some(i) != current && blocked.has(t))
                .map(|(i, _)| i)
                .collect()
        };
        self.queue_remove(rows);
    }

    /// Takes the artist just blocked out of the queue and skips their song
    /// playing, but not one paused, which a skip would play. Only this artist.
    fn leave_out_blocked(self: &Rc<Self>, artist: ArtistRef) {
        let blocked: Blocked = [artist].into_iter().collect();
        // First, so the skip lands on a song kept.
        self.leave_out_of_queue(&blocked);
        let playing = {
            let queue = &self.state.borrow().queue;
            (queue.current())
                .filter(|t| blocked.has(t))
                .zip(queue.current_uid())
                .map(|(t, uid)| (t.id.clone(), uid))
        };
        let Some((id, uid)) = playing else { return };
        if self.player.is_paused() || self.stopped() {
            // So its autoplay radio doesn't follow it; a later one leaves the artist out.
            self.drop_autoplay_radio(&id);
            return;
        }
        self.skip_past(&id);
        // Its entry, skipped past; stopped on it, `prefetch_next` takes it out
        // once the skip carries on.
        let row = self.state.borrow().queue.find_uid(uid);
        self.queue_remove(row.into_iter().collect());
    }
}
