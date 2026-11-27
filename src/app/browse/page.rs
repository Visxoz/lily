use std::{rc::Rc, time::Instant};

use slint::{ComponentHandle, Image, Model, ModelRc, VecModel};

use crate::app::{
    App, State,
    browse::items::{bare_card, card_item, track_item, track_row},
    library::pins::{PINNED, pinned_cards},
    opaque, s,
};
use crate::{
    Api as ApiGlobal, CardItem, RowItem, RowKind, Rows, TrackItem,
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

/// Whether `page` shows Library's albums by artist: chosen, and all of them
/// loaded. Applied as it shows, never to the cache.
pub(in crate::app) fn orders_albums(state: &State, page: &Page) -> bool {
    state.route == Route::Library && state.settings.albums_by_artist && !page.albums_cut_short
}

/// Library's albums past YouTube's first page load only to be put in order.
pub(in crate::app) fn drop_albums_rest_unless(page: &mut Page, by_artist: bool) {
    if !by_artist && matches!(page.more, Some(api::More::Albums(_))) {
        page.more = None;
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
        ..Default::default()
    })
}

/// Grids longer than this hold art only near the screen: about what the 32 MB
/// artwork cache holds of card covers at 150% scale.
const LAZY_GRID: usize = 120;

/// A lazy grid's cards from `first` up to `end` are near the screen; those
/// past a quarter as many again either side let go of their art.
fn grid_keeps(first: usize, end: usize) -> std::ops::Range<usize> {
    let margin = end.saturating_sub(first) / 4;
    first.saturating_sub(margin)..end + margin
}

/// A lazy grid's cards `first` up to `end` not near the screen as it last
/// said, `was`, so not yet asked for.
fn newly_near(
    first: usize,
    end: usize,
    was: Option<(usize, usize)>,
) -> impl Iterator<Item = usize> {
    let asked = was.map_or(0..0, |(first, end)| first..end);
    (first..end).filter(move |i| !asked.contains(i))
}

/// Where Library scrolls to as its Pinned rows, `added` tall (0 as they go),
/// change: none once its first row is off screen, as Slint keeps it still then.
fn pins_scroll(first_shown: bool, scroll: f32, added: f32) -> Option<f32> {
    if !first_shown || scroll >= 0.0 {
        // At the very top, rows added show where they go.
        None
    } else if added > 0.0 {
        Some(scroll - added)
    } else {
        // What was below can't sit lower than the top.
        Some(0.0)
    }
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

/// Whether a card section is a grid long enough to be lazy.
fn is_lazy(cards: &[api::Card], layout: &CardLayout) -> bool {
    *layout == CardLayout::Grid && cards.len() > LAZY_GRID
}

/// A card as its section shows it: a lazy grid's without art until near the screen.
fn section_card(card: &api::Card, layout: &CardLayout, lazy: bool) -> CardItem {
    if lazy {
        bare_card(card)
    } else {
        card_item(card, card_edge(layout))
    }
}

fn card_model(cards: &[api::Card], layout: &CardLayout, lazy: bool) -> ModelRc<CardItem> {
    let items: Vec<_> = cards
        .iter()
        .map(|c| section_card(c, layout, lazy))
        .collect();
    ModelRc::new(VecModel::from(items))
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
                let lazy = is_lazy(cards, layout);
                let model = card_model(cards, layout, lazy);
                if !lazy {
                    self.card_art
                        .push((model.clone(), card_thumbs(cards), card_edge(layout)));
                }
                self.rows.push(RowItem {
                    kind: match layout {
                        CardLayout::Carousel => RowKind::Carousel,
                        CardLayout::Grid => RowKind::Grid,
                        CardLayout::Chips => RowKind::Chips,
                        CardLayout::Top => RowKind::Top,
                    },
                    cards: model,
                    lazy_art: lazy,
                    section: section_index as i32,
                    ..Default::default()
                });
            }
        }
    }

    /// Library's Pinned row, kept out of the page so the cache never holds it.
    fn pins(&mut self, state: &State, sections: &[Section]) {
        let cards = pinned_cards(&state.settings.pinned, sections);
        if cards.is_empty() {
            return;
        }
        let pinned = Section::Cards {
            title: "Pinned".to_owned(),
            cards,
            layout: CardLayout::Carousel,
        };
        let start = self.rows.len();
        self.section(0, &pinned, &state.downloads);
        for row in &mut self.rows[start..] {
            row.section = PINNED;
        }
    }
}

