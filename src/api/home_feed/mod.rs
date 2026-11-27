//! The personal home feed (`FEmusic_home`), which rustypipe has no model for.
//! Shelves are held back until every one ranked above them has arrived.

mod parse;

use rustypipe::client::{ClientType, RustyPipeQuery};

use super::{Card, CardLayout, QueueMode, Section, Track};
use parse::{Shelves, first_page, next_page};

/// Continuations fetched at most after the first page; the whole feed is
/// five or six pages.
const CONTINUATIONS: usize = 8;
/// Pages after the first asked for together, the feed's usual rest.
/// `Feed::next` sends exactly four.
const AT_ONCE: usize = 4;
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

fn varint(bytes: &[u8], at: &mut usize) -> Option<u64> {
    let mut value = 0;
    for shift in (0..64).step_by(7) {
        let byte = *bytes.get(*at)?;
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            return Some(value);
        }
    }
    None
}

/// Where protobuf field `number`'s value lies.
fn field(bytes: &[u8], number: u64) -> Option<std::ops::Range<usize>> {
    let mut at = 0;
    while at < bytes.len() {
        let key = varint(bytes, &mut at)?;
        let start = at;
        let range = match key & 7 {
            0 => {
                varint(bytes, &mut at)?;
                start..at
            }
            1 => start..start.checked_add(8)?,
            2 => {
                let len = usize::try_from(varint(bytes, &mut at)?).ok()?;
                at..at.checked_add(len)?
            }
            5 => start..start.checked_add(4)?,
            _ => return None,
        };
        if range.end > bytes.len() {
            return None;
        }
        at = range.end;
        if key >> 3 == number {
            return Some(range);
        }
    }
    None
}

/// `text` decoded by the base64 engine that encodes it back exactly.
fn unbase64(text: &[u8]) -> Option<(Vec<u8>, &'static base64::engine::GeneralPurpose)> {
    use base64::{Engine as _, engine::general_purpose as gp};
    [
        &gp::STANDARD,
        &gp::STANDARD_NO_PAD,
        &gp::URL_SAFE,
        &gp::URL_SAFE_NO_PAD,
    ]
    .into_iter()
    .find_map(|engine| {
        let bytes = engine.decode(text).ok()?;
        (engine.encode(&bytes).as_bytes() == text).then_some((bytes, engine))
    })
}

