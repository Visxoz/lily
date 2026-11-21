//! The personal home feed (`FEmusic_home`), which rustypipe has no model for.
//! Shelves are held back until every one ranked above them has arrived.

mod parse;

use rustypipe::client::{ClientType, RustyPipeQuery};

use super::{
    Card, CardKind, CardLayout, QueueMode, Section, Track,
    convert::{CARD_THUMB, resize_thumb},
};
use parse::{first_page, next_page};

/// Continuations fetched at most after the first page; the whole feed is
/// five or six pages.
const CONTINUATIONS: usize = 8;
/// The shelves Home shows, in order: those made from the account's own
/// listening. Titles are in English, rustypipe's language.
const SHELVES: [&str; 6] = [
    "Listen again",
    "Quick picks",
    "Forgotten favorites",
    "Trending songs for you",
    "Mixed for you",
    "Fresh finds, old favorites",
];
const LISTEN_AGAIN: &str = SHELVES[0];
/// Songs played lately that join Listen again, which YouTube fills with
/// older favourites and seldom the latest plays, one after every
/// `RECENT_EVERY` of YouTube's own, so they never lead it.
const RECENT: usize = 5;
const RECENT_EVERY: usize = 3;

const ATTEMPTS: u32 = 4;

/// YouTube refuses a share of signed-in feed requests, in bursts, with "400:
/// Request contains an invalid argument", and answers the same one moments later.
fn refused(e: &rustypipe::error::Error) -> bool {
    matches!(e, rustypipe::error::Error::HttpStatus(400, _))
}

