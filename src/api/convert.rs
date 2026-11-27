use std::ptr;

use rustypipe::model::{
    AlbumItem, AlbumType, ArtistId, ArtistItem, MusicGenreItem, MusicPlaylistItem, Thumbnail,
    TrackItem, TrackType,
};
use windows_sys::Win32::Globalization::{
    LCMAP_SORTKEY, LCMapStringEx, LINGUISTIC_IGNORECASE, LINGUISTIC_IGNOREDIACRITIC,
    LOCALE_NAME_INVARIANT, NORM_IGNOREWIDTH, SORT_DIGITSASNUMBERS,
};

use super::{
    ArtistRef, Card, CardKind, CardLayout, QueueMode, Section, Track,
    format::{compact, songs},
};

pub const ROW_THUMB: u32 = 120;
pub const CARD_THUMB: u32 = 240;
pub const HEADER_THUMB: u32 = 544;

pub fn join_artists(artists: &[ArtistRef]) -> String {
    artists
        .iter()
        .map(|a| a.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn pick_thumb(thumbs: &[Thumbnail], edge: u32) -> Option<String> {
    fit_thumb(thumbs.iter().map(|t| (t.width, t.url.as_str())), edge)
}

/// Of thumbnails given as (width, URL), the smallest at least `edge`
/// pixels wide, else the largest, sized to `edge`.
pub fn fit_thumb<'a>(
    thumbs: impl Iterator<Item = (u32, &'a str)> + Clone,
    edge: u32,
) -> Option<String> {
    let (_, url) = thumbs
        .clone()
        .filter(|(width, _)| *width >= edge)
        .min_by_key(|(width, _)| *width)
        .or_else(|| thumbs.max_by_key(|(width, _)| *width))?;
    Some(resize_thumb(url, edge))
}

/// googleusercontent thumbnails render at any size given in the URL suffix.
pub fn resize_thumb(url: &str, edge: u32) -> String {
    if url.contains("googleusercontent.com")
        && let Some(eq) = url.rfind('=')
        && url[eq + 1..].starts_with('w')
    {
        return format!("{}=w{edge}-h{edge}-l90-rj", &url[..eq]);
    }
    url.to_owned()
}

pub fn artist_refs(artists: &[ArtistId]) -> Vec<ArtistRef> {
    artists
        .iter()
        .map(|a| ArtistRef {
            id: a.id.clone(),
            name: a.name.clone(),
        })
        .collect()
}

pub fn track(t: TrackItem) -> Track {
    Track {
        video: t.track_type == TrackType::Video,
        thumb: pick_thumb(&t.cover, ROW_THUMB),
        artists: artist_refs(&t.artists),
        album: t.album.map(|a| (a.id, a.name)),
        duration: t.duration,
        track_nr: t.track_nr,
        id: t.id,
        title: t.name,
        set_video_id: None,
    }
}

pub fn tracks(items: Vec<TrackItem>) -> Vec<Track> {
    items.into_iter().map(track).collect()
}

/// A playlist's tracks, with each entry's ID only in the account's own, which
/// edits take.
pub fn playlist_tracks(items: Vec<TrackItem>, own: bool) -> Vec<Track> {
    items
        .into_iter()
        .map(|mut t| Track {
            set_video_id: t.set_video_id.take().filter(|_| own),
            ..track(t)
        })
        .collect()
}

pub fn album_type(t: AlbumType) -> &'static str {
    match t {
        AlbumType::Album => "Album",
        AlbumType::Ep => "EP",
        AlbumType::Single => "Single",
        AlbumType::Audiobook => "Audiobook",
        AlbumType::Show => "Podcast",
        _ => "Album",
    }
}

pub fn album_card(a: AlbumItem) -> Card {
    let mut subtitle = vec![album_type(a.album_type).to_owned()];
    let artists = join_artists(&artist_refs(&a.artists));
    if !artists.is_empty() {
        subtitle.push(artists);
    }
    if let Some(year) = a.year {
        subtitle.push(year.to_string());
    }
    Card {
        kind: CardKind::Album,
        thumb: pick_thumb(&a.cover, CARD_THUMB),
        id: a.id,
        title: a.name,
        subtitle: subtitle.join(" • "),
        color: None,
        track: None,
    }
}

/// Library's section of saved albums.
pub(super) const ALBUMS: &str = "Albums";

/// Library's saved albums, should `sections` hold them.
pub(super) fn saved_albums(sections: &mut [Section]) -> Option<&mut Vec<Card>> {
    sections.iter_mut().find_map(|s| match s {
        Section::Cards { title, cards, .. } if title == ALBUMS => Some(cards),
        _ => None,
    })
}

/// Library's albums by artist, then title, as Windows sorts the user's
/// language; those by no one named last, ties as they came.
pub fn sort_by_artist(sections: &mut [Section]) {
    if let Some(cards) = saved_albums(sections) {
        cards.sort_by_cached_key(|c| {
            let artist = album_artist(&c.subtitle);
            (artist.is_empty(), sort_key(artist), sort_key(&c.title))
        });
    }
}

/// `text`'s sort key for the user's language, whose bytes compare as Windows
/// compares the text; empty should Windows refuse it.
fn sort_key(text: &str) -> Vec<u8> {
    let flags = LCMAP_SORTKEY
        | LINGUISTIC_IGNORECASE
        | LINGUISTIC_IGNOREDIACRITIC
        | NORM_IGNOREWIDTH
        | SORT_DIGITSASNUMBERS;
    map_string(text, flags, false)
}

/// What `map_string` fills: a sort key's bytes, or UTF-16 units.
pub(super) trait MapUnit: Copy + Default {}
impl MapUnit for u8 {}
impl MapUnit for u16 {}

/// `text` mapped by Windows with `flags`, for the invariant locale or the
/// user's; empty should Windows refuse it.
pub(super) fn map_string<T: MapUnit>(text: &str, flags: u32, invariant: bool) -> Vec<T> {
    let wide: Vec<u16> = text.encode_utf16().collect();
    let Ok(len) = i32::try_from(wide.len()) else {
        return Vec::new();
    };
    if len == 0 {
        return Vec::new();
    }
    let locale = if invariant {
        LOCALE_NAME_INVARIANT
    } else {
        ptr::null()
    };
    let unit = if flags & LCMAP_SORTKEY == 0 { 2 } else { 1 };
    let map = |out: &mut [T]| {
        let Ok(capacity) = i32::try_from(size_of_val(out) / unit) else {
            return 0;
        };
        // SAFETY: `wide` holds `len` units and `out` `capacity` of what the
        // flags count (bytes for a sort key, else units), both borrowed through
        // the call, which writes at most `capacity` and none when it is 0. The
        // locale is null (the user's) or a static null-terminated string.
        unsafe {
            LCMapStringEx(
                locale,
                flags,
                wide.as_ptr(),
                len,
                out.as_mut_ptr().cast(),
                capacity,
                ptr::null(),
                ptr::null(),
                0,
            )
        }
    };
    let mut out = vec![T::default(); usize::try_from(map(&mut [])).unwrap_or(0)];
    let written = usize::try_from(map(&mut out)).unwrap_or(0);
    if written > out.len() {
        return Vec::new();
    }
    out.truncate(written);
    out
}

/// From `album_card`'s "Album • Artist • 2024": what lies between the type,
/// which always leads, and any year.
// shortcut: a four-digit artist without a year reads as one; Card has no artist.
fn album_artist(subtitle: &str) -> &str {
    let is_year = |s: &str| s.len() == 4 && s.bytes().all(|b| b.is_ascii_digit());
    let Some((_, rest)) = subtitle.split_once(" • ") else {
        return "";
    };
    match rest.rsplit_once(" • ") {
        Some((artist, year)) if is_year(year) => artist,
        None if is_year(rest) => "",
        _ => rest,
    }
}

pub fn artist_card(a: ArtistItem) -> Card {
    Card {
        kind: CardKind::Artist,
        thumb: pick_thumb(&a.avatar, CARD_THUMB),
        id: a.id,
        title: a.name,
        subtitle: a.subscriber_count.map_or_else(
            || "Artist".to_owned(),
            |n| format!("{} subscribers", compact(n)),
        ),
        color: None,
        track: None,
    }
}

pub fn playlist_card(p: MusicPlaylistItem) -> Card {
    let mut subtitle = vec!["Playlist".to_owned()];
    if let Some(channel) = &p.channel {
        subtitle.push(channel.name.clone());
    }
    if let Some(n) = p.track_count {
        subtitle.push(songs(n));
    }
    Card {
        kind: CardKind::Playlist,
        thumb: pick_thumb(&p.thumbnail, CARD_THUMB),
        id: p.id,
        title: p.name,
        subtitle: subtitle.join(" • "),
        color: None,
        track: None,
    }
}

pub fn genre_card(g: MusicGenreItem) -> Card {
    Card {
        kind: CardKind::Genre,
        id: g.id,
        title: g.name,
        subtitle: String::new(),
        thumb: None,
        color: Some(g.color),
        track: None,
    }
}

pub fn track_section(title: &str, tracks: Vec<Track>, mode: QueueMode) -> Option<Section> {
    (!tracks.is_empty()).then(|| Section::Tracks {
        title: title.to_owned(),
        tracks,
        mode,
        numbered: false,
    })
}

pub fn card_section(title: &str, cards: Vec<Card>, layout: CardLayout) -> Option<Section> {
    (!cards.is_empty()).then(|| Section::Cards {
        title: title.to_owned(),
        cards,
        layout,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Thumbnail is non-exhaustive, so build it the way rustypipe does.
    fn thumb(url: &str, width: u32) -> Thumbnail {
        serde_json::from_value(serde_json::json!({ "url": url, "width": width, "height": width }))
            .unwrap()
    }

    #[test]
    fn picks_smallest_sufficient_thumbnail() {
        let thumbs = [thumb("a", 60), thumb("b", 226), thumb("c", 544)];
        assert_eq!(pick_thumb(&thumbs, 120).as_deref(), Some("b"));
        assert_eq!(pick_thumb(&thumbs, 1000).as_deref(), Some("c"));
        assert_eq!(pick_thumb(&[], 120), None);
    }

    #[test]
    fn resizes_googleusercontent_thumbnails() {
        assert_eq!(
            resize_thumb("https://lh3.googleusercontent.com/abc=w60-h60-l90-rj", 360),
            "https://lh3.googleusercontent.com/abc=w360-h360-l90-rj"
        );
        let ytimg = "https://i.ytimg.com/vi/x/hqdefault.jpg";
        assert_eq!(resize_thumb(ytimg, 360), ytimg);
    }

    #[test]
    fn orders_albums_by_artist_then_title() {
        let card = |kind, id: &str, title: &str, subtitle: &str| Card {
            subtitle: subtitle.to_owned(),
            ..Card::bare(kind, id, title)
        };
        let album =
            |id: &str, subtitle: &str| card(CardKind::Album, id, &format!("Album {id}"), subtitle);
        fn ids(section: &Section) -> Vec<&str> {
            match section {
                Section::Cards { cards, .. } => cards.iter().map(|c| c.id.as_str()).collect(),
                Section::Tracks { .. } => Vec::new(),
            }
        }
        let others = card_section(
            "Albums for you",
            vec![album("z", "Album • Zed"), album("y", "Album • Abba")],
            CardLayout::Grid,
        );
        let albums = card_section(
            ALBUMS,
            vec![
                album("c", "Album • 2015"),
                album("d", "EP • Tame Impala • 2019"),
                album("a", "Single • Beck"),
                card(CardKind::Album, "j", "Vol 10", "Album • Beck"),
                card(CardKind::Album, "i", "Vol 2", "Album • Beck"),
                album("b", "Album • Tame Impala • 2015"),
                album("e", "Album • air • 1998"),
                card(CardKind::Playlist, "p", "Mix", "Playlist • Zed"),
                card(CardKind::Album, "g", "lemonade", "Album • BEYONCE"),
                card(CardKind::Album, "f", "Lemonade", "Album • Beyoncé • 2016"),
                album("h", "Album • Tyler, The Creator & Kali Uchis • 2017"),
            ],
            CardLayout::Grid,
        );
        let mut sections: Vec<Section> = others.into_iter().chain(albums).collect();
        sort_by_artist(&mut sections);
        assert_eq!(ids(&sections[0]), ["z", "y"], "only Library's own albums");
        assert_eq!(
            ids(&sections[1]),
            ["e", "a", "i", "j", "g", "f", "b", "d", "h", "p", "c"],
            "by artist whatever the case or accents, then title with numbers by value, ties as \
             they came, a stray card too, and no artist last"
        );
    }

    #[test]
    fn keeps_entry_ids_only_from_own_playlists() {
        let item: TrackItem = serde_json::from_value(serde_json::json!({
            "id": "3_R4ulvg8OY", "name": "I Hate Everything", "duration": 83, "cover": [],
            "artists": [], "artist_id": null, "album": null, "view_count": null,
            "track_type": "track", "track_nr": null, "by_va": false,
            "set_video_id": "56B44F6D10557CC6"
        }))
        .unwrap();
        let own = playlist_tracks(vec![item.clone()], true);
        assert_eq!(own[0].set_video_id.as_deref(), Some("56B44F6D10557CC6"));
        let other = playlist_tracks(vec![item.clone()], false);
        assert_eq!(other[0].set_video_id, None);
        assert_eq!(track(item).set_video_id, None);
        // Saved queues and history never carry it.
        assert!(
            !serde_json::to_string(&own[0])
                .unwrap()
                .contains("set_video_id")
        );
    }
}
