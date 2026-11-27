use std::{
    rc::Rc,
    time::{Duration, Instant},
};

use slint::{ComponentHandle, ModelRc, SharedString, TimerMode, VecModel};

use super::page::drop_albums_rest_unless;
use crate::app::{App, HistoryEntry, s};
use crate::{
    Api as ApiGlobal, AppWindow,
    api::{self, Page, Route},
    settings,
};

const PAGE_FRESH_FOR: Duration = Duration::from_secs(5 * 60);
const PAGE_CACHE_SIZE: usize = 12;
const SUGGEST_DELAY: Duration = Duration::from_millis(150);

/// Whether an expired copy shows while it reloads: not one half-loaded, whose
/// continuation expired too, nor one paging on scroll, as refreshing fetches all.
fn shows_expired(route: &Route, page: &Page) -> bool {
    page.more.is_none() && !route.pages_on_scroll()
}

impl App {
    pub(in crate::app) fn navigate(self: &Rc<Self>, route: Route, push: bool) {
        let ui = self.ui();
        let generation = {
            let mut state = self.state.borrow_mut();
            if push {
                if state.route == route && state.page_generation > 0 {
                    return;
                }
                let current = HistoryEntry {
                    route: state.route.clone(),
                    scroll: ui.get_page_scroll(),
                };
                state.back.push(current);
                state.forward.clear();
                // A new page opens at the top, whatever Back left unshown.
                state.restore_scroll = None;
            }
            state.route = route.clone();
            state.page_generation += 1;
            state.page_generation
        };
        self.update_nav(&ui);
        ui.invoke_close_search();
        let signed_in = self.state.borrow().signed_in;
        ui.set_page_shape(
            match route {
                Route::Album(_) => "album",
                // Song lists with a header, as a playlist's.
                Route::Playlist(_) | Route::History | Route::LocalFiles | Route::Downloads => {
                    "playlist"
                }
                Route::Artist(_) => "artist",
                Route::Genre(_) => "genre",
                Route::Home if signed_in => "home",
                Route::Home => "trending",
                Route::Explore => "chips",
                Route::Library => "grid",
                Route::Search(_, None) => "search",
                Route::Search(_, Some(kind)) if kind.songs() => "songs",
                Route::Search(..) => "cards",
            }
            .into(),
        );
        ui.set_page_loading(true);
        let global = ui.global::<ApiGlobal>();
        // Until the page loads and says whether it is one.
        global.set_own_playlist(false);
        global.set_more_paused(false);
        ui.set_page_error(SharedString::new());
        match &route {
            Route::Search(query, kind) => {
                ui.set_search_text(query.into());
                global.set_search_filter(api::SearchKind::filter(*kind));
            }
            _ => {
                ui.set_search_text(SharedString::new());
                global.set_search_filter(-1);
            }
        }

        if route == Route::Library && !signed_in {
            // No page takes it, so it would scroll the next one.
            self.state.borrow_mut().restore_scroll = None;
            ui.set_page_loading(false);
            ui.set_page_error("Sign in to see your playlists, albums and artists.".into());
            ui.set_sign_in_open(true);
            return;
        }
        // Signed in, the History page is the account's own, fetched fresh
        // each visit; the local one stands in when plays aren't sent there.
        let online_history = signed_in && self.state.borrow().settings.youtube_history;
        match route {
            Route::History if !online_history => {
                self.show_or_empty(self.history_page(), "Songs you play will show up here.");
                return;
            }
            Route::Downloads => {
                self.show_downloads();
                return;
            }
            Route::LocalFiles => {
                self.load_local_files(generation);
                return;
            }
            _ => {}
        }

        // An expired copy shows while a fresh one loads behind it.
        if let Some((page, fresh, cached_at)) = self.cached_page(&route) {
            ui.set_page_loading(false);
            self.show_page(page);
            if !fresh {
                self.refresh_page(route, signed_in, generation, cached_at);
            }
            return;
        }

        if route == Route::Home && signed_in {
            self.load_personal_home(generation);
            return;
        }

        let api = self.api.clone();
        let asked = route.clone();
        self.spawn(
            async move { api.page(&asked, signed_in).await },
            // The history isn't cached, so a song just played shows.
            move |app, result| {
                let cache_as = (route != Route::History).then_some(route);
                app.page_loaded(generation, result, cache_as)
            },
        );
    }