async fn browse(
    q: &RustyPipeQuery,
    body: &serde_json::Value,
    attempts: u32,
) -> Result<String, String> {
    let mut attempt = 1;
    loop {
        match q.raw(ClientType::DesktopMusic, "browse", body).await {
            Ok(json) => return Ok(json),
            Err(e) if attempt < attempts && refused(&e) => {
                log::debug!("the home feed was refused, asking again: {e}");
                let wait = std::time::Duration::from_millis(300 * u64::from(attempt));
                tokio::time::sleep(wait).await;
                attempt += 1;
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// YouTube orders the shelves differently on every load, so each is held back
/// until every one above it in `SHELVES` has arrived, and nothing shown moves.
#[derive(Debug, Clone)]
pub struct Feed {
    held: Vec<Section>,
    /// The next page's, until the last page or `CONTINUATIONS` are read.
    token: Option<String>,
    pages_left: usize,
    /// The place in `SHELVES` of the next shelf to show.
    next: usize,
    /// A visitor ID set for the first page, kept for the rest, as their
    /// tokens may be tied to it.
    visitor: Option<String>,
}

impl Feed {
    /// The first page. `q` must send the sign-in cookie: the feed is personal.
    /// `visitor` replaces one from rustypipe's pool.
    pub async fn first(q: &RustyPipeQuery, visitor: Option<&str>) -> Result<Self, String> {
        let body = serde_json::json!({ "browseId": "FEmusic_home" });
        let q = q.clone().visitor_data_opt(visitor);
        let (held, token) = first_page(&browse(&q, &body, ATTEMPTS).await?)?;
        Ok(Self {
            held,
            token,
            pages_left: CONTINUATIONS,
            next: 0,
            visitor: visitor.map(str::to_owned),
        })
    }

    pub fn done(&self) -> bool {
        self.token.is_none()
    }

    /// Reads the next page. Should it fail, the feed ends with what was read.
    pub async fn next(&mut self, q: &RustyPipeQuery) -> Result<(), String> {
        let Some(token) = self.token.take() else {
            return Ok(());
        };
        let body = serde_json::json!({ "continuation": token });
        let q = q.clone().visitor_data_opt(self.visitor.as_deref());
        let (more, token) = next_page(&browse(&q, &body, ATTEMPTS).await?)?;
        self.held.extend(more);
        self.pages_left -= 1;
        self.token = token.filter(|_| self.pages_left > 0);
        Ok(())
    }

    /// The shelves whose betters in `SHELVES` are all in. Should none come, as
    /// when YouTube renames them, the feed is kept whole rather than left empty.
    pub fn release(&mut self) -> Vec<Section> {
        let done = self.done();
        let wanted = |s: &Section| SHELVES.contains(&s.title());
        let mut out = if done && self.next == 0 && !self.held.iter().any(wanted) {
            std::mem::take(&mut self.held)
        } else {
            let mut out = Vec::new();
            while let Some(name) = SHELVES.get(self.next) {
                match self.held.iter().position(|s| s.title() == *name) {
                    Some(i) => out.push(self.held.remove(i)),
                    None if !done => break,
                    None => {}
                }
                self.next += 1;
            }
            out
        };
        // Once one has shown the feed won't be kept whole, so hold only the
        // shelves still to come, and read no further page once all have.
        if self.next > 0 {
            let rest = &SHELVES[self.next..];
            self.held.retain(|s| rest.contains(&s.title()));
            if rest.is_empty() {
                self.token = None;
            }
        }
        // List shelves such as Quick picks play with a radio after them, as
        // YouTube Music does, rather than the shelf's ten on their own.
        for section in &mut out {
            if let Section::Tracks { mode, .. } = section {
                *mode = QueueMode::Radio;
            }
        }
        out
    }
}

/// A whole feed, such as the saved one, as `Feed::release` shows it.
pub fn arrange(sections: Vec<Section>) -> Vec<Section> {
    let mut feed = Feed {
        held: sections,
        token: None,
        pages_left: 0,
        next: 0,
        visitor: None,
    };
    feed.release()
}

/// Mixes the latest of `recent`, songs played newest first, into Listen
/// again, keeping its length, or makes the shelf of them when the feed has
/// none. A song YouTube shows already stays where it is.
pub fn add_recent<'a>(sections: &mut Vec<Section>, recent: impl Iterator<Item = &'a Track>) {
    fn card(track: &Track) -> Card {
        Card {
            kind: CardKind::Song,
            id: track.id.clone(),
            title: track.title.clone(),
            subtitle: track.artist_names(),
            thumb: track
                .thumb
                .as_deref()
                .map(|url| resize_thumb(url, CARD_THUMB)),
            color: None,
            track: Some(track.clone()),
        }
    }
    match sections.iter_mut().find(|s| s.title() == LISTEN_AGAIN) {
        Some(Section::Cards { cards, .. }) => {
            let len = cards.len();
            // Slots first, so a card is made only for a slot within the shelf.
            let slots = (RECENT_EVERY..len).step_by(RECENT_EVERY + 1).take(RECENT);
            let fresh: Vec<(usize, Card)> = slots
                .zip(recent.filter(|t| !cards.iter().any(|c| c.id == t.id)))
                .map(|(slot, track)| (slot, card(track)))
                .collect();
            for (slot, card) in fresh {
                cards.insert(slot, card);
            }
            cards.truncate(len);
        }
        Some(Section::Tracks { .. }) => {}
        None => {
            let cards: Vec<Card> = recent.take(RECENT).map(card).collect();
            if !cards.is_empty() {
                sections.insert(
                    0,
                    Section::Cards {
                        title: LISTEN_AGAIN.to_owned(),
                        cards,
                        layout: CardLayout::Carousel,
                    },
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_only_refusals() {
        use rustypipe::error::Error;
        assert!(refused(&Error::HttpStatus(400, "invalid argument".into())));
        assert!(!refused(&Error::HttpStatus(401, "".into())));
        assert!(!refused(&Error::HttpStatus(404, "".into())));
        assert!(!refused(&Error::Other("network".into())));
    }

    fn track(id: &str) -> Track {
        Track {
            id: id.to_owned(),
            title: id.to_owned(),
            ..Track::default()
        }
    }

    fn shelf(title: &str, ids: &[&str]) -> Section {
        Section::Cards {
            title: title.to_owned(),
            cards: ids
                .iter()
                .map(|id| Card {
                    kind: CardKind::Song,
                    id: (*id).to_owned(),
                    title: (*id).to_owned(),
                    subtitle: String::new(),
                    thumb: None,
                    color: None,
                    track: Some(track(id)),
                })
                .collect(),
            layout: CardLayout::Carousel,
        }
    }

    fn ids(section: &Section) -> Vec<&str> {
        match section {
            Section::Cards { cards, .. } => cards.iter().map(|c| c.id.as_str()).collect(),
            Section::Tracks { tracks, .. } => tracks.iter().map(|t| t.id.as_str()).collect(),
        }
    }

    fn titles(sections: &[Section]) -> Vec<&str> {
        sections.iter().map(Section::title).collect()
    }

    fn feed(held: Vec<Section>) -> Feed {
        Feed {
            held,
            token: Some("next".to_owned()),
            pages_left: 3,
            next: 0,
            visitor: None,
        }
    }

    #[test]
    fn releases_shelves_from_the_top_down() {
        let mut feed = feed(vec![
            shelf("Forgotten favorites", &["f"]),
            shelf("Recaps", &["r"]),
            shelf("Listen again", &["a"]),
        ]);
        let mut shown = feed.release();
        assert_eq!(
            titles(&shown),
            ["Listen again"],
            "Forgotten favorites waits for Quick picks"
        );
        assert_eq!(
            titles(&feed.held),
            ["Forgotten favorites"],
            "a shelf Home will not show is let go once one has shown"
        );

        let quick_picks = Section::Tracks {
            title: "Quick picks".to_owned(),
            tracks: vec![track("q")],
            mode: QueueMode::Section,
            numbered: false,
        };
        feed.held.extend([
            shelf("Mixed for you", &["m"]),
            quick_picks,
            shelf("Listen again", &["b"]),
        ]);
        shown.extend(feed.release());
        assert_eq!(
            titles(&shown),
            ["Listen again", "Quick picks", "Forgotten favorites"],
            "Mixed for you waits for Trending songs for you, which may yet come"
        );
        assert!(
            matches!(
                shown[1],
                Section::Tracks {
                    mode: QueueMode::Radio,
                    ..
                }
            ),
            "a Quick pick plays with a radio after it"
        );

        feed.token = None;
        shown.extend(feed.release());
        assert_eq!(
            titles(&shown),
            [
                "Listen again",
                "Quick picks",
                "Forgotten favorites",
                "Mixed for you"
            ],
            "the rest once the feed ends"
        );
        assert_eq!(ids(&shown[0]), ["a"], "the first of two with one title");
    }

    #[test]
    fn keeps_a_renamed_feed_whole_once_it_ends() {
        let mut feed = feed(vec![shelf("Neu", &["n"]), shelf("Alt", &["o"])]);
        assert!(!feed.done());
        assert!(
            feed.release().is_empty(),
            "nothing yet: Listen again may come"
        );
        feed.token = None;
        assert_eq!(titles(&feed.release()), ["Neu", "Alt"]);
    }

    #[test]
    fn stops_reading_once_every_shelf_has_shown() {
        let mut feed = feed(SHELVES.iter().map(|name| shelf(name, &["x"])).collect());
        assert_eq!(feed.release().len(), SHELVES.len());
        assert!(feed.done(), "no page could add a shelf");
    }

    #[test]
    fn arranges_a_whole_feed() {
        let arranged = arrange(vec![
            shelf("Quick picks", &["q"]),
            shelf("Recaps", &["r"]),
            shelf("Listen again", &["a"]),
        ]);
        assert_eq!(titles(&arranged), ["Listen again", "Quick picks"]);
    }

    #[test]
    fn listen_again_mixes_in_recent_plays() {
        let mut sections = vec![
            shelf("Listen again", &["a", "b", "c", "d", "e", "f", "g"]),
            shelf("Mixed for you", &["m"]),
        ];
        let recent = [track("c"), track("x"), track("y"), track("z")];
        add_recent(&mut sections, recent.iter());
        assert_eq!(
            ids(&sections[0]),
            ["a", "b", "c", "x", "d", "e", "f"],
            "YouTube's lead, one recent play after every three, c left where it was, length kept"
        );
        assert_eq!(ids(&sections[1]), ["m"]);

        let youtube: Vec<String> = (0..19).map(|i| i.to_string()).collect();
        let youtube: Vec<&str> = youtube.iter().map(String::as_str).collect();
        let mut sections = vec![shelf("Listen again", &youtube)];
        let recent: Vec<Track> = ["r1", "r2", "r3", "r4", "r5"].map(track).into();
        add_recent(&mut sections, recent.iter());
        let shown = ids(&sections[0]);
        assert_eq!(shown.len(), 19);
        let at: Vec<usize> = (0..19).filter(|&i| shown[i].starts_with('r')).collect();
        assert_eq!(at, [3, 7, 11, 15], "as many as fit, never first");

        let mut sections = vec![shelf("Quick picks", &["q"])];
        let recent = [track("c"), track("d")];
        add_recent(&mut sections, recent.iter());
        assert_eq!(
            sections[0].title(),
            "Listen again",
            "made when the feed has none"
        );
        assert_eq!(ids(&sections[0]), ["c", "d"]);

        let mut sections = vec![shelf("Listen again", &["a"])];
        add_recent(&mut sections, std::iter::empty());
        assert_eq!(ids(&sections[0]), ["a"], "unchanged with nothing played");
    }
}
