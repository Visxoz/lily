use std::{collections::HashSet, rc::Rc};

use crate::api::{self, ArtistRef, CardKind, Page, QueueMode, Route, Section, Track};
use crate::app::{App, State, library::pins::shown_cards};

pub(in crate::app) fn card_kind(kind: &str) -> Option<CardKind> {
    match kind {
        "album" => Some(CardKind::Album),
        "playlist" => Some(CardKind::Playlist),
        "artist" => Some(CardKind::Artist),
        "song" => Some(CardKind::Song),
        _ => None,
    }
}

/// A song by its ID: from the queue, the page's rows and cards, or the pins.
pub(in crate::app) fn find_song<'a>(state: &'a State, id: &str) -> Option<&'a Track> {
    let rows = || {
        state.page.sections.iter().find_map(|s| match s {
            Section::Tracks { tracks, .. } => tracks.iter().find(|t| t.id == id),
            Section::Cards { .. } => None,
        })
    };
    let cards = || shown_cards(state).find_map(|c| c.track.as_ref().filter(|t| t.id == id));
    (state.queue.items().find(|t| t.id == id))
        .or_else(rows)
        .or_else(cards)
}

fn row_track(state: &State, section: usize, index: usize) -> Option<&Track> {
    match state.page.sections.get(section)? {
        Section::Tracks { tracks, .. } => tracks.get(index),
        Section::Cards { .. } => None,
    }
}

impl App {
    fn section_track(&self, section: usize, index: usize) -> Option<Track> {
        row_track(&self.state.borrow(), section, index).cloned()
    }

    /// The artist a song row's "block" acts on.
    pub(in crate::app) fn track_artist(&self, section: usize, index: usize) -> Option<ArtistRef> {
        row_track(&self.state.borrow(), section, index)?
            .artists
            .first()
            .cloned()
    }

    pub(in crate::app) fn play_from_section(self: &Rc<Self>, section: usize, index: usize) {
        // Only a section played whole is copied; a radio seed is one track.
        let (track, whole) = {
            let state = self.state.borrow();
            let Some(Section::Tracks { tracks, mode, .. }) = state.page.sections.get(section)
            else {
                return;
            };
            let Some(track) = tracks.get(index) else {
                return;
            };
            match mode {
                QueueMode::Section => (track.clone(), Some(tracks.clone())),
                QueueMode::Radio => (track.clone(), None),
            }
        };
        match whole {
            Some(tracks) => self.play_queue(tracks, Some(index)),
            None => self.play_with_radio(track),
        }
    }

    fn play_with_radio(self: &Rc<Self>, track: Track) {
        let seed = track.id.clone();
        let (queue_generation, radio_generation) = {
            let mut state = self.state.borrow_mut();
            // Shuffled, if on, so the radio is shuffled in as it arrives.
            let shuffle = state.settings.shuffle;
            state.set_queue(vec![track], 0, shuffle);
            // Autoplay would otherwise fetch the same radio once this starts.
            state.playback.extending = true;
            // So a radio still on its way for the last queue can't clear it.
            state.playback.radio_generation += 1;
            (state.queue_generation, state.playback.radio_generation)
        };
        self.start_current();
        let api = self.api.clone();
        self.spawn(
            async move { api.track_radio(&seed).await },
            move |app, result| {
                // Dropped with its seed, and `extending` cleared then.
                if app.state.borrow().playback.radio_generation != radio_generation {
                    return;
                }
                app.state.borrow_mut().playback.extending = false;
                match result {
                    Ok(tracks) => {
                        if app.state.borrow().queue_generation == queue_generation {
                            app.append_unique(tracks);
                        }
                    }
                    Err(e) => {
                        log::warn!("radio failed: {e}");
                        app.extend_with_radio_if_ending();
                    }
                }
            },
        );
    }

    /// For radios, which may repeat what is queued.
    pub(in crate::app) fn append_unique(self: &Rc<Self>, tracks: Vec<Track>) {
        let fresh: Vec<Track> = {
            let state = self.state.borrow();
            let known: HashSet<&str> = state.queue.items().map(|t| t.id.as_str()).collect();
            tracks
                .into_iter()
                .filter(|t| !known.contains(t.id.as_str()))
                .collect()
        };
        self.append(fresh);
    }

