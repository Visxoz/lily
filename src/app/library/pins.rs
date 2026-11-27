//! Pinning songs, albums, playlists and artists to Library from their menus:
//! Library's Pinned row, the mark on its other cards, and the sidebar's
//! pinned playlists. Pins are kept on this PC only.

use std::{collections::HashMap, rc::Rc};

use slint::{Model, SharedString};

use crate::api::{Card, CardKind, Route, Section};
use crate::app::{
    App, State,
    browse::actions::{card_kind, find_song},
};
use crate::settings::Pins;

/// Library's Pinned row's `section`, as its cards are the pins, not the page's.
pub(in crate::app) const PINNED: i32 = -1;

/// The page's cards, and the pins, which Library shows as cards too.
pub(in crate::app) fn shown_cards(state: &State) -> impl Iterator<Item = &Card> {
    (state.page.sections.iter())
        .flat_map(Section::cards)
        .chain(state.settings.pinned.cards())
}

/// Library's Pinned row, first pinned first, with Library's own card where it
/// lists one; an older version's untitled pin shows only there.
pub(in crate::app) fn pinned_cards(pins: &Pins, sections: &[Section]) -> Vec<Card> {
    let listed: HashMap<&str, &Card> = (sections.iter())
        .flat_map(Section::cards)
        .filter(|c| pins.has(c.kind, &c.id))
        .map(|c| (c.id.as_str(), c))
        .collect();
    (pins.cards().iter())
        .filter_map(|pin| match listed.get(pin.id.as_str()) {
            Some(card) if card.kind == pin.kind => Some(*card),
            _ => (!pin.title.is_empty()).then_some(pin),
        })
        .cloned()
        .collect()
}

impl App {
    /// What a menu's pin acts on: the page's own from `"header"`, a song
    /// from wherever it shows, or a card on the page, in the sidebar or pinned.
    fn card_of(&self, kind: &str, id: &str) -> Option<Card> {
        let state = self.state.borrow();
        if kind == "header" {
            // The route shown, as `state.route` moves on while the next page loads.
            let shown = self.rows_route.borrow();
            let mut card = Card::pinning(shown.as_ref()?, state.page.header.as_ref()?)?;
            // An artist's own picture, should a card of theirs be at hand,
            // rather than its banner's centre.
            let cached = (state.page_cache.values()).flat_map(|(page, _)| &page.sections);
            if card.kind == CardKind::Artist
                && let Some(thumb) = (state.page.sections.iter().chain(cached))
                    .flat_map(Section::cards)
                    .find(|c| c.kind == CardKind::Artist && c.id == card.id)
                    .and_then(|c| c.thumb.clone())
            {
                card.thumb = Some(thumb);
            }
            return Some(card);
        }
        match card_kind(kind)? {
            CardKind::Song => find_song(&state, id)
                .filter(|t| !t.is_local())
                .map(Card::song),
            kind => (shown_cards(&state).chain(&state.library))
                .find(|c| c.kind == kind && c.id == id)
                .cloned(),
        }
    }

    /// The menu entry pinning what a menu acts on; none signed out, as
    /// Library, where pins show, is then only a sign-in prompt.
    pub(in crate::app) fn pin_label(&self, kind: &str, id: &str) -> SharedString {
        let header = (kind == "header").then(|| self.card_of(kind, id)).flatten();
        let target = match &header {
            Some(card) => Some((card.kind, card.id.as_str())),
            None => card_kind(kind).map(|k| (k, id)),
        };
        let Some((kind, id)) = target else {
            return SharedString::new();
        };
        let state = self.state.borrow();
        if !state.signed_in {
            SharedString::new()
        } else if state.settings.pinned.has(kind, id) {
            "Unpin from library".into()
        } else {
            "Pin to library".into()
        }
    }

    /// Pins or unpins it, then shows the pins as they now are: the sidebar's
    /// order should a playlist's change, and Library's Pinned row and marks.
    pub(in crate::app) fn toggle_pin(self: &Rc<Self>, kind: &str, id: &str) {
        let Some(card) = self.card_of(kind, id) else {
            return;
        };
        let playlist = card.kind == CardKind::Playlist;
        let mut pinning = false;
        self.update_settings(|s| {
            pinning = !s.pinned.remove(card.kind, &card.id);
            if pinning {
                s.pinned.add(card);
            }
        });
        self.toast(if pinning {
            "Pinned to your library"
        } else {
            "Unpinned from your library"
        });
        if playlist {
            self.list_library();
        }
        self.show_pins();
        self.mark_pins();
    }

    /// Marks Library's pinned cards, but for the Pinned row's own.
    pub(in crate::app) fn mark_pins(&self) {
        if self.rows_route.borrow().as_ref() != Some(&Route::Library) {
            return;
        }
        let mut marks = Vec::new();
        {
            let pins = &self.state.borrow().settings.pinned;
            for row in self.rows().iter().filter(|r| r.section != PINNED) {
                for i in 0..row.cards.row_count() {
                    let Some(mut card) = row.cards.row_data(i) else {
                        continue;
                    };
                    let pinned = card_kind(&card.kind).is_some_and(|kind| pins.has(kind, &card.id));
                    if card.pinned != pinned {
                        card.pinned = pinned;
                        marks.push((row.cards.clone(), i, card));
                    }
                }
            }
        }
        for (cards, i, card) in marks {
            cards.set_row_data(i, card);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::CardLayout;

    #[test]
    fn pins_lead_library_in_the_order_pinned() {
        let grid = |title: &str, cards| Section::Cards {
            title: title.into(),
            cards,
            layout: CardLayout::Grid,
        };
        let sections = [
            grid(
                "Playlists",
                vec![
                    Card::bare(CardKind::Playlist, "a", "Listed a"),
                    Card::bare(CardKind::Playlist, "b", "Listed b"),
                ],
            ),
            grid("Albums", vec![Card::bare(CardKind::Album, "b", "Album b")]),
        ];
        let pins: Pins = [
            Card::bare(CardKind::Song, "v", "Song"),
            Card::bare(CardKind::Album, "b", "Pinned b"),
            Card::bare(CardKind::Playlist, "a", "Old name"),
            Card::bare(CardKind::Playlist, "gone", ""),
            Card::bare(CardKind::Artist, "UC1", "Not in Library"),
        ]
        .into_iter()
        .collect();
        let titles: Vec<String> = (pinned_cards(&pins, &sections).into_iter())
            .map(|c| c.title)
            .collect();
        assert_eq!(
            titles,
            ["Song", "Album b", "Listed a", "Not in Library"],
            "Library's own card where it lists one, of the same kind"
        );
        let untitled: Pins = [Card::bare(CardKind::Playlist, "gone", "")]
            .into_iter()
            .collect();
        assert!(pinned_cards(&untitled, &sections).is_empty());
        assert!(pinned_cards(&Pins::default(), &sections).is_empty());
    }
}
