//! Signing in to YouTube Music and keeping the session, the sidebar's
//! playlists and pins, and scrobbling to Last.fm and ListenBrainz.

pub(super) mod scrobble;
pub(super) mod spotify;

use std::{cell::Cell, rc::Rc, time::Duration};

use slint::{ComponentHandle, Image, Model, SharedString, Timer, TimerMode};

use super::{App, browse::items::card_item, now_playing::lyrics::PassCheck};
use crate::{
    Api as ApiGlobal, CardItem,
    api::{Card, Route},
    google_sign_in::{self, SignIn, Site},
    images, settings,
};

/// The least time between cookie checks, after one that didn't find the
/// cookie rejected.
const SIGN_IN_RECHECK_AFTER: Duration = Duration::from_secs(10 * 60);

impl App {
    pub(super) fn sign_in(self: &Rc<Self>, cookie: String) {
        if cookie.trim().is_empty() {
            self.ui()
                .set_sign_in_error("Paste your cookies first.".into());
            return;
        }
        let ui = self.ui();
        ui.set_signing_in(true);
        ui.set_sign_in_error(SharedString::new());
        let api = self.api.clone();
        self.spawn(
            async move { api.sign_in(&cookie).await },
            move |app, result| {
                let ui = app.ui();
                ui.set_signing_in(false);
                match result {
                    Ok(()) => {
                        {
                            let mut state = app.state.borrow_mut();
                            state.signed_in = true;
                            state.account += 1;
                            state.page_cache.clear();
                        }
                        // It may be another account's.
                        settings::remove_account_data();
                        app.load_library();
                        app.load_account_photo();
                        app.premium.reset();
                        app.update_settings(|s| {
                            s.premium_account = false;
                            s.no_premium = false;
                            s.account_photo = None;
                        });
                        app.warm_premium();
                        ui.global::<ApiGlobal>().set_signed_in(true);
                        ui.set_sign_in_open(false);
                        if let Some(id) = app.playing_id() {
                            app.load_rating(id);
                        }
                        app.toast("Signed in");
                        app.show_fresh(Route::Library);
                    }
                    Err(e) => ui.set_sign_in_error(format!("Sign-in failed: {e}").into()),
                }
            },
        );
    }

    pub(super) fn sign_in_with_google(self: &Rc<Self>) {
        self.open_sign_in(&google_sign_in::GOOGLE);
    }

    /// One sign-in window at a time, as both share WebView2's folder.
    pub(super) fn open_sign_in(self: &Rc<Self>, site: &'static Site) {
        if let Some(open) = self.sign_in_window.borrow().as_ref()
            && std::ptr::eq(open.site(), site)
        {
            let _ = open.window().show();
            return;
        }
        self.close_sign_in();
        self.stop_lyrics_pass();
        // Each step is deferred: WebView2 calls back from inside its own
        // events, and the window must not be dropped from its own handler.
        let weak = Rc::downgrade(self);
        let on_page = move || {
            let weak = weak.clone();
            Timer::single_shot(Duration::ZERO, move || {
                if let Some(app) = weak.upgrade() {
                    app.check_sign_in_window();
                }
            });
        };
        let open = match SignIn::open(site) {
            Ok(open) => open,
            Err(e) => return self.sign_in_failed(site, &e),
        };
        let weak = Rc::downgrade(self);
        open.window().on_resized(move || {
            if let Some(app) = weak.upgrade()
                && let Some(open) = app.sign_in_window.borrow().as_ref()
            {
                open.fit();
            }
        });
        let weak = Rc::downgrade(self);
        open.window().window().on_close_requested(move || {
            let weak = weak.clone();
            Timer::single_shot(Duration::ZERO, move || {
                if let Some(app) = weak.upgrade() {
                    app.close_sign_in();
                }
            });
            slint::CloseRequestResponse::HideWindow
        });
        *self.sign_in_window.borrow_mut() = Some(open);
        let ui = self.ui();
        if site.is_google() {
            ui.set_sign_in_error(SharedString::new());
            ui.set_signing_in_browser(true);
        } else {
            ui.set_spotify_signing_in(true);
        }

        // Windows creates the window a moment after it is shown. The page is
        // attached with the window out of its cell, as WebView2 pumps
        // messages meanwhile, which may call back into the app.
        let weak = Rc::downgrade(self);
        let attempts = Cell::new(0);
        self.sign_in_timer
            .start(TimerMode::Repeated, Duration::from_millis(50), move || {
                let Some(app) = weak.upgrade() else { return };
                // The lyrics check's WebView2 shares the folder.
                if matches!(*app.lyrics_pass.borrow(), Some(PassCheck::Starting)) {
                    return;
                }
                let Some(mut open) = app.sign_in_window.borrow_mut().take() else {
                    app.sign_in_timer.stop();
                    return;
                };
                let result = open.attach(on_page.clone());
                let ui = app.ui();
                // Closed while attaching, so it goes rather than being kept hidden.
                if !ui.get_signing_in_browser() && !ui.get_spotify_signing_in() {
                    app.sign_in_timer.stop();
                    return;
                }
                *app.sign_in_window.borrow_mut() = Some(open);
                attempts.set(attempts.get() + 1);
                match result {
                    Ok(false) if attempts.get() < 100 => return,
                    Ok(false) => app.sign_in_failed(site, "the sign-in window did not open"),
                    Ok(true) => {}
                    Err(e) => app.sign_in_failed(site, &e),
                }
                app.sign_in_timer.stop();
            });
    }