    /// Leaves out blocked artists' songs, as none of these was chosen.
    fn append(self: &Rc<Self>, mut tracks: Vec<Track>) {
        (self.state.borrow().settings.blocked_artists).leave_out(&mut tracks, None);
        if tracks.is_empty() {
            return;
        }
        self.state.borrow_mut().queue.extend(tracks);
        self.refresh_queue();
        // Shuffled in, one may now be next.
        self.drop_stale_next();
        self.prefetch_next();
    }

    /// A Shuffle button turns shuffle on; Play leaves it, and Smart Shuffle,
    /// as they are.
    fn play_tracks(self: &Rc<Self>, tracks: Vec<Track>, shuffle: bool) {
        if tracks.is_empty() {
            self.toast("Nothing to play here");
            return;
        }
        // Saving the settings writes to disk, so only for a change.
        if shuffle && !self.state.borrow().settings.shuffle {
            self.update_settings(|s| s.set_shuffle(true));
        }
        self.play_queue(tracks, None);
    }

    /// A `start` of `None` starts from a random track if shuffling, else the
    /// first. The song at `start` was chosen, so it plays though its artist is
    /// blocked; the rest leave blocked artists out.
    fn play_queue(self: &Rc<Self>, mut tracks: Vec<Track>, start: Option<usize>) {
        let start = (self.state.borrow().settings.blocked_artists).leave_out(&mut tracks, start);
        if tracks.is_empty() {
            self.toast("Every song here is by an artist you've blocked");
            return;
        }
        {
            let mut state = self.state.borrow_mut();
            let shuffle = state.settings.shuffle;
            let start = match start {
                Some(start) => start,
                None if shuffle => state.queue.random_below(tracks.len()),
                None => 0,
            };
            state.set_queue(tracks, start, shuffle);
        }
        self.start_current();
    }

    fn play_radio(self: &Rc<Self>, radio_id: String) {
        let api = self.api.clone();
        self.spawn(
            async move { api.radio(&radio_id).await },
            |app, result| match result {
                Ok(tracks) => app.play_tracks(tracks, false),
                Err(e) => app.toast(format!("Couldn't start the radio: {e}")),
            },
        );
    }

    pub(in crate::app) fn header_action(self: &Rc<Self>, action: &str) {
        if action == "copy-link" {
            let link = self.state.borrow().route.link();
            return self.copy_link(link);
        }
        if action == "save" || action == "subscribe" {
            return self.toggle_in_library(action == "subscribe");
        }
        if let "rename" | "delete" | "public" | "unlisted" | "private" = action {
            return self.edit_playlist(action);
        }
        if action == "block" {
            return self.toggle_block(self.artist_of("header", ""));
        }
        if action == "pin" {
            return self.toggle_pin("header", "");
        }
        let (tracks, shuffle_playlist, unloaded, radio_id) = {
            let state = self.state.borrow();
            let Some(header) = &state.page.header else {
                return;
            };
            // A long playlist whose rest is still loading.
            let unloaded = match (&state.route, &state.page.more) {
                (Route::Playlist(id), Some(_)) => Some(id.clone()),
                _ => None,
            };
            (
                state.page.header_tracks().to_vec(),
                header.shuffle_playlist.clone(),
                unloaded,
                header.radio_id.clone(),
            )
        };
        match action {
            // Played from what has loaded, with the rest queued as it arrives.
            "play" | "shuffle" if unloaded.is_some() => {
                if action == "shuffle" && !self.state.borrow().settings.shuffle {
                    self.update_settings(|s| s.set_shuffle(true));
                }
                let (route, page) = {
                    let state = self.state.borrow();
                    (state.route.clone(), state.page.clone())
                };
                self.play_page(route, page, false);
            }
            "play" => self.play_tracks(tracks, false),
            "shuffle" => self.with_all_tracks(tracks, shuffle_playlist, |app, tracks| {
                app.play_tracks(tracks, true);
            }),
            "download" => {
                self.with_all_tracks(tracks, unloaded, |app, tracks| app.download(tracks))
            }
            "radio" => {
                if let Some(radio) = radio_id {
                    self.play_radio(radio);
                }
            }
            _ => {}
        }
    }

