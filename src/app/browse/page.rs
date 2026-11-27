use std::{rc::Rc, time::Instant};

use slint::{ComponentHandle, Image, Model, ModelRc, VecModel};

use crate::app::{
    App,
    browse::items::{card_item, track_item, track_row},
    opaque, s,
};
use crate::{
    Api as ApiGlobal, CardItem, RowItem, RowKind, TrackItem,
    api::{self, CardLayout, Header, Page, QueueMode, Route, Section, Track},
    downloads::Downloads,
    images,
};

/// Carries `live`'s saved and subscribed flags onto `page`, fetched from an
/// older copy and so maybe stale.
pub(in crate::app) fn keep_library_state(page: &mut Page, live: Option<&Page>) {
    if let (Some(header), Some(live)) = (page.header.as_mut(), live.and_then(|p| p.header.as_ref()))
    {
        header.saved = live.saved;
        header.subscribed = live.subscribed;
    }
}

/// A page of `tracks` built on this PC, or `None` without any.
pub(in crate::app) fn track_list_page(
    label: &str,
    title: &str,
    tracks: Vec<Track>,
) -> Option<Page> {
    (!tracks.is_empty()).then(|| Page {
        header: Some(Header {
            label: label.to_owned(),
            title: title.to_owned(),
            meta: api::songs(tracks.len() as u64),
            ..Default::default()
        }),
        sections: vec![Section::Tracks {
            title: String::new(),
            tracks,
            mode: QueueMode::Section,
            numbered: false,
        }],
        more: None,
    })
}

/// About a page of search results: rows this near a paused list's end ask
/// for its next.
const MORE_AHEAD: usize = 20;

/// Long enough for a frame or two to lay out a page's first rows.
const RESEEK_AFTER: std::time::Duration = std::time::Duration::from_millis(60);

/// A model's items' artwork URLs, in its order, at an edge.
type ModelArt<T> = (ModelRc<T>, Vec<Option<String>>, u32);

/// Whether the page shown is still the one loaded as `generation`, so art
/// for a page left behind is neither downloaded nor put in its rows, and the
/// window is shown, as hidden it holds none.
fn on_page(generation: u64) -> impl Fn(&Rc<App>) -> bool + Copy {
    move |app| app.state.borrow().page_generation == generation && !app.art_hidden.get()
}

fn card_edge(layout: &CardLayout) -> u32 {
    match layout {
        CardLayout::Top => images::TOP,
        _ => images::CARD,
    }
}

fn card_thumbs(cards: &[api::Card]) -> Vec<Option<String>> {
    cards.iter().map(|c| c.thumb.clone()).collect()
}

fn track_thumbs(tracks: &[Track]) -> Vec<Option<String>> {
    tracks.iter().map(|t| t.thumb.clone()).collect()
}

fn header_edge(header: Option<&Header>) -> u32 {
    match header {
        Some(h) if h.banner => images::BANNER,
        _ => images::HEADER,
    }
}

#[derive(Default)]
struct Built {
    rows: Vec<RowItem>,
    card_art: Vec<ModelArt<CardItem>>,
    track_art: Vec<ModelArt<TrackItem>>,
    /// Home's titled song sections become shelves that scroll sideways.
    shelves: bool,
}

impl Built {
    fn new(route: &Route) -> Self {
        Self {
            shelves: *route == Route::Home,
            ..Default::default()
        }
    }

    fn section(&mut self, section_index: usize, section: &Section, downloads: &Downloads) {
        let title = section.title();
        if !title.is_empty() {
            self.rows.push(RowItem {
                kind: RowKind::Title,
                title: s(title),
                ..Default::default()
            });
        }
        match section {
            // A shelf makes all its rows, in view or not, so its art loads up
            // front as a card row's does.
            Section::Tracks { tracks, .. } if self.shelves && !title.is_empty() => {
                let model = ModelRc::new(VecModel::from(
                    tracks
                        .iter()
                        .enumerate()
                        .map(|(i, t)| track_item(t, i + 1, downloads.contains(&t.id)))
                        .collect::<Vec<_>>(),
                ));
                self.track_art
                    .push((model.clone(), track_thumbs(tracks), images::ROW));
                self.rows.push(RowItem {
                    kind: RowKind::Songs,
                    tracks: model,
                    section: section_index as i32,
                    ..Default::default()
                });
            }
            Section::Tracks {
                tracks, numbered, ..
            } => {
                // Album and playlist track lists get column labels.
                if title.is_empty() {
                    self.rows.push(RowItem {
                        kind: RowKind::Columns,
                        show_album: !*numbered,
                        ..Default::default()
                    });
                }
                for (i, track) in tracks.iter().enumerate() {
                    self.rows.push(track_row(
                        track,
                        i,
                        *numbered,
                        section_index,
                        downloads.contains(&track.id),
                    ));
                }
            }
            Section::Cards { cards, layout, .. } => {
                let edge = card_edge(layout);
                let model = ModelRc::new(VecModel::from(
                    cards.iter().map(|c| card_item(c, edge)).collect::<Vec<_>>(),
                ));
                self.card_art
                    .push((model.clone(), card_thumbs(cards), edge));
                self.rows.push(RowItem {
                    kind: match layout {
                        CardLayout::Carousel => RowKind::Carousel,
                        CardLayout::Grid => RowKind::Grid,
                        CardLayout::Chips => RowKind::Chips,
                        CardLayout::Top => RowKind::Top,
                    },
                    cards: model,
                    section: section_index as i32,
                    ..Default::default()
                });
            }
        }
    }
}