impl App {
    pub(in crate::app) fn show_page(self: &Rc<Self>, mut page: Page) {
        {
            let state = self.state.borrow();
            // Applied as it shows rather than as it loads, so a cached Home
            // also has what played since, and the cache keeps YouTube's order.
            if state.route == Route::Home && state.signed_in {
                // Local files have no radio to play after them there.
                let recent = state.history.heard().filter(|t| !t.is_local());
                api::add_recent(&mut page.sections, recent);
            }
            if orders_albums(&state, &page) {
                api::sort_by_artist(&mut page.sections);
            }
        }
        self.show_arranged(page);
    }

    /// Shows `page` as `show_page` has arranged it.
    pub(super) fn show_arranged(self: &Rc<Self>, page: Page) {
        let (generation, route) = {
            let state = self.state.borrow();
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
        if route == Route::Library {
            built.pins(&state, &page.sections);
        }
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
        // Rows are made afresh either way, so lazy grids say their lines again.
        self.grid_lines.borrow_mut().clear();
        if in_place {
            self.rows().set_vec(rows);
        } else {
            let model = Rc::new(VecModel::from(rows));
            *self.rows.borrow_mut() = Rc::clone(&model);
            ui.set_rows(ModelRc::from(model));
        }
        ui.set_page_scroll(restore.unwrap_or(0.0));
        *self.rows_route.borrow_mut() = Some(route.clone());
        self.mark_pins();
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
                    Some(Section::Cards { cards, layout, .. })
                        if row.cards.row_count() > 0 && !row.lazy_art =>
                    {
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
        self.load_grids();
        self.show_pins();
    }

    /// Shows Library's Pinned row as the pins now are, in place, so the rest
    /// of the page keeps its rows and where it is scrolled.
    pub(in crate::app) fn show_pins(self: &Rc<Self>) {
        if self.rows_route.borrow().as_ref() != Some(&Route::Library) {
            return;
        }
        let mut built = Built::new(&Route::Library);
        let generation = {
            let state = self.state.borrow();
            built.pins(&state, &state.page.sections);
            state.page_generation
        };
        let rows = self.rows();
        let old = (0..rows.row_count())
            .take_while(|&i| rows.row_data(i).is_some_and(|r| r.section == PINNED))
            .count();
        let new = built.rows.len();
        if old == new && new > 0 {
            // The same title and a carousel, so nothing moves.
            if let Some(carousel) = built.rows.pop() {
                rows.set_row_data(new - 1, carousel);
            }
        } else if old != new {
            // Slint keeps rows on screen still through rows changed above them,
            // so only while the page's first row shows does the page scroll.
            let ui = self.ui();
            let scroll = ui.get_page_scroll();
            let height = |row: RowItem| ui.global::<Rows>().invoke_height(row, 0.0);
            let first_shown = rows.row_data(0).is_some_and(|r| -scroll < height(r));
            let added: f32 = built.rows.iter().cloned().map(height).sum();
            for _ in 0..old {
                rows.remove(0);
            }
            for (i, row) in built.rows.into_iter().enumerate() {
                rows.insert(i, row);
            }
            if let Some(scroll) = pins_scroll(first_shown, scroll, added) {
                ui.set_page_scroll(scroll);
            }
            // Lazy grids say where they are by their row, which moved.
            let moved = (self.grid_lines.take().into_iter())
                .map(|(row, lines)| ((row + new).saturating_sub(old), lines))
                .collect();
            *self.grid_lines.borrow_mut() = moved;
            self.load_grids();
        }
        self.load_art(built.card_art, Vec::new(), generation);
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

    /// Lazy grid `row` has cards `first` up to `end` near the screen.
    pub(in crate::app) fn on_grid_lines(self: &Rc<Self>, row: i32, first: i32, end: i32) {
        let [row, first, end] = [row, first, end].map(|n| usize::try_from(n).unwrap_or(0));
        let was = self.grid_lines.borrow_mut().insert(row, (first, end));
        // Said during Slint's layout pass, so the model is changed once it ends.
        let app = Rc::clone(self);
        slint::Timer::single_shot(std::time::Duration::ZERO, move || {
            app.load_grid(row, was);
        });
    }

    fn load_grids(self: &Rc<Self>) {
        let rows: Vec<usize> = self.grid_lines.borrow().keys().copied().collect();
        for row in rows {
            self.load_grid(row, None);
        }
    }

    /// Loads the art of lazy grid `row`'s cards near the screen, those not
    /// already near it as it said before (`was`), and lets go of any now far
    /// off; none while the window is hidden.
    fn load_grid(self: &Rc<Self>, row: usize, was: Option<(usize, usize)>) {
        if self.art_hidden.get() {
            return;
        }
        let Some((first, end)) = self.grid_lines.borrow().get(&row).copied() else {
            return;
        };
        let Some(item) = self.rows().row_data(row).filter(|r| r.lazy_art) else {
            return;
        };
        let (cards, keeps) = (item.cards, grid_keeps(first, end));
        // A lazy grid starts bare and art lands only within where it last said
        // it was, so only those cards can need letting go of.
        let held = was.map_or(0..0, |(first, end)| grid_keeps(first, end));
        for i in held.start..held.end.min(cards.row_count()) {
            if !keeps.contains(&i)
                && let Some(mut card) = cards.row_data(i).filter(|c| c.art.size().width > 0)
            {
                card.art = Image::default();
                cards.set_row_data(i, card);
            }
        }
        let (generation, wanted) = {
            let state = self.state.borrow();
            let Some(Section::Cards { cards: page, .. }) =
                state.page.sections.get(item.section as usize)
            else {
                return;
            };
            let wanted: Vec<_> = newly_near(first, end.min(cards.row_count()), was)
                .filter_map(|i| {
                    let card = cards.row_data(i).filter(|c| c.art.size().width == 0)?;
                    Some((i, card.id, page.get(i).and_then(|c| c.thumb.clone())))
                })
                .collect();
            (state.page_generation, wanted)
        };
        for (i, id, url) in wanted {
            // Skipped once scrolled far past before its turn comes.
            let still = move |app: &Rc<Self>| {
                on_page(generation)(app)
                    && app
                        .grid_lines
                        .borrow()
                        .get(&row)
                        .is_some_and(|&(first, end)| grid_keeps(first, end).contains(&i))
            };
            let cards = cards.clone();
            self.load_image_while(url, images::CARD, still, move |app, image| {
                // Only on that card, should a reorder have moved another in, and
                // while its grid is still that row, as Pinned's rows move them.
                if still(app)
                    && app.rows().row_data(row).is_some_and(|r| r.cards == cards)
                    && let Some(mut card) = cards
                        .row_data(i)
                        .filter(|c| c.id == id && c.art.size().width == 0)
                {
                    card.art = image;
                    cards.set_row_data(i, card);
                }
            });
        }
    }

    /// Fetches the page's next step if it has one, or waits for its rows to
    /// near the end should it page on scroll.
    fn follow_more(self: &Rc<Self>, route: Route, generation: u64, more: bool) {
        let paused = more && route.pages_on_scroll();
        // Kept through each step, so a failed one waits for the next ask.
        self.ui().global::<ApiGlobal>().set_more_paused(paused);
        self.state.borrow_mut().more_paused = paused.then_some(generation);
        if more && !paused {
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
            // A `Result`, so a step that panics still reaches `done`.
            async move { Ok(api.complete(page).await) },
            move |app, result: api::Result<(Page, bool)>| {
                let Ok((mut page, whole)) = result else {
                    // Paused for the step, the list asks again as it next nears its end.
                    let mut state = app.state.borrow_mut();
                    if route.pages_on_scroll() && state.page_generation == generation {
                        state.more_paused = Some(generation);
                    }
                    return;
                };
                // Signed in or out meanwhile: the page belongs to the last account.
                if app.state.borrow().account != account {
                    return;
                }
                let current = app.state.borrow().page_generation == generation;
                {
                    let state = app.state.borrow();
                    drop_albums_rest_unless(&mut page, state.settings.albums_by_artist);
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
                        // Not on screen, it is needed only here.
                        *cached = if current {
                            page.clone()
                        } else {
                            std::mem::take(&mut page)
                        };
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

    /// Shows tracks or cards added to the shown sections, Library's albums put
    /// in order once all are in, and sections after the shown ones.
    fn extend_page(self: &Rc<Self>, mut page: Page, route: Route, generation: u64) {
        let (shown_tracks, shown_sections, grown) = {
            let state = self.state.borrow();
            let ordered = orders_albums(&state, &page);
            if ordered {
                api::sort_by_artist(&mut page.sections);
            }
            let tracks = match state.page.sections.first() {
                Some(Section::Tracks { tracks, .. }) => tracks.len(),
                _ => 0,
            };
            // Each card list that changed, with how many it showed, or none once
            // put in order, as every card moves.
            let grown: Vec<(usize, Option<usize>)> = state
                .page
                .sections
                .iter()
                .zip(&page.sections)
                .enumerate()
                .filter_map(|(i, sections)| match sections {
                    (Section::Cards { cards: old, .. }, Section::Cards { cards: new, .. })
                        if old.len() != new.len() || ordered && old != new =>
                    {
                        Some((i, (!ordered).then_some(old.len())))
                    }
                    _ => None,
                })
                .collect();
            (tracks, state.page.sections.len(), grown)
        };

        let (lazy_rows, mut added) = self.grow_cards(&page.sections, grown, generation);

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
            added.extend(rows.iter().map(|r| (r.cards.clone(), 0)));
            self.rows().extend(rows);
            self.load_art(card_art, track_art, generation);
        }

        let more = page.more.is_some();
        self.state.borrow_mut().page = page;
        // Once the page holds their cards' artwork.
        for row in lazy_rows {
            self.load_grid(row, None);
        }
        self.mark_cards(added.into_iter());
        self.header_changed();
        self.follow_more(route, generation, more);
    }

    /// Shows the cards of `sections` that `grown` lists, each with how many
    /// it showed (none to start again). Returns the lazy grids' rows, and each
    /// card model changed from its first new card, for the pins' mark.
    fn grow_cards(
        self: &Rc<Self>,
        sections: &[Section],
        grown: Vec<(usize, Option<usize>)>,
        generation: u64,
    ) -> (Vec<usize>, Vec<(ModelRc<CardItem>, usize)>) {
        let (mut lazy_rows, mut added) = (Vec::new(), Vec::new());
        for (i, shown) in grown {
            let Section::Cards { cards, layout, .. } = &sections[i] else {
                continue;
            };
            let Some((at, mut row)) = (0..self.rows().row_count()).find_map(|r| {
                let row = self.rows().row_data(r)?;
                (row.section == i as i32 && row.cards.row_count() > 0).then_some((r, row))
            }) else {
                continue;
            };
            let lazy = is_lazy(cards, layout);
            // Grown long, a grid starts again bare, so art still on its way to
            // its cards lands on none, and only those near the screen get theirs.
            let mut changed = row.lazy_art != lazy;
            row.lazy_art = lazy;
            // Grown in place, so the cards shown and what lies below stay put.
            let mut from = 0;
            if !changed
                && let Some(shown) = shown.filter(|&shown| shown <= cards.len())
                && let Some(model) = row.cards.as_any().downcast_ref::<VecModel<CardItem>>()
            {
                model.extend(cards[shown..].iter().map(|c| section_card(c, layout, lazy)));
                from = shown;
            } else {
                row.cards = card_model(cards, layout, lazy);
                changed = true;
            }
            let model = row.cards.clone();
            added.push((model.clone(), from));
            if changed {
                self.rows().set_row_data(at, row);
            }
            if lazy {
                lazy_rows.push(at);
            } else {
                // Those shown have theirs.
                let mut urls = vec![None; from];
                urls.extend(cards[from..].iter().map(|c| c.thumb.clone()));
                self.load_art(
                    vec![(model, urls, card_edge(layout))],
                    Vec::new(),
                    generation,
                );
            }
        }
        (lazy_rows, added)
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

    #[test]
    fn lazy_grids_keep_art_a_little_past_the_screen() {
        assert_eq!(grid_keeps(80, 160), 60..180);
        assert_eq!(grid_keeps(0, 40), 0..50, "none before the first");
    }

    #[test]
    fn lazy_grids_ask_only_for_cards_new_near_the_screen() {
        assert!(newly_near(10, 20, Some((5, 15))).eq(15..20));
        assert!(newly_near(10, 20, Some((15, 30))).eq(10..15));
        assert!(newly_near(10, 20, None).eq(10..20), "all when first said");
    }

    #[test]
    fn a_page_step_that_panics_still_reaches_done() {
        use crate::app::tasks::Outcome;
        assert!(<api::Result<(Page, bool)> as Outcome>::panicked().is_some());
    }

    #[test]
    fn pins_keep_library_still_where_slint_does_not() {
        assert_eq!(pins_scroll(true, 0.0, 300.0), None, "at the top they show");
        assert_eq!(pins_scroll(true, -20.0, 300.0), Some(-320.0));
        assert_eq!(
            pins_scroll(true, -20.0, 0.0),
            Some(0.0),
            "gone, back to the top"
        );
        assert_eq!(pins_scroll(true, 0.0, 0.0), None);
        assert_eq!(pins_scroll(false, -900.0, 300.0), None, "Slint keeps it");
        assert_eq!(pins_scroll(false, -900.0, 0.0), None);
    }
}