    /// A menu action on a song, album, playlist or artist, by its kind and ID.
    pub(in crate::app) fn card_action(self: &Rc<Self>, action: &str, kind: &str, id: &str) {
        match action {
            "copy-link" => self.copy_link(api::link(kind, id)),
            "pin" => self.toggle_pin(kind, id),
            "block" => self.toggle_block(self.artist_of(kind, id)),
            _ => {}
        }
    }

    fn copy_link(self: &Rc<Self>, link: Option<String>) {
        let Some(link) = link else { return };
        match clipboard_win::set_clipboard_string(&link) {
            Ok(()) => self.toast("Link copied"),
            Err(e) => self.toast(format!("Couldn't copy the link: {e}")),
        }
    }

    /// Runs `then` with the page's tracks, or the whole playlist when the
    /// page shows only part of it.
    fn with_all_tracks(
        self: &Rc<Self>,
        shown: Vec<Track>,
        playlist: Option<String>,
        then: impl FnOnce(&Rc<Self>, Vec<Track>) + 'static,
    ) {
        let Some(playlist) = playlist else {
            then(self, shown);
            return;
        };
        let api = self.api.clone();
        self.spawn(
            async move { api.playlist_tracks(&playlist).await },
            move |app, result| match result {
                Ok(found) if !found.is_empty() => then(app, found),
                _ => then(app, shown),
            },
        );
    }

    fn song_card(&self, id: &str) -> Option<Track> {
        find_song(&self.state.borrow(), id).cloned()
    }

    pub(in crate::app) fn play_song_card(self: &Rc<Self>, id: &str) {
        if let Some(track) = self.song_card(id) {
            self.play_with_radio(track);
        }
    }

    pub(in crate::app) fn play_card(self: &Rc<Self>, kind: &str, id: String, radio: bool) {
        if kind == "song" {
            self.play_song_card(&id);
            return;
        }
        let Some(kind) = card_kind(kind) else {
            return;
        };
        if radio && kind == CardKind::Playlist {
            self.play_radio(format!("RDAMPL{id}"));
            return;
        }
        let route = match kind {
            CardKind::Album => Route::Album(id),
            CardKind::Playlist => Route::Playlist(id),
            _ => {
                let api = self.api.clone();
                self.spawn(
                    async move { api.card_tracks(kind, &id).await },
                    move |app, result| match result {
                        Ok(tracks) => app.play_card_tracks(tracks, radio),
                        Err(e) => app.toast(format!("Couldn't load: {e}")),
                    },
                );
                return;
            }
        };
        // An expired copy may be missing songs added since.
        if let Some((page, true, _)) = self.cached_page(&route) {
            self.play_page(route, page, radio);
            return;
        }
        let api = self.api.clone();
        let (signed_in, account) = {
            let state = self.state.borrow();
            (state.signed_in, state.account)
        };
        let asked = route.clone();
        self.spawn(
            async move { api.page(&asked, signed_in).await },
            move |app, result| match result {
                Ok(page) => {
                    // So opening it next shows it at once.
                    if app.state.borrow().account == account {
                        app.cache_page(route.clone(), page.clone());
                    }
                    app.play_page(route, page, radio);
                }
                Err(e) => app.toast(format!("Couldn't load: {e}")),
            },
        );
    }

    /// Plays `tracks`, or with `radio` the first one's radio.
    fn play_card_tracks(self: &Rc<Self>, tracks: Vec<Track>, radio: bool) {
        if !radio {
            self.play_tracks(tracks, false);
            return;
        }
        match tracks.into_iter().next() {
            Some(first) => self.play_with_radio(first),
            None => self.toast("Nothing to play here"),
        }
    }