impl App {
    pub(in crate::app) fn show_page(self: &Rc<Self>, mut page: Page) {
        let (generation, route) = {
            let state = self.state.borrow();
            // Applied as it shows rather than as it loads, so a cached Home
            // also has what played since.
            if state.route == Route::Home && state.signed_in {
                // Local files have no radio to play after them there.
                let recent = state.history.heard().filter(|t| !t.is_local());
                api::add_recent(&mut page.sections, recent);
            }
            (state.page_generation, state.route.clone())
        };
        let mut built = Built::new(&route);

        if route == Route::Home {
            let (date, greeting) = greeting();
            built.rows.push(RowItem {
                kind: RowKind::Greeting,
                label: s(&date),
                title: s(greeting),
                ..Default::default()
            });
        }

        let header_edge = header_edge(page.header.as_ref());
        let state = self.state.borrow();
        let downloads = &state.downloads;
        if let Some(header) = &page.header {
            let tracks = page.header_tracks();
            let has_tracks = !tracks.is_empty();
            built.rows.push(RowItem {
                kind: RowKind::Header,
                label: s(&header.label),
                title: s(&header.title),
                subtitle: s(&header.subtitle),
                meta: s(&header.meta),
                art: images::cached_or_empty(header.thumb.as_deref(), header_edge),
                round: header.round,
                banner: header.banner,
                can_play: has_tracks,
                can_shuffle: has_tracks || header.shuffle_playlist.is_some(),
                can_radio: header.radio_id.is_some(),
                can_download: has_tracks && matches!(route, Route::Album(_) | Route::Playlist(_)),
                can_share: route.link().is_some(),
                downloaded: has_tracks && tracks.iter().all(|t| downloads.contains(&t.id)),
                can_save: header.saved.is_some() && header.save_id.is_some() && !header.editable,
                saved: header.saved == Some(true),
                can_subscribe: header.subscribed.is_some() && header.channel_id.is_some(),
                subscribed: header.subscribed == Some(true),
                ..Default::default()
            });
        }

        for (section_index, section) in page.sections.iter().enumerate() {
            built.section(section_index, section, downloads);
        }
        drop(state);

        let ui = self.ui();
        let header_thumb = page.header.as_ref().and_then(|h| h.thumb.clone());
        ui.set_page_title(s(page.header.as_ref().map_or("", |h| h.title.as_str())));
        ui.set_page_can_play(!page.header_tracks().is_empty());
        let tint = header_thumb
            .as_deref()
            .and_then(|url| images::accent(url, header_edge))
            .unwrap_or_else(|| route_tint(&route));
        ui.set_accent(tint);

        let more = page.more.is_some();
        self.state.borrow_mut().page = page;
        self.offer_edits();
        let Built {
            rows,
            card_art,
            track_art,
            ..
        } = built;
        let restore = self.state.borrow_mut().restore_scroll.take();
        // Shown again in place, as a refresh does, the rows keep their model.
        // Any other page gets a new one, as Slint's ListView keeps where the old
        // rows were scrolled through a reset and can open it part-way down.
        let in_place = restore.is_some() && self.rows_route.borrow().as_ref() == Some(&route);
        self.row_art.reset();
        if in_place {
            self.rows().set_vec(rows);
        } else {
            let model = Rc::new(VecModel::from(rows));
            *self.rows.borrow_mut() = Rc::clone(&model);
            ui.set_rows(ModelRc::from(model));
        }
        ui.set_page_scroll(restore.unwrap_or(0.0));
        *self.rows_route.borrow_mut() = Some(route.clone());
        // A new model's first layout measures only its first row, the header,
        // and seeks by it; once real rows are measured, the same seek lands.
        if let Some(scroll) = restore.filter(|&s| !in_place && s != 0.0) {
            let weak = Rc::downgrade(self);
            slint::Timer::single_shot(RESEEK_AFTER, move || {
                if let Some(app) = weak.upgrade()
                    && app.state.borrow().page_generation == generation
                {
                    app.ui().set_page_scroll(scroll);
                }
            });
        }

        self.load_header_art(header_thumb, header_edge, generation);
        self.load_art(card_art, track_art, generation);
        self.follow_more(route, generation, more);
    }

