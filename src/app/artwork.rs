//! Loading artwork, and the rows of long lists asking for theirs only once
//! on screen, so a fast scroll or a long list doesn't download or hold every one.

use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    rc::Rc,
    time::Duration,
};

use slint::{Image, Model, Timer};

use super::App;
use crate::images;

/// A row's artwork still waiting once this many more rows have asked is for
/// one scrolled past, as a screen shows far fewer.
const RECENT_ASKS: u64 = 64;
/// Row thumbnails a list's model keeps. Past it, the oldest given away from
/// the rows on screen are let go, as a long list scrolled through would
/// otherwise hold every one it showed, beyond the artwork cache's budget.
const KEEP_ROW_ART: usize = 160;

/// A list's rows asking for their artwork.
#[derive(Default)]
pub(super) struct RowArt {
    /// How many have asked, so a download left waiting by a fast scroll is skipped.
    asks: Cell<u64>,
    /// The last to ask, which is on screen.
    last: Cell<usize>,
    /// Rows given art, oldest first.
    kept: RefCell<VecDeque<usize>>,
    /// Rows that asked while the window was hidden, to load once shown, as a
    /// row asks only as its art goes.
    held: RefCell<Vec<usize>>,
}

impl RowArt {
    /// Forgets the rows given art, as their model was rebuilt. Held rows
    /// stay, as their loaders look again at what each row holds.
    pub(super) fn reset(&self) {
        self.kept.borrow_mut().clear();
    }

    fn hold(&self, row: usize) {
        let mut held = self.held.borrow_mut();
        if !held.contains(&row) {
            held.push(row);
        }
    }

    /// Notes `row` given art, and returns the rows whose art to let go.
    fn keep(&self, row: usize) -> Vec<usize> {
        let mut kept = self.kept.borrow_mut();
        kept.push_back(row);
        overflow(&mut kept, self.last.get())
    }
}

/// Takes the oldest of `kept` past `KEEP_ROW_ART`, skipping those within
/// `RECENT_ASKS` rows of `last`, which may be on screen and keep theirs.
fn overflow(kept: &mut VecDeque<usize>, last: usize) -> Vec<usize> {
    let mut released = Vec::new();
    let mut near = 0;
    while kept.len() > KEEP_ROW_ART && near < kept.len() {
        let Some(row) = kept.pop_front() else { break };
        if row.abs_diff(last) <= RECENT_ASKS as usize {
            kept.push_back(row);
            near += 1;
        } else {
            released.push(row);
        }
    }
    released
}

impl App {
    pub(super) fn load_image(
        self: &Rc<Self>,
        url: Option<String>,
        edge: u32,
        apply: impl FnOnce(&Rc<Self>, Image) + 'static,
    ) {
        self.load_image_while(url, edge, |_| true, apply);
    }

    /// `load_image` for an image skipped if, once its turn to download
    /// comes, it is no longer `wanted`.
    pub(super) fn load_image_while(
        self: &Rc<Self>,
        url: Option<String>,
        edge: u32,
        wanted: impl FnOnce(&Rc<Self>) -> bool + 'static,
        apply: impl FnOnce(&Rc<Self>, Image) + 'static,
    ) {
        let Some(url) = url else { return };
        if let Some(image) = images::cached(&url, edge) {
            apply(self, image);
            return;
        }
        let app = Rc::clone(self);
        let rt = self.rt.clone();
        let _ = slint::spawn_local(async move {
            if let Some(image) = images::load(&rt, &url, edge, || wanted(&app)).await {
                apply(&app, image);
            }
        });
    }

    /// Rows ask during Slint's layout pass, so the model is changed once it ends.
    pub(super) fn on_row_art(self: &Rc<Self>, load: fn(&Rc<Self>, usize), row: i32) {
        let app = Rc::clone(self);
        Timer::single_shot(Duration::ZERO, move || load(&app, row as usize));
    }