    /// Plays an album or playlist page's tracks, or its first one's radio, at
    /// once, and queues the rest of a long playlist as it arrives.
    fn play_page(self: &Rc<Self>, route: Route, mut page: Page, radio: bool) {
        let more = page.more.is_some() && !radio;
        let tracks = match page.sections.first_mut() {
            // Taken rather than copied when the page is done with.
            Some(Section::Tracks { tracks, .. }) if !more => std::mem::take(tracks),
            Some(Section::Tracks { tracks, .. }) => tracks.clone(),
            Some(Section::Cards { .. }) | None => Vec::new(),
        };
        let shown = tracks.len();
        // A first page by blocked artists alone starts once the rest is in.
        let waits = more && shown > 0 && {
            let state = self.state.borrow();
            tracks.iter().all(|t| state.settings.blocked_artists.has(t))
        };
        if !waits {
            self.play_card_tracks(tracks, radio);
        }
        // Read once the queue is set, so it is this queue's.
        let (queue_generation, account) = {
            let state = self.state.borrow();
            (state.queue_generation, state.account)
        };
        if !more || shown == 0 {
            return;
        }
        let api = self.api.clone();
        self.spawn(
            async move { api.complete(page).await },
            move |app, (mut page, whole)| {
                let keep = whole && app.state.borrow().account == account;
                if keep && let Some((cached, _)) = app.state.borrow_mut().page_cache.get_mut(&route)
                {
                    super::page::keep_library_state(&mut page, Some(cached));
                    *cached = page.clone();
                }
                if app.state.borrow().queue_generation != queue_generation {
                    return;
                }
                let Some(Section::Tracks { tracks, .. }) = page.sections.first_mut() else {
                    return;
                };
                if waits {
                    // Every song blocked, it says so only once the whole list is in.
                    let gave_up = !whole && {
                        let state = app.state.borrow();
                        tracks.iter().all(|t| state.settings.blocked_artists.has(t))
                    };
                    if gave_up {
                        app.toast("Couldn't load the rest of this playlist");
                        return;
                    }
                    app.play_tracks(std::mem::take(tracks), false);
                } else if tracks.len() > shown {
                    // Not deduplicated, as a playlist may list a song twice.
                    app.append(tracks.split_off(shown));
                }
            },
        );
    }

    pub(in crate::app) fn download_card(self: &Rc<Self>, kind: &str, id: String) {
        if kind == "song" {
            if let Some(track) = self.song_card(&id) {
                self.download(vec![track]);
            }
            return;
        }
        let Some(kind) = card_kind(kind) else {
            return;
        };
        let api = self.api.clone();
        self.spawn(
            async move { api.card_tracks(kind, &id).await },
            |app, result| match result {
                Ok(tracks) => app.download(tracks),
                Err(e) => app.toast(format!("Couldn't load: {e}")),
            },
        );
    }

    /// A song in the queue, from its row or the player bar.
    pub(in crate::app) fn download_queued(self: &Rc<Self>, id: &str, remove: bool) {
        if remove {
            self.remove_download(id);
            return;
        }
        let track = self
            .state
            .borrow()
            .queue
            .items()
            .find(|t| t.id == id)
            .cloned();
        if let Some(track) = track {
            self.download(vec![track]);
        }
    }

    pub(in crate::app) fn track_action(
        self: &Rc<Self>,
        action: &str,
        section: usize,
        index: usize,
    ) {
        let Some(track) = self.section_track(section, index) else {
            return;
        };
        match action {
            "next" | "queue" => {
                let empty = self.state.borrow().queue.is_empty();
                let at = {
                    let mut state = self.state.borrow_mut();
                    if action == "next" {
                        state.queue.play_next(track.clone());
                        state.queue.current_index().map_or(0, |c| c + 1)
                    } else {
                        state.queue.push(track.clone());
                        state.queue.len() - 1
                    }
                };
                if empty {
                    self.start_current();
                } else {
                    self.queue_inserted(at);
                    self.toast(if action == "next" {
                        format!("\u{201c}{}\u{201d} plays next", track.title)
                    } else {
                        format!("Added \u{201c}{}\u{201d} to the queue", track.title)
                    });
                }
            }
            "radio" => self.play_with_radio(track),
            "download" => self.download(vec![track]),
            "remove-download" => self.remove_download(&track.id),
            "artist" => {
                if let Some(id) = track.first_artist_id() {
                    self.navigate(Route::Artist(id.to_owned()), true);
                }
            }
            "album" => {
                if let Some(id) = track.album_id() {
                    self.navigate(Route::Album(id.to_owned()), true);
                }
            }
            "copy-link" if !track.is_local() => self.copy_link(api::link("song", &track.id)),
            "remove-from-playlist" => self.remove_from_playlist(section, track),
            "block" => self.toggle_block(track.artists.into_iter().next()),
            _ => {}
        }
    }
}