    fn sign_in_failed(self: &Rc<Self>, site: &'static Site, error: &str) {
        log::warn!("{error}");
        self.close_sign_in();
        if site.is_google() {
            self.ui().set_sign_in_error(
                format!("Couldn't sign in with Google: {error}. You can paste cookies instead.")
                    .into(),
            );
        } else {
            self.toast(format!("Couldn't sign in to Spotify: {error}"));
        }
    }

    fn check_sign_in_window(self: &Rc<Self>) {
        let session = self
            .sign_in_window
            .borrow()
            .as_ref()
            .and_then(|open| Some((open.site().is_google(), open.session()?)));
        let Some((google, cookie)) = session else {
            return;
        };
        self.close_sign_in();
        if google {
            self.sign_in(cookie);
        } else {
            self.spotify_signed_in(cookie);
        }
    }

    pub(super) fn close_sign_in(&self) {
        if let Some(open) = self.sign_in_window.borrow_mut().take() {
            let _ = open.window().hide();
        }
        let ui = self.ui();
        ui.set_signing_in_browser(false);
        ui.set_spotify_signing_in(false);
    }

    pub(super) fn load_library(self: &Rc<Self>) {
        let api = self.api.clone();
        // Signing in or out meanwhile makes the playlists another account's.
        let account = self.state.borrow().account;
        let generation = {
            let mut state = self.state.borrow_mut();
            state.library_generation += 1;
            state.library_generation
        };
        self.spawn(
            async move { api.library_playlists().await },
            move |app, result| match result {
                Ok(cards)
                    if app.state.borrow().account == account
                        && app.state.borrow().library_generation == generation =>
                {
                    app.show_library(cards.clone());
                    app.spawn(async move { settings::save_library(&cards) }, |_, ()| {});
                }
                Ok(_) => {}
                Err(e) => log::warn!("{e}"),
            },
        );
    }

    /// The sidebar's playlists, as saved by the last run or freshly loaded,
    /// in YouTube's order.
    pub(super) fn show_library(self: &Rc<Self>, cards: Vec<Card>) {
        self.state.borrow_mut().library = cards;
        self.list_library();
    }

    /// Lists the playlists kept, the pinned ones first.
    pub(super) fn list_library(self: &Rc<Self>) {
        let (items, art): (Vec<CardItem>, Vec<_>) = {
            let state = self.state.borrow();
            let pinned = &state.settings.pinned;
            pinned_first(&state.library, pinned)
                .into_iter()
                .map(|card| {
                    let item = CardItem {
                        pinned: pinned.contains(&card.id),
                        ..card_item(card, images::ROW)
                    };
                    // Art the cache already held is in the item.
                    let art =
                        (item.art.size().width == 0).then(|| (card.thumb.clone(), item.id.clone()));
                    (item, art)
                })
                .unzip()
        };
        self.library.set_vec(items);
        for (i, art) in art.into_iter().enumerate() {
            let Some((thumb, id)) = art else { continue };
            // The saved list is replaced by the fresh one while its art loads.
            self.load_image(thumb, images::ROW, move |app, image| {
                if let Some(mut item) = app.library.row_data(i)
                    && item.id == id
                {
                    item.art = image;
                    app.library.set_row_data(i, item);
                }
            });
        }
    }

    /// Waits for YouTube to answer a signed-in request as signed out, then
    /// checks the cookie, as the session can end at any time.
    pub(super) fn watch_sign_in(self: &Rc<Self>) {
        let api = self.api.clone();
        self.spawn(async move { api.sign_in_lost().await }, |app, ()| {
            app.check_sign_in();
        });
    }

