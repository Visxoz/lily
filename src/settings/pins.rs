//! Songs, albums, playlists and artists pinned to Library, one list for every
//! account on this PC, as blocked artists are, and never sent to YouTube.

use serde::{Deserialize, Serialize};

use crate::api::{Card, CardKind, Track};

/// First pinned first. Tens at most, so a scan answers whether one is pinned.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(from = "Vec<serde_json::Value>")]
pub struct Pins {
    cards: Vec<Card>,
}

/// A pin as saved: a song as its track alone, anything else as its card.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum Saved<C, T> {
    Song {
        song: T,
    },
    Card(C),
    /// An older version's pin, a playlist by ID.
    Id(String),
}

impl Serialize for Pins {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.cards.iter().map(|card| match &card.track {
            Some(song) if card.kind == CardKind::Song => Saved::Song { song },
            _ => Saved::Card(card),
        }))
    }
}

/// One by one, so a pin that won't read loses only itself.
impl From<Vec<serde_json::Value>> for Pins {
    fn from(saved: Vec<serde_json::Value>) -> Self {
        saved
            .into_iter()
            .filter_map(|pin| match serde_json::from_value(pin).ok()? {
                Saved::<Card, Track>::Song { song } => Some(Card::song(&song)),
                Saved::Card(card) => Some(card),
                Saved::Id(id) => Some(Card::bare(CardKind::Playlist, id, "")),
            })
            .collect()
    }
}

impl FromIterator<Card> for Pins {
    fn from_iter<I: IntoIterator<Item = Card>>(cards: I) -> Self {
        let mut pins = Self::default();
        cards.into_iter().for_each(|card| pins.add(card));
        pins
    }
}

impl Pins {
    pub fn cards(&self) -> &[Card] {
        &self.cards
    }

    pub fn has(&self, kind: CardKind, id: &str) -> bool {
        self.cards.iter().any(|c| c.kind == kind && c.id == id)
    }

    /// Pins `card` last, unless it is pinned already.
    pub fn add(&mut self, card: Card) {
        if !self.has(card.kind, &card.id) {
            self.cards.push(card);
        }
    }

    /// Unpins it, answering whether it was pinned.
    pub fn remove(&mut self, kind: CardKind, id: &str) -> bool {
        let len = self.cards.len();
        self.cards.retain(|c| c.kind != kind || c.id != id);
        self.cards.len() != len
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Settings;

    #[test]
    fn pins_keep_their_order_and_kind() {
        let mut pins = Pins::default();
        pins.add(Card::bare(CardKind::Album, "a", "First"));
        pins.add(Card::bare(CardKind::Playlist, "b", "Second"));
        pins.add(Card::bare(CardKind::Album, "a", "Again"));
        let ids: Vec<&str> = pins.cards().iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["a", "b"], "first pinned first, once");
        assert!(pins.has(CardKind::Album, "a"));
        assert!(!pins.has(CardKind::Playlist, "a"), "by kind too");
        assert!(!pins.remove(CardKind::Artist, "b"));
        assert!(pins.remove(CardKind::Playlist, "b"));
        assert!(!pins.has(CardKind::Playlist, "b"));
        assert_eq!(pins.cards().len(), 1);
    }

    #[test]
    fn pins_round_trip_and_older_playlist_ids_still_load() {
        let mut settings = Settings::default();
        settings
            .pinned
            .add(Card::bare(CardKind::Album, "MPREb1", "Currents"));
        let song = Track {
            id: "v1".into(),
            title: "Let It Happen".into(),
            thumb: Some("https://lh3.googleusercontent.com/t=w120-h120-l90-rj".into()),
            ..Default::default()
        };
        settings.pinned.add(Card::song(&song));
        let json = serde_json::to_string(&settings).unwrap();
        assert_eq!(json.matches("Let It Happen").count(), 1, "a song once");
        assert_eq!(Settings::parse(json.as_bytes()), settings);

        let old = Settings::parse(br#"{"pinned": ["LM", "PL1"]}"#);
        let ids: Vec<&str> = old.pinned.cards().iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["LM", "PL1"]);
        assert!(old.pinned.has(CardKind::Playlist, "PL1"));

        let file = br#"{"pinned": [{"kind": "Album", "id": "MPREb1"}, {"kind": "Podcast"},
            {"song": {"id": "v1"}}]}"#;
        let read = Settings::parse(file).pinned;
        assert_eq!(
            read.cards().len(),
            2,
            "fields missing default, and one that won't read loses only itself"
        );
        assert!(read.has(CardKind::Album, "MPREb1"));
        assert!(read.has(CardKind::Song, "v1"));
        assert!(Settings::parse(b"{}").pinned.cards().is_empty());
    }
}