    fn load_header_art(self: &Rc<Self>, thumb: Option<String>, edge: u32, generation: u64) {
        // Only called with an image, so with a URL.
        let url = thumb.clone().unwrap_or_default();
        self.load_image_while(thumb, edge, on_page(generation), move |app, image| {
            // Shown again while the next page loads, the old one's header is
            // reloaded under the new page's generation.
            let shown = app
                .state
                .borrow()
                .page
                .header
                .as_ref()
                .and_then(|h| h.thumb.clone());
            if !on_page(generation)(app) || shown.as_deref() != Some(url.as_str()) {
                return;
            }
            if let Some(color) = images::accent(&url, edge) {
                app.ui().set_accent(color);
            }
            if let Some(index) = app.find_row(RowKind::Header) {
                let mut row = app.rows().row_data(index).unwrap_or_default();
                row.art = image;
                row.backdrop = images::backdrop(&url, edge).unwrap_or_default();
                app.rows().set_row_data(index, row);
            }
        });
    }

    /// Loads the shown page's header, card and shelf art again, as the page first
    /// did, once the window that let go of it is shown.
    pub(in crate::app) fn load_page_art(self: &Rc<Self>) {
        let (generation, thumb, edge) = {
            let state = self.state.borrow();
            let header = state.page.header.as_ref();
            let thumb = header.and_then(|h| h.thumb.clone());
            (state.page_generation, thumb, header_edge(header))
        };
        let (mut card_art, mut track_art) = (Vec::new(), Vec::new());
        {
            let state = self.state.borrow();
            for row in self.rows().iter() {
                match state.page.sections.get(row.section as usize) {
                    Some(Section::Cards { cards, layout, .. }) if row.cards.row_count() > 0 => {
                        card_art.push((row.cards, card_thumbs(cards), card_edge(layout)));
                    }
                    Some(Section::Tracks { tracks, .. }) if row.kind == RowKind::Songs => {
                        track_art.push((row.tracks, track_thumbs(tracks), images::ROW));
                    }
                    _ => {}
                }
            }
        }
        self.load_header_art(thumb, edge, generation);
        self.load_art(card_art, track_art, generation);
    }

    pub(in crate::app) fn find_row(&self, kind: RowKind) -> Option<usize> {
        (0..self.rows().row_count())
            .find(|&i| self.rows().row_data(i).is_some_and(|r| r.kind == kind))
    }

    /// Loads card rows' and song shelves' artwork into their models.
    fn load_art(
        self: &Rc<Self>,
        cards: Vec<ModelArt<CardItem>>,
        tracks: Vec<ModelArt<TrackItem>>,
        generation: u64,
    ) {
        self.load_model_art(cards, generation, |card, image| card.art = image);
        self.load_model_art(tracks, generation, |track, image| track.art = image);
    }