/// The first page's continuation token and the next three pages'. YouTube
/// keeps the feed as a snapshot, and its pages' tokens differ only in the
/// shelf they start from: field 1 inside field 3 of field 80226972, and that
/// plus one in field 318126543 inside field 15, each layer base64.
fn later_tokens(token: &str) -> Option<[String; AT_ONCE]> {
    use base64::Engine as _;
    let (outer, outer_b64) = unbase64(token.replace("%3D", "=").as_bytes())?;
    let browse = field(&outer, 80_226_972)?;
    let text = field(&outer[browse.clone()], 3)?;
    let text = browse.start + text.start..browse.start + text.end;
    let (inner, inner_b64) = unbase64(&outer[text.clone()])?;
    let first = field(&inner, 1)?;
    let nested_text = field(&inner, 15)?;
    let (nested, nested_b64) = unbase64(&inner[nested_text.clone()])?;
    let next = field(&nested, 318_126_543)?;
    let value = field(&nested[next.clone()], 1)?;
    let value = next.start + value.start..next.start + value.end;
    // The second page starts where the first ended: the page length.
    let (&[step], &[after]) = (&inner[first.clone()], &nested[value.clone()]) else {
        return None;
    };
    // Bound first, so `step + 1` can't overflow.
    if step == 0 || usize::from(step) * AT_ONCE + 1 >= 0x80 || after != step + 1 {
        return None;
    }
    let tokens: [String; AT_ONCE] = std::array::from_fn(|page| {
        let shelf = step * (page as u8 + 1);
        let (mut outer, mut inner, mut nested) = (outer.clone(), inner.clone(), nested.clone());
        nested[value.start] = shelf + 1;
        inner[first.start] = shelf;
        inner[nested_text.clone()].copy_from_slice(nested_b64.encode(&nested).as_bytes());
        outer[text.clone()].copy_from_slice(inner_b64.encode(&inner).as_bytes());
        outer_b64.encode(&outer).replace('=', "%3D")
    });
    (tokens[0] == token).then_some(tokens)
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

    /// Reads the next page, or after the first the next `AT_ONCE` together.
    /// Should the next fail, the feed ends with what was read.
    pub async fn next(&mut self, q: &RustyPipeQuery) -> Result<(), String> {
        let Some(token) = self.token.take() else {
            return Ok(());
        };
        let q = q.clone().visitor_data_opt(self.visitor.as_deref());
        let page = |token: String, attempts| {
            let q = &q;
            async move {
                let body = serde_json::json!({ "continuation": token });
                next_page(&browse(q, &body, attempts).await?)
            }
        };
        // Only the first page's token gives the page length.
        let tokens = (self.pages_left == CONTINUATIONS)
            .then(|| later_tokens(&token))
            .flatten();
        let Some(tokens) = tokens else {
            self.add(page(token, ATTEMPTS).await?);
            return Ok(());
        };
        // Made pages retry once: the batch waits for its slowest, and a
        // failed one is read again later.
        let [a, b, c, d] = tokens.clone();
        let (a, b, c, d) = tokio::join!(page(a, ATTEMPTS), page(b, 2), page(c, 2), page(d, 2));
        self.add_batch(&tokens, [a, b, c, d])
    }

    /// Adds a batch's pages in order, stopping at one that failed or led
    /// elsewhere; the rest is then read one by one.
    fn add_batch(
        &mut self,
        tokens: &[String],
        answers: impl IntoIterator<Item = Shelves>,
    ) -> Result<(), String> {
        for (i, answer) in answers.into_iter().enumerate() {
            match answer {
                Ok(answer) => self.add(answer),
                Err(e) if i == 0 => return Err(e),
                Err(e) => {
                    log::debug!("a later page of the home feed failed: {e}");
                    break;
                }
            }
            // Leading nowhere ends the feed; elsewhere, the tokens were wrong.
            if let Some(next) = tokens.get(i + 1)
                && self.token.as_ref() != Some(next)
            {
                if self.token.is_some() {
                    log::debug!("the home feed's tokens no longer match the pages made");
                }
                break;
            }
        }
        Ok(())
    }

    fn add(&mut self, (more, token): (Vec<Section>, Option<String>)) {
        self.held.extend(more);
        self.pages_left -= 1;
        self.token = token.filter(|_| self.pages_left > 0);
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
        // YouTube Music does, rather than the shelf's songs on their own.
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
    match sections.iter_mut().find(|s| s.title() == LISTEN_AGAIN) {
        Some(Section::Cards { cards, .. }) => {
            let len = cards.len();
            // Slots first, so a card is made only for a slot within the shelf.
            let slots = (RECENT_EVERY..len).step_by(RECENT_EVERY + 1).take(RECENT);
            let fresh: Vec<(usize, Card)> = slots
                .zip(recent.filter(|t| !cards.iter().any(|c| c.id == t.id)))
                .map(|(slot, track)| (slot, Card::song(track)))
                .collect();
            for (slot, card) in fresh {
                cards.insert(slot, card);
            }
            cards.truncate(len);
        }
        Some(Section::Tracks { .. }) => {}
        None => {
            let cards: Vec<Card> = recent.take(RECENT).map(Card::song).collect();
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
    use crate::api::CardKind;

    /// A first page's token in YouTube's layout.
    fn feed_token(shelf: u8) -> String {
        use base64::{Engine as _, engine::general_purpose as gp};
        fn push_varint(mut v: u64, bytes: &mut Vec<u8>) {
            while v >= 0x80 {
                bytes.push(v as u8 | 0x80);
                v >>= 7;
            }
            bytes.push(v as u8);
        }
        fn len_field(field: u64, value: &[u8]) -> Vec<u8> {
            let mut bytes = Vec::new();
            push_varint(field << 3 | 2, &mut bytes);
            push_varint(value.len() as u64, &mut bytes);
            bytes.extend_from_slice(value);
            bytes
        }
        // Fields to step over: varint, fixed32, fixed64.
        let mut nested = vec![
            0x18, 0xac, 0x02, 0x25, 1, 2, 3, 4, 0x29, 1, 2, 3, 4, 5, 6, 7, 8,
        ];
        nested.extend(len_field(11, b"snapshot_a-c"));
        nested.extend(len_field(318_126_543, &[0x08, shelf + 1]));
        let mut inner = vec![0x08, shelf];
        inner.extend(len_field(15, gp::URL_SAFE_NO_PAD.encode(nested).as_bytes()));
        let mut browse = len_field(2, b"FEmusic_home");
        browse.extend(len_field(3, gp::STANDARD.encode(inner).as_bytes()));
        gp::STANDARD
            .encode(len_field(80_226_972, &browse))
            .replace('=', "%3D")
    }

    #[test]
    fn makes_the_later_pages_tokens() {
        let token = feed_token(3);
        assert!(token.ends_with("%3D"), "padded, as YouTube's are");
        let tokens = later_tokens(&token).expect("laid out as YouTube's");
        assert_eq!(tokens, [3, 6, 9, 12].map(feed_token));
        assert_eq!(later_tokens("not a token"), None);
        // Short of the padding, which is optional.
        for len in 0..token.len() - "%3D".len() {
            assert_eq!(later_tokens(&token[..len]), None, "cut at {len}");
        }
        assert_eq!(later_tokens(&feed_token(0)), None, "no page length");
        assert_eq!(later_tokens(&feed_token(40)), None, "past one byte");
    }

    #[test]
    fn adds_a_batch_up_to_where_it_goes_wrong() {
        let tokens = ["t1", "t2", "t3", "t4"].map(str::to_owned);
        let page = |title: &str, next: Option<&str>| -> Shelves {
            Ok((vec![shelf(title, &[title])], next.map(str::to_owned)))
        };
        let read = |answers: [Shelves; 4]| {
            let mut feed = feed(Vec::new());
            // As `next` leaves it.
            feed.pages_left = CONTINUATIONS;
            feed.token = None;
            let ok = feed.add_batch(&tokens, answers).is_ok();
            let held = titles(&feed.held).join(" ");
            (ok, held, feed.token, CONTINUATIONS - feed.pages_left)
        };
        let token = |t: &str| Some(t.to_owned());

        let all = [
            page("a", Some("t2")),
            page("b", Some("t3")),
            page("c", Some("t4")),
            page("d", Some("t5")),
        ];
        assert_eq!(
            read(all),
            (true, "a b c d".into(), token("t5"), 4),
            "read on from the last"
        );

        let ended = [
            page("a", Some("t2")),
            page("b", None),
            page("", None),
            page("", None),
        ];
        assert_eq!(read(ended), (true, "a b".into(), None, 2), "the feed ended");

        let failed = [
            page("a", Some("t2")),
            page("b", Some("t3")),
            Err("refused".to_owned()),
            page("d", None),
        ];
        assert_eq!(
            read(failed),
            (true, "a b".into(), token("t3"), 2),
            "the failed page read again"
        );

        let elsewhere = [
            page("a", Some("other")),
            page("wrong", Some("t3")),
            page("wrong", Some("t4")),
            page("wrong", None),
        ];
        assert_eq!(
            read(elsewhere),
            (true, "a".into(), token("other"), 1),
            "made tokens that lead elsewhere are dropped"
        );

        let first_failed = [
            Err("refused".to_owned()),
            page("b", Some("t3")),
            page("c", Some("t4")),
            page("d", None),
        ];
        assert_eq!(read(first_failed), (false, String::new(), None, 0));
    }

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