    fn check_sign_in(self: &Rc<Self>) {
        let (signed_in, account) = {
            let state = self.state.borrow();
            (state.signed_in, state.account)
        };
        // An answer to a request sent before the cookie was removed.
        if !signed_in {
            return self.watch_sign_in();
        }
        let api = self.api.clone();
        self.spawn(
            async move {
                // A stale cookie may only need renewing, which the first
                // check of a run can beat.
                match api.cookie_valid().await {
                    Some(false) => {
                        if let Err(e) = api.rotate_cookies().await {
                            log::warn!("could not renew the sign-in: {e}");
                        }
                        api.cookie_valid().await
                    }
                    valid => valid,
                }
            },
            move |app, valid| {
                // Signing in or out meanwhile makes the answer another account's.
                if app.state.borrow().account != account {
                    return app.watch_sign_in();
                }
                if valid == Some(false) {
                    return app.sign_in_expired();
                }
                // Otherwise the cookie isn't checked again at every answer.
                let weak = Rc::downgrade(app);
                Timer::single_shot(SIGN_IN_RECHECK_AFTER, move || {
                    if let Some(app) = weak.upgrade() {
                        app.watch_sign_in();
                    }
                });
            },
        );
    }

    /// Removes the dead cookie too, so the next start doesn't sign in with it.
    fn sign_in_expired(self: &Rc<Self>) {
        log::warn!("the stored sign-in was rejected");
        let api = self.api.clone();
        let account = self.state.borrow().account;
        self.spawn(async move { api.sign_out().await }, move |app, ()| {
            // The user signed out themselves meanwhile.
            if app.state.borrow().account != account {
                return app.watch_sign_in();
            }
            app.show_signed_out();
            app.toast("YouTube signed you out");
            let reload = matches!(
                app.state.borrow().route,
                Route::Home | Route::Library | Route::History
            );
            if reload {
                app.reload();
            }
            app.watch_sign_in();
        });
    }

    fn show_signed_out(&self) {
        {
            let mut state = self.state.borrow_mut();
            state.signed_in = false;
            state.account += 1;
            state.page_cache.clear();
            state.library.clear();
        }
        settings::remove_account_data();
        self.library.set_vec(Vec::new());
        let ui = self.ui();
        ui.global::<ApiGlobal>().set_signed_in(false);
        self.offer_edits();
        ui.set_account_photo(Image::default());
        self.premium.reset();
        self.update_settings(|s| {
            s.premium_account = false;
            s.no_premium = false;
            s.account_photo = None;
        });
    }

    pub(super) fn load_account_photo(self: &Rc<Self>) {
        let api = self.api.clone();
        // Signing in or out meanwhile makes the photo another account's.
        let account = self.state.borrow().account;
        self.spawn(
            async move { api.account_photo().await },
            move |app, result| match result {
                Ok(Some(url)) if app.state.borrow().account == account => {
                    // Kept for the next start, written only as it changes.
                    if app.state.borrow().settings.account_photo.as_ref() != Some(&url) {
                        app.update_settings(|s| s.account_photo = Some(url.clone()));
                    }
                    app.show_account_photo(url);
                }
                Ok(_) => {}
                Err(e) => log::warn!("could not load the account's picture: {e}"),
            },
        );
    }

    pub(super) fn show_account_photo(self: &Rc<Self>, url: String) {
        // Only while it is still the account's latest picture; signing in or
        // out clears it, and the saved one may load after a fresh one.
        let shown = url.clone();
        self.load_image(Some(url), images::AVATAR, move |app, image| {
            if app.state.borrow().settings.account_photo.as_ref() == Some(&shown) {
                app.ui().set_account_photo(image);
            }
        });
    }

    /// Keeps the session from going stale or being revoked.
    pub(super) fn rotate_cookies(&self) {
        let api = self.api.clone();
        self.rt.spawn(async move {
            if let Err(e) = api.rotate_cookies().await {
                log::warn!("could not renew the sign-in: {e}");
            }
        });
    }

    pub(super) fn sign_out(self: &Rc<Self>) {
        let api = self.api.clone();
        self.spawn(async move { api.sign_out().await }, |app, ()| {
            app.show_signed_out();
            app.ui().set_settings_open(false);
            app.toast("Signed out");
            app.show_fresh(Route::Home);
        });
    }
}

/// `cards` with the `pinned` ones first, in the order they were pinned, then
/// the rest in theirs. A pinned ID not among them is passed over.
fn pinned_first<'a>(cards: &'a [Card], pinned: &[String]) -> Vec<&'a Card> {
    let mut sorted: Vec<&Card> = cards.iter().collect();
    sorted.sort_by_cached_key(|card| {
        pinned
            .iter()
            .position(|id| *id == card.id)
            .unwrap_or(usize::MAX)
    });
    sorted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::CardKind;

    #[test]
    fn pinned_playlists_lead_in_the_order_pinned() {
        let cards: Vec<Card> = ["a", "b", "c", "d"]
            .map(|id| Card {
                kind: CardKind::Playlist,
                id: id.to_owned(),
                title: String::new(),
                subtitle: String::new(),
                thumb: None,
                color: None,
                track: None,
            })
            .into();
        let pinned = ["d", "gone", "b"].map(String::from);
        let order: Vec<&str> = pinned_first(&cards, &pinned)
            .iter()
            .map(|c| c.id.as_str())
            .collect();
        assert_eq!(order, ["d", "b", "a", "c"]);
    }
}
