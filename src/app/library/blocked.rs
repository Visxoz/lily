//! Blocking artists from the menus, which leaves their songs out of what
//! plays unless chosen, and unblocking them from Settings.

use std::rc::Rc;

use slint::{ModelRc, SharedString, VecModel};

use super::pins::find_card;
use crate::api::{ArtistRef, CardKind};
use crate::app::{App, browse::actions::find_song};
use crate::settings::Blocked;

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

    /// From Settings, without a toast, which would show behind it; the row
    /// going says as much.
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

    /// Takes the artist just blocked out of the queue and skips their song
    /// playing, but not one paused, which a skip would play. Only this artist.
    fn leave_out_blocked(self: &Rc<Self>, artist: ArtistRef) {
        let blocked: Blocked = [artist].into_iter().collect();
        let (rows, playing) = {
            let state = self.state.borrow();
            let queue = &state.queue;
            let current = queue.current_index();
            let rows = (queue.items().enumerate())
                .filter(|&(i, t)| Some(i) != current && blocked.has(t))
                .map(|(i, _)| i)
                .collect();
            let playing = (queue.current())
                .filter(|t| blocked.has(t))
                .zip(queue.current_uid())
                .map(|(t, uid)| (t.id.clone(), uid));
            (rows, playing)
        };
        // First, so the skip lands on a song kept.
        self.queue_remove(rows);
        let Some((id, uid)) = playing else { return };
        if self.player.is_paused() || self.stopped() {
            // So its autoplay radio doesn't follow it; a later one leaves the artist out.
            self.drop_autoplay_radio(&id);
            return;
        }
        self.skip_past(&id);
        // Its entry, skipped past; a no-op when stopped on it, as the current one stays.
        let row = self.state.borrow().queue.find_uid(uid);
        self.queue_remove(row.into_iter().collect());
    }
}