    fn load_model_art<T: Clone + 'static>(
        self: &Rc<Self>,
        art: Vec<ModelArt<T>>,
        generation: u64,
        set: fn(&mut T, Image),
    ) {
        for (model, urls, edge) in art {
            for (i, url) in urls.into_iter().enumerate() {
                let model = model.clone();
                self.load_image_while(url, edge, on_page(generation), move |app, image| {
                    if on_page(generation)(app)
                        && let Some(mut item) = model.row_data(i)
                    {
                        set(&mut item, image);
                        model.set_row_data(i, item);
                    }
                });
            }
        }
    }

    /// Loads the artwork of track row `row`, asked for by the row once on
    /// screen, and the page's next step should the row near its end.
    pub(in crate::app) fn load_row_art(self: &Rc<Self>, row: usize) {
        self.more_near(row);
        let Some(item) = self
            .rows()
            .row_data(row)
            .filter(|r| r.kind == RowKind::Track && r.track.art.size().width == 0)
        else {
            return;
        };
        let (url, on_page) = {
            let state = self.state.borrow();
            let url = match state.page.sections.get(item.section as usize) {
                Some(Section::Tracks { tracks, .. }) => tracks
                    .get(item.index as usize)
                    .and_then(|t| t.thumb.clone()),
                _ => None,
            };
            (url, on_page(state.page_generation))
        };
        let id = item.track.id;
        self.load_row_image(
            url,
            row,
            |app| &app.row_art,
            move |app| {
                on_page(app)
                    && app
                        .rows()
                        .row_data(row)
                        .is_some_and(|r| r.track.id == id && r.track.art.size().width == 0)
            },
            move |app, image| {
                if let Some(mut item) = app.rows().row_data(row) {
                    item.track.art = image;
                    app.rows().set_row_data(row, item);
                }
            },
            |app, row| {
                if let Some(mut item) = app
                    .rows()
                    .row_data(row)
                    .filter(|r| r.kind == RowKind::Track && r.track.art.size().width > 0)
                {
                    item.track.art = Image::default();
                    app.rows().set_row_data(row, item);
                }
            },
        );
    }

    /// Fetches the page's next step if it has one, or waits for its rows to
    /// near the end should it page on scroll.
    fn follow_more(self: &Rc<Self>, route: Route, generation: u64, more: bool) {
        let paused = more && route.pages_on_scroll();
        // Kept through each step, so a failed one waits for the next ask.
        self.ui().global::<ApiGlobal>().set_more_paused(paused);
        if paused {
            self.state.borrow_mut().more_paused = Some(generation);
        } else if more {
            self.complete_page(route, generation);
        }
    }

    /// Fetches the paused page's next step once row `row`, on screen, is
    /// within `MORE_AHEAD` of its end; hidden, the row is held and asks again.
    fn more_near(self: &Rc<Self>, row: usize) {
        if self.art_hidden.get() {
            return;
        }
        let (route, generation) = {
            let mut state = self.state.borrow_mut();
            if state.more_paused != Some(state.page_generation)
                || row + MORE_AHEAD < self.rows().row_count()
            {
                return;
            }
            state.more_paused = None;
            (state.route.clone(), state.page_generation)
        };
        self.complete_page(route, generation);
    }

    /// Fetches one step of `Page.more`; the next follows once it is on screen.
    fn complete_page(self: &Rc<Self>, route: Route, generation: u64) {
        let (page, account, edits) = {
            let state = self.state.borrow();
            let edits = state.page_edits.get(&route).copied();
            (state.page.clone(), state.account, edits)
        };
        let shown = page.sections.len();
        let api = self.api.clone();
        self.spawn(
            async move { api.complete(page).await },
            move |app, (mut page, whole)| {
                // Signed in or out meanwhile: the page belongs to the last account.
                if app.state.borrow().account != account {
                    return;
                }
                let current = app.state.borrow().page_generation == generation;
                {
                    let state = app.state.borrow();
                    let live = if current {
                        Some(&state.page)
                    } else {
                        state.page_cache.get(&route).map(|(cached, _)| cached)
                    };
                    keep_library_state(&mut page, live);
                }
                if route == Route::Home {
                    if current {
                        app.complete_home(&page, shown, whole);
                    }
                } else if !whole && page.more.is_none() {
                    // Cut short, it would play and show as whole; the next visit asks again.
                    app.state.borrow_mut().page_cache.remove(&route);
                } else {
                    // Edited since it was asked, the cached copy is newer.
                    let mut state = app.state.borrow_mut();
                    let edited = state.page_edits.get(&route).copied() != edits;
                    if !edited && let Some((cached, at)) = state.page_cache.get_mut(&route) {
                        *cached = page.clone();
                        // Never refreshed, it stays fresh from its last step.
                        if route.pages_on_scroll() {
                            *at = Instant::now();
                        }
                    }
                }
                if current {
                    app.extend_page(page, route, generation);
                }
            },
        );
    }

    /// The cached Home, unlike the page on screen, has Listen again as YouTube
    /// sent it. After a failed request it is neither cached nor saved, so the
    /// next visit asks again.
    fn complete_home(self: &Rc<Self>, page: &Page, shown: usize, whole: bool) {
        let saved = {
            let mut state = self.state.borrow_mut();
            if !whole {
                state.page_cache.remove(&Route::Home);
                return;
            }
            let Some((cached, _)) = state.page_cache.get_mut(&Route::Home) else {
                return;
            };
            cached
                .sections
                .extend(page.sections.iter().skip(shown).cloned());
            cached.more.clone_from(&page.more);
            cached.more.is_none().then(|| cached.sections.clone())
        };
        if let Some(saved) = saved {
            self.save_home(saved);
        }
    }

    /// Shows tracks or cards added to the first section, and sections after
    /// the shown ones.
    fn extend_page(self: &Rc<Self>, page: Page, route: Route, generation: u64) {
        let (shown_tracks, shown_cards, shown_sections) = {
            let state = self.state.borrow();
            let (tracks, cards) = match state.page.sections.first() {
                Some(Section::Tracks { tracks, .. }) => (tracks.len(), 0),
                Some(Section::Cards { cards, .. }) => (0, cards.len()),
                None => (0, 0),
            };
            (tracks, cards, state.page.sections.len())
        };

        // A kind's search results grow their one grid.
        if let Some(Section::Cards { cards, layout, .. }) = page.sections.first()
            && cards.len() > shown_cards
            && shown_sections > 0
            && let Some(row) = self
                .rows()
                .iter()
                .find(|r| r.section == 0 && r.cards.row_count() > 0)
            && let Some(model) = row.cards.as_any().downcast_ref::<VecModel<CardItem>>()
        {
            let edge = card_edge(layout);
            model.extend(cards[shown_cards..].iter().map(|c| card_item(c, edge)));
            let mut urls = card_thumbs(cards);
            // Those shown have theirs.
            urls[..shown_cards].fill(None);
            self.load_art(
                vec![(row.cards.clone(), urls, edge)],
                Vec::new(),
                generation,
            );
        }

        if let Some(Section::Tracks {
            tracks, numbered, ..
        }) = page.sections.first()
            && tracks.len() > shown_tracks
            && shown_sections > 0
            && let Some(last_row) = (0..self.rows().row_count()).rev().find(|&i| {
                self.rows()
                    .row_data(i)
                    .is_some_and(|r| r.kind == RowKind::Track && r.section == 0)
            })
        {
            for (index, track) in tracks.iter().enumerate().skip(shown_tracks) {
                let row_index = last_row + 1 + index - shown_tracks;
                let downloaded = self.state.borrow().downloads.contains(&track.id);
                self.rows()
                    .insert(row_index, track_row(track, index, *numbered, 0, downloaded));
            }
        }

        if page.sections.len() > shown_sections {
            let mut built = Built::new(&route);
            let state = self.state.borrow();
            for (section_index, section) in page.sections.iter().enumerate().skip(shown_sections) {
                built.section(section_index, section, &state.downloads);
            }
            drop(state);
            let Built {
                rows,
                card_art,
                track_art,
                ..
            } = built;
            self.rows().extend(rows);
            self.load_art(card_art, track_art, generation);
        }

        let more = page.more.is_some();
        self.state.borrow_mut().page = page;
        self.header_changed();
        self.follow_more(route, generation, more);
    }

    /// Shows the page's header as it now is, its song count for one.
    pub(in crate::app) fn refresh_header(&self) {
        let state = self.state.borrow();
        let Some(header) = &state.page.header else {
            return;
        };
        self.ui().set_page_title(s(&header.title));
        if let Some(i) = self.find_row(RowKind::Header)
            && let Some(mut row) = self.rows().row_data(i)
        {
            let tracks = state.page.header_tracks();
            row.title = s(&header.title);
            row.meta = s(&header.meta);
            row.downloaded =
                !tracks.is_empty() && tracks.iter().all(|t| state.downloads.contains(&t.id));
            self.rows().set_row_data(i, row);
        }
    }
}

/// The date, as "Tuesday 24 November", and the hour's greeting.
fn greeting() -> (String, &'static str) {
    let now = crate::app::local_now();
    let date = format!("{} {} {}", now.weekday(), now.day(), now.month());
    (date, greeting_for(now.hour()))
}

fn greeting_for(hour: u8) -> &'static str {
    match hour {
        4..12 => "Good morning",
        12..18 => "Good afternoon",
        18..23 => "Good evening",
        _ => "Good night",
    }
}

fn route_tint(route: &Route) -> slint::Color {
    let rgb = match route {
        // Transparent, for the page to follow the theme's accent.
        Route::Home => return slint::Color::default(),
        Route::Explore => 0x1e3264,
        Route::Library => 0x3d1f5c,
        Route::Search(..) => 0x2a2a2a,
        _ => 0x333333,
    };
    opaque(rgb)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn greets_by_hour() {
        assert_eq!(greeting_for(4), "Good morning");
        assert_eq!(greeting_for(11), "Good morning");
        assert_eq!(greeting_for(12), "Good afternoon");
        assert_eq!(greeting_for(18), "Good evening");
        assert_eq!(greeting_for(23), "Good night");
        assert_eq!(greeting_for(2), "Good night");
    }
}