    /// The cached copy of `route`, whether it is fresh, and when it was cached.
    pub(in crate::app) fn cached_page(&self, route: &Route) -> Option<(Page, bool, Instant)> {
        self.state
            .borrow()
            .page_cache
            .get(route)
            .and_then(|(page, at)| {
                let fresh = at.elapsed() < PAGE_FRESH_FOR;
                (fresh || shows_expired(route, page)).then(|| (page.clone(), fresh, *at))
            })
    }

    fn page_loaded(
        self: &Rc<Self>,
        generation: u64,
        result: api::Result<Page>,
        cache_as: Option<Route>,
    ) {
        if self.state.borrow().page_generation != generation {
            return;
        }
        let ui = self.ui();
        ui.set_page_loading(false);
        match result {
            Ok(mut page) => {
                drop_albums_rest_unless(&mut page, self.state.borrow().settings.albums_by_artist);
                if let Some(route) = cache_as {
                    self.cache_page(route, page.clone());
                }
                self.show_page(page);
            }
            Err(e) => {
                log::warn!("page failed: {e}");
                self.state.borrow_mut().restore_scroll = None;
                ui.set_page_error(e.into());
            }
        }
    }

    /// Fetches the whole of the expired page on screen, cached at `cached_at`,
    /// and swaps it in where the user has scrolled to should it differ. A
    /// failure keeps the copy.
    fn refresh_page(
        self: &Rc<Self>,
        route: Route,
        signed_in: bool,
        generation: u64,
        cached_at: Instant,
    ) {
        let home = route == Route::Home && signed_in;
        if home {
            // As with each fresh feed, so a changed picture shows.
            self.load_account_photo();
        }
        let revision = self.state.borrow().library_revision;
        let api = self.api.clone();
        let asked = route.clone();
        let edits = self.state.borrow().page_edits.get(&route).copied();
        let by_artist = self.state.borrow().settings.albums_by_artist;
        self.spawn(
            async move {
                let mut page = api.page(&asked, signed_in).await?;
                drop_albums_rest_unless(&mut page, by_artist);
                while page.more.is_some() {
                    let whole;
                    (page, whole) = api.complete(page).await;
                    if !whole {
                        return Err("part of it failed".to_owned());
                    }
                }
                Ok(page)
            },
            move |app, result: api::Result<Page>| {
                let mut page = match result {
                    Ok(page) => page,
                    Err(e) => {
                        log::warn!("could not refresh the page: {e}");
                        return;
                    }
                };
                // Asked without the rest of the albums, which are now wanted.
                if page.albums_cut_short && app.state.borrow().settings.albums_by_artist {
                    return;
                }
                // Put in order for the comparison, so shown without sorting again.
                let mut sorted = None;
                let (ours, swap) = {
                    let state = app.state.borrow();
                    // Gone, as after a like or signing out, it is to load
                    // afresh. Still ours, it is cached even once the user has
                    // moved on. Replaced while still on screen, it was by an
                    // earlier refresh from a visit left and come back to.
                    let Some((cached, at)) = state.page_cache.get(&route) else {
                        return;
                    };
                    if state.page_edits.get(&route).copied() != edits {
                        return;
                    }
                    // Saved or subscribed to since it was asked, it may predate that.
                    if state.library_revision != revision {
                        super::page::keep_library_state(&mut page, Some(cached));
                    }
                    let ours = *at == cached_at;
                    let current = state.page_generation == generation;
                    if !ours && !current {
                        return;
                    }
                    // Home's shelves differ on every load and Home never moves
                    // what is shown, so its fresh feed waits for the next visit.
                    let swap = current
                        && !home
                        && (state.page.header != page.header || {
                            // Against the fresh copy as it would show.
                            let shown_as = if super::page::orders_albums(&state, &page) {
                                let mut sections = page.sections.clone();
                                api::sort_by_artist(&mut sections);
                                &*sorted.insert(sections)
                            } else {
                                &page.sections
                            };
                            state.page.sections != *shown_as
                        });
                    (ours, swap)
                };
                if ours && home {
                    app.save_home(page.sections.clone());
                }
                log::debug!(
                    "refreshed an expired page: {}",
                    if swap {
                        "swapped in"
                    } else if ours {
                        "cached for the next visit"
                    } else {
                        "dropped"
                    }
                );
                if !swap {
                    if ours {
                        app.cache_page(route, page);
                    }
                    return;
                }
                if ours {
                    app.cache_page(route, page.clone());
                }
                app.prepare_reshow();
                match sorted {
                    Some(sections) => {
                        page.sections = sections;
                        app.show_arranged(page);
                    }
                    None => app.show_page(page),
                }
            },
        );
    }