    /// A list row's artwork, asked for by `row` once on screen, put in by
    /// `apply` while the row still `wants` it. Skipped should `RECENT_ASKS`
    /// more rows of the `list` ask before its download's turn comes. Rows
    /// past `KEEP_ROW_ART` lose theirs to `release`, to ask again if shown.
    pub(super) fn load_row_image(
        self: &Rc<Self>,
        url: Option<String>,
        row: usize,
        list: fn(&Self) -> &RowArt,
        wants: impl Fn(&Rc<Self>) -> bool + Clone + 'static,
        apply: impl FnOnce(&Rc<Self>, Image) + 'static,
        release: fn(&Rc<Self>, usize),
    ) {
        // Hidden, the row is held, and loaded once shown.
        let shown = move |app: &Rc<Self>| {
            if app.art_hidden.get() {
                list(app).hold(row);
            }
            !app.art_hidden.get()
        };
        if !shown(self) {
            return;
        }
        let ask = list(self).asks.get() + 1;
        list(self).asks.set(ask);
        list(self).last.set(row);
        let still = wants.clone();
        self.load_image_while(
            url,
            images::ROW,
            move |app| list(app).asks.get() - ask < RECENT_ASKS && shown(app) && still(app),
            move |app, image| {
                if shown(app) && wants(app) {
                    apply(app, image);
                    for old in list(app).keep(row) {
                        release(app, old);
                    }
                }
            },
        );
    }

    /// Lets go of the page's and the queue's artwork while the window is
    /// hidden, as their models would otherwise hold it past the cache's
    /// budget, and loads it again once shown. The player's stays, as the
    /// mini player shows it.
    pub fn hide_art(self: &Rc<Self>, hidden: bool) {
        if self.art_hidden.replace(hidden) == hidden {
            return;
        }
        if hidden {
            self.row_art.reset();
            self.queue_art.reset();
            let rows = self.rows();
            for i in 0..rows.row_count() {
                let Some(mut row) = rows.row_data(i) else {
                    continue;
                };
                for c in 0..row.cards.row_count() {
                    if let Some(mut card) = row.cards.row_data(c).filter(|c| has(&c.art)) {
                        card.art = Image::default();
                        row.cards.set_row_data(c, card);
                    }
                }
                for t in 0..row.tracks.row_count() {
                    if let Some(mut track) = row.tracks.row_data(t).filter(|t| has(&t.art)) {
                        track.art = Image::default();
                        row.tracks.set_row_data(t, track);
                    }
                }
                if has(&row.art) || has(&row.backdrop) || has(&row.track.art) {
                    row.art = Image::default();
                    row.backdrop = Image::default();
                    row.track.art = Image::default();
                    rows.set_row_data(i, row);
                }
            }
            for i in 0..self.queue_model.row_count() {
                if let Some(mut item) = self.queue_model.row_data(i).filter(|q| has(&q.art)) {
                    item.art = Image::default();
                    self.queue_model.set_row_data(i, item);
                }
            }
            self.ui().set_np_cover(Image::default());
            images::set_hidden(true);
        } else {
            images::set_hidden(false);
            for row in self.row_art.held.take() {
                self.load_row_art(row);
            }
            for row in self.queue_art.held.take() {
                self.load_queue_art(row);
            }
            self.load_page_art();
            if self.ui().get_full_player() {
                self.load_cover();
            }
        }
    }
}

fn has(image: &Image) -> bool {
    image.size().width > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lets_go_of_old_row_art_away_from_the_screen() {
        let near = RECENT_ASKS as usize;
        // Rows 0.. given art in order, the screen now at the last of them.
        let mut kept: VecDeque<usize> = (0..=KEEP_ROW_ART).collect();
        assert_eq!(overflow(&mut kept, KEEP_ROW_ART), [0]);
        assert_eq!(kept.len(), KEEP_ROW_ART);
        // Scrolled back to the top: the oldest are near it, so the newest go.
        kept.push_back(1_000);
        let released = overflow(&mut kept, 1);
        assert_eq!(released, [near + 2], "the first one far from the screen");
        assert!(kept.contains(&1) && kept.contains(&1_000));
        // Every row near the screen: none go, and the loop ends.
        let mut all_near: VecDeque<usize> = (0..=KEEP_ROW_ART).map(|_| 5).collect();
        assert!(overflow(&mut all_near, 5).is_empty());
    }
}
