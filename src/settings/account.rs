//! What belongs to the account signed in when it was saved: the Home feed,
//! the sidebar's playlists and the visitor ID, dropped on signing in or out.

use std::{path::PathBuf, time::Duration};

use super::{data_dir, read_json, save_json, write_atomic};

fn home_path() -> PathBuf {
    data_dir().join("home.json")
}

/// The signed-in home feed as it last loaded in full. YouTube refuses some
/// requests for the feed while the rest of the account works, so Home shows
/// this when a fresh copy cannot be had.
pub fn load_home() -> Option<Vec<crate::api::Section>> {
    read_json::<Vec<crate::api::Section>>(&home_path()).filter(|sections| !sections.is_empty())
}

pub fn save_home(sections: &[crate::api::Section]) {
    save_json(&home_path(), &sections, "the home feed");
}

/// Drops the saved feed, sidebar and visitor ID, which belong to the account
/// signed in when they were saved.
pub fn remove_account_data() {
    let _ = std::fs::remove_file(home_path());
    let _ = std::fs::remove_file(library_path());
    let _ = std::fs::remove_file(visitor_path());
}

fn library_path() -> PathBuf {
    data_dir().join("library.json")
}

/// The sidebar's playlists as they last loaded, shown at once on the next start.
pub fn load_library() -> Vec<crate::api::Card> {
    read_json(&library_path()).unwrap_or_default()
}

pub fn save_library(cards: &[crate::api::Card]) {
    save_json(&library_path(), &cards, "the sidebar's playlists");
}

/// Each run saves a fresh visitor ID, so only a copy left closed this long
/// finds its saved one too old to send.
const VISITOR_FOR: Duration = Duration::from_secs(24 * 60 * 60);

fn visitor_path() -> PathBuf {
    data_dir().join("visitor.txt")
}

/// The visitor ID the last run fetched, while under a day old by the file's
/// time; checked, as it goes into a request header.
pub fn load_visitor() -> Option<String> {
    let path = visitor_path();
    let age = std::fs::metadata(&path)
        .ok()?
        .modified()
        .ok()?
        .elapsed()
        .ok()?;
    usable_visitor(std::fs::read_to_string(&path).ok()?, age)
}

fn usable_visitor(id: String, age: Duration) -> Option<String> {
    let plausible = (1..=1024).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_graphic());
    (plausible && age < VISITOR_FOR).then_some(id)
}

pub fn save_visitor(id: &str) {
    if let Err(e) = write_atomic(&visitor_path(), id.as_bytes()) {
        log::warn!("could not save the visitor ID: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_visitor_stands_in_for_a_day_only() {
        let usable = |id: &str, age| usable_visitor(id.into(), age);
        let minute = Duration::from_secs(60);
        assert_eq!(usable("Cgt%3D", minute).as_deref(), Some("Cgt%3D"));
        assert_eq!(usable("Cgt%3D", VISITOR_FOR), None);
        assert_eq!(usable("a\r\nb", minute), None, "not a header value");
        assert_eq!(usable("", minute), None);
    }

    #[test]
    fn home_feed_survives_saving() {
        use crate::api::{Card, CardKind, CardLayout, QueueMode, Section, Track};
        let track = Track {
            id: "44zDzdf08s4".into(),
            title: "Matt Hardy 999".into(),
            thumb: Some("https://lh3.googleusercontent.com/x".into()),
            video: true,
            ..Default::default()
        };
        let sections = vec![
            Section::Cards {
                title: "Listen again".into(),
                cards: vec![Card {
                    kind: CardKind::Song,
                    id: track.id.clone(),
                    title: track.title.clone(),
                    subtitle: "Song".into(),
                    thumb: None,
                    color: Some(0x5c0f1c),
                    track: Some(track.clone()),
                }],
                layout: CardLayout::Carousel,
            },
            Section::Tracks {
                title: "Quick picks".into(),
                tracks: vec![track],
                mode: QueueMode::Section,
                numbered: false,
            },
        ];
        let json = serde_json::to_vec(&sections).unwrap();
        let back: Vec<Section> = serde_json::from_slice(&json).unwrap();
        let Section::Cards {
            title,
            cards,
            layout,
        } = &back[0]
        else {
            panic!("expected cards")
        };
        assert_eq!(title, "Listen again");
        assert_eq!(*layout, CardLayout::Carousel);
        assert_eq!(cards[0].kind, CardKind::Song);
        assert_eq!(cards[0].color, Some(0x5c0f1c));
        assert!(cards[0].track.as_ref().is_some_and(|t| t.video));
        assert!(matches!(
            &back[1],
            Section::Tracks {
                mode: QueueMode::Section,
                ..
            }
        ));
    }
}