    /// Before the page on screen shows again where it is scrolled. Rows move,
    /// so art still on its way to their old places is dropped.
    pub(in crate::app) fn prepare_reshow(&self) {
        let scroll = self.ui().get_page_scroll();
        let mut state = self.state.borrow_mut();
        state.restore_scroll = Some(scroll);
        state.page_generation += 1;
    }

    /// Should the feed fail before any of it shows, the last whole one stands
    /// in, or the charts: rather that than a fresh one missing shelves.
    fn load_personal_home(self: &Rc<Self>, generation: u64) {
        // Asked with each fresh feed, so a changed or failed picture shows
        // within minutes.
        self.load_account_photo();
        let api = self.api.clone();
        let ahead = self.home_ahead.take();
        self.spawn(
            async move {
                let result = match ahead {
                    Some(ahead) => ahead.await.unwrap_or_else(|e| Err(e.to_string())),
                    None => api.page(&Route::Home, true).await,
                };
                result.map_err(|e| {
                    log::warn!("could not load the personal home feed: {e}");
                    settings::load_home().map(api::arrange_home)
                })
            },
            move |app, result| match result {
                Ok(page) => {
                    // A feed still arriving is saved once whole.
                    if page.more.is_none() && app.state.borrow().page_generation == generation {
                        app.save_home(page.sections.clone());
                    }
                    app.page_loaded(generation, Ok(page), Some(Route::Home));
                }
                Err(Some(sections)) => {
                    let page = Page {
                        sections,
                        ..Default::default()
                    };
                    app.page_loaded(generation, Ok(page), None);
                }
                Err(None) => {
                    if app.state.borrow().page_generation == generation {
                        app.show_public_home(generation);
                    }
                }
            },
        );
    }

    /// Not cached, so Home asks for the personal feed again next time.
    fn show_public_home(self: &Rc<Self>, generation: u64) {
        let api = self.api.clone();
        self.spawn(
            async move { api.page(&Route::Home, false).await },
            move |app, result| app.page_loaded(generation, result, None),
        );
    }

    pub(in crate::app) fn save_home(self: &Rc<Self>, sections: Vec<api::Section>) {
        if !self.state.borrow().signed_in {
            return;
        }
        self.spawn(async move { settings::save_home(&sections) }, |_, ()| {});
    }

    pub(in crate::app) fn cache_page(&self, route: Route, page: Page) {
        let mut state = self.state.borrow_mut();
        let cache = &mut state.page_cache;
        // Expired pages are kept to show while they reload, but not those
        // `navigate` loads afresh.
        cache.retain(|kept, (copy, at)| at.elapsed() < PAGE_FRESH_FOR || shows_expired(kept, copy));
        if !cache.contains_key(&route)
            && cache.len() >= PAGE_CACHE_SIZE
            && let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, (_, at))| *at)
                .map(|(r, _)| r.clone())
        {
            cache.remove(&oldest);
        }
        cache.insert(route, (page, Instant::now()));
    }

    /// A page built on this PC, or the message in its place.
    pub(in crate::app) fn show_or_empty(self: &Rc<Self>, page: Option<Page>, empty: &str) {
        let ui = self.ui();
        ui.set_page_loading(false);
        match page {
            Some(page) => {
                // Shown again with songs, the page may have been empty.
                ui.set_page_error(SharedString::new());
                self.show_page(page)
            }
            None => {
                // No page takes it, so it would scroll the next one.
                self.state.borrow_mut().restore_scroll = None;
                ui.set_page_error(empty.into());
            }
        }
    }

    /// A cached Library with part of its albums loads the rest once they go by
    /// artist, and asks no more once not; one on screen shows again in place.
    pub(in crate::app) fn set_albums_by_artist(self: &Rc<Self>, on: bool) {
        self.update_settings(|s| s.albums_by_artist = on);
        let ui = self.ui();
        let cached = {
            let mut state = self.state.borrow_mut();
            let shown = state.route == Route::Library && state.signed_in && !ui.get_page_loading();
            let cache = &mut state.page_cache;
            let cut_short = cache
                .get(&Route::Library)
                .is_some_and(|(page, _)| page.albums_cut_short);
            if on && cut_short {
                cache.remove(&Route::Library);
            } else if let Some((page, _)) = cache.get_mut(&Route::Library) {
                drop_albums_rest_unless(page, on);
            }
            // Shown in YouTube's order already, as only part came; a step on
            // its way asks for no more once in.
            if !shown || (!on && state.page.albums_cut_short) {
                return;
            }
            state
                .page_cache
                .get(&Route::Library)
                .map(|(page, _)| page.clone())
        };
        self.prepare_reshow();
        match cached {
            // Even expired, so the setting alone asks for nothing.
            Some(page) => self.show_page(page),
            None => self.navigate(Route::Library, false),
        }
    }

    pub(in crate::app) fn reload(self: &Rc<Self>) {
        let route = self.state.borrow().route.clone();
        self.state.borrow_mut().page_cache.remove(&route);
        self.navigate(route, false);
    }

    /// For signing in or out; navigating to the page already shown would do nothing.
    pub(in crate::app) fn show_fresh(self: &Rc<Self>, route: Route) {
        if self.state.borrow().route == route {
            self.reload();
        } else {
            self.navigate(route, true);
        }
    }

    pub(in crate::app) fn go_back(self: &Rc<Self>) {
        self.step_history(false);
    }

    pub(in crate::app) fn go_forward(self: &Rc<Self>) {
        self.step_history(true);
    }

    fn step_history(self: &Rc<Self>, forward: bool) {
        let scroll = self.ui().get_page_scroll();
        let entry = {
            let mut state = self.state.borrow_mut();
            let state = &mut *state;
            let (from, to) = if forward {
                (&mut state.forward, &mut state.back)
            } else {
                (&mut state.back, &mut state.forward)
            };
            let Some(entry) = from.pop() else {
                return;
            };
            to.push(HistoryEntry {
                route: state.route.clone(),
                scroll,
            });
            state.restore_scroll = Some(entry.scroll);
            entry
        };
        self.navigate(entry.route, false);
    }

    pub(in crate::app) fn update_nav(&self, ui: &AppWindow) {
        let state = self.state.borrow();
        ui.set_nav_current(state.route.nav_name().into());
        ui.set_can_back(!state.back.is_empty());
        ui.set_can_forward(!state.forward.is_empty());
    }

    pub(in crate::app) fn search(self: &Rc<Self>, query: &str) {
        self.drop_suggestions();
        let query = query.trim();
        if query.is_empty() {
            return;
        }
        let recorded = self.state.borrow_mut().searches.record(query);
        // Each new search starts on All.
        self.navigate(Route::Search(query.to_owned(), None), true);
        if recorded {
            self.recent_searches_changed();
        }
    }

    /// Shows the search on screen through the filter at `index`, in its place,
    /// so Back leaves the search rather than stepping through its filters.
    pub(in crate::app) fn filter_search(self: &Rc<Self>, index: i32) {
        let kind = api::SearchKind::from_filter(index);
        let route = match &self.state.borrow().route {
            Route::Search(query, shown) if *shown != kind => Route::Search(query.clone(), kind),
            _ => return,
        };
        // Back's scroll for a page still loading isn't this one's.
        self.state.borrow_mut().restore_scroll = None;
        self.navigate(route, false);
    }

    pub(in crate::app) fn forget_search(&self, query: &str) {
        let removed = self.state.borrow_mut().searches.remove(query);
        if removed {
            self.recent_searches_changed();
        }
    }

    /// Saves the recent searches at once rather than through
    /// `persist_soon`, which would rewrite the whole queue too.
    fn recent_searches_changed(&self) {
        self.state.borrow_mut().searches.save();
        self.show_recent_searches();
    }

    pub(in crate::app) fn show_recent_searches(&self) {
        let recent: VecModel<SharedString> =
            self.state.borrow().searches.queries().map(s).collect();
        self.ui()
            .set_recent_searches(ModelRc::from(Rc::new(recent)));
    }

    fn drop_suggestions(&self) {
        self.suggest_timer.stop();
        self.state.borrow_mut().suggest_generation += 1;
        self.ui().set_suggestions(ModelRc::default());
    }

    pub(in crate::app) fn suggest(self: &Rc<Self>, query: String) {
        if query.trim().chars().count() < 2 {
            self.drop_suggestions();
            return;
        }
        let generation = {
            let mut state = self.state.borrow_mut();
            state.suggest_generation += 1;
            state.suggest_generation
        };
        // Restarting the timer drops the previous keystroke's request.
        let weak = Rc::downgrade(self);
        self.suggest_timer
            .start(TimerMode::SingleShot, SUGGEST_DELAY, move || {
                let Some(app) = weak.upgrade() else { return };
                let api = app.api.clone();
                let query = query.clone();
                app.spawn(
                    async move { api.suggestions(&query).await },
                    move |app, terms| {
                        if app.state.borrow().suggest_generation != generation {
                            return;
                        }
                        let terms: Vec<SharedString> = terms.iter().take(8).map(s).collect();
                        app.ui()
                            .set_suggestions(ModelRc::from(Rc::new(VecModel::from(terms))));
                    },
                );
            });
    }
}
