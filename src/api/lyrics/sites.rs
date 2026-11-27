//! Lyrics sites other than YouTube, each a miss on any failure, as YouTube's
//! own lyrics stand without them.

use std::borrow::Cow;

use serde::Deserialize;
use serde_json::Value;

use super::{Lyrics, formats, matching};
use crate::api::Track;

pub(super) const LRC_RED: &str = "Source: lrc.red";
pub(super) const KUGOU: &str = "Source: KuGou";
const LRCLIB: &str = "Source: LRCLIB";
/// Larger answers are no lyrics.
pub(super) const MAX_BODY: usize = 4 << 20;

/// `None` on any failure, a 404 included, which these sites answer a miss with.
async fn get(host: &str, path: &str, query: &[(&str, &str)]) -> Option<Vec<u8>> {
    let sent = crate::http::lyrics_client()
        .get(format!("https://{host}{path}"))
        .query(query)
        .send()
        .await;
    read(
        sent.map_err(|e| log::debug!("lyrics from {host}: {e}"))
            .ok()?,
    )
    .await
}

/// The body of a successful answer, read up to `MAX_BODY`.
pub(super) async fn read(response: reqwest::Response) -> Option<Vec<u8>> {
    let response = response
        .error_for_status()
        .map_err(|e| log::debug!("lyrics: {e}"))
        .ok()?;
    crate::http::body(response, MAX_BODY).await.ok()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    track_name: String,
    artist_name: String,
    duration: Option<f64>,
    instrumental: Option<bool>,
    synced_lyrics: Option<String>,
    plain_lyrics: Option<String>,
}

/// LRCLIB's community lyrics, found by name and length, as it knows no video
/// IDs.
// shortcut: line timing only; its `lyricsfile` has word timing for many
// songs, worth reading should lrc.red go quiet.
pub(super) async fn lrclib(track: &Track) -> Option<Lyrics> {
    let artist = matching::artist(track)?;
    let duration = track.duration?.to_string();
    let title = matching::title(&track.title);
    let named = [("track_name", title.as_str()), ("artist_name", &artist)];
    let exact = get(
        "lrclib.net",
        "/api/get",
        &[named[0], named[1], ("duration", &duration)],
    )
    .await;
    if let Some(found) = exact.and_then(|data| record(serde_json::from_slice(&data).ok()?, track)) {
        return Some(found);
    }
    // The exact lookup wants the length to the second, which a record a little
    // off misses.
    let records: Vec<Record> =
        serde_json::from_slice(&get("lrclib.net", "/api/search", &named).await?).ok()?;
    searched(records, track)
}

/// The first timed record of the track's song, else the first untimed one.
fn searched(records: Vec<Record>, track: &Track) -> Option<Lyrics> {
    let mut plain = None;
    for found in records.into_iter().filter_map(|r| record(r, track)) {
        if found.is_synced() {
            return Some(found);
        }
        plain = plain.or(Some(found));
    }
    plain
}

/// `None` unless the record is the track's song and has words: LRCLIB gives
/// its nearest record, which can be another song altogether.
fn record(record: Record, track: &Track) -> Option<Lyrics> {
    let song = matching::same_song(
        track,
        &record.track_name,
        &record.artist_name,
        record.duration,
    );
    if !song || record.instrumental == Some(true) {
        return None;
    }
    lrclib_lyrics(
        record.synced_lyrics.as_deref(),
        record.plain_lyrics.as_deref(),
    )
}

/// The timed lyrics, else the text if there is any.
pub(super) fn lrclib_lyrics(synced: Option<&str>, plain: Option<&str>) -> Option<Lyrics> {
    synced
        .and_then(|synced| formats::lrc(synced, LRCLIB))
        .or_else(|| {
            let plain = plain.filter(|text| !text.trim().is_empty())?;
            Some(Lyrics::plain(plain, LRCLIB.to_owned()))
        })
}

/// Apple Music's lyrics, often word by word, as lrc.red keeps them by ISRC.
pub(super) async fn lrc_red(track: &Track) -> Option<Lyrics> {
    let artist = matching::artist(track)?;
    let title = matching::title(&track.title);
    let mut isrc = lrc_red_search(&title, &artist, track).await;
    if isrc.is_none()
        && let Some((title, artist)) = simplified(&title, &artist)
    {
        isrc = lrc_red_search(&title, &artist, track).await;
    }
    let ttml = get("lrc.red", &format!("/s/{}.ttml", isrc?), &[]).await?;
    formats::ttml(std::str::from_utf8(&ttml).ok()?, LRC_RED)
}

/// Whether `lrc_red` searches only under the names the hub is sent, with
/// nothing cut or simplified.
pub(super) fn lrc_red_searches_as_given(track: &Track) -> bool {
    let Some(artist) = matching::artist(track) else {
        return true;
    };
    let title = matching::title(&track.title);
    title == track.title && artist == track.artist_names() && simplified(&title, &artist).is_none()
}

/// The names in Simplified, as Chinese songs are filed, should that change
/// them; never with kana, as Japanese kanji aren't Chinese to simplify.
fn simplified<'a>(title: &'a str, artist: &'a str) -> Option<(Cow<'a, str>, Cow<'a, str>)> {
    let kana = |text: &str| text.chars().any(|c| ('\u{3040}'..'\u{3100}').contains(&c));
    if kana(title) || kana(artist) {
        return None;
    }
    let names = (matching::simplified(title), matching::simplified(artist));
    (names.0 != title || names.1 != artist).then_some(names)
}

async fn lrc_red_search(title: &str, artist: &str, track: &Track) -> Option<String> {
    let duration = track.duration?.to_string();
    // No album: lrc.red takes it as a filter, and YouTube's often names another
    // edition.
    let query = [
        ("track", title),
        ("artist", artist),
        ("duration", &duration),
    ];
    isrc(&get("lrc.red", "/api/v1", &query).await?, track)
}

#[derive(Deserialize)]
struct LrcRedResults {
    results: Vec<LrcRedResult>,
}

#[derive(Deserialize)]
struct LrcRedResult {
    timing_type: Option<String>,
    track_name: String,
    artist_name: String,
    duration: Option<f64>,
    isrc: Option<String>,
}

/// The ISRC of the first timed result that is the track's own recording, as
/// the search returns whatever is nearest. The path is built from a checked
/// ISRC, so nothing else can enter it.
fn isrc(data: &[u8], track: &Track) -> Option<String> {
    let found: LrcRedResults = serde_json::from_slice(data).ok()?;
    found.results.into_iter().find_map(|result| {
        let timed = result.timing_type.as_deref() != Some("none");
        let song = matching::same_song(
            track,
            &result.track_name,
            &result.artist_name,
            result.duration,
        );
        let isrc = result.isrc.filter(|_| timed && song)?.to_ascii_uppercase();
        is_isrc(&isrc).then_some(isrc)
    })
}

/// Two letters, three letters or digits, seven digits.
fn is_isrc(text: &str) -> bool {
    let b = text.as_bytes();
    b.len() == 12
        && b[..2].iter().all(u8::is_ascii_uppercase)
        && b[2..5]
            .iter()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        && b[5..].iter().all(u8::is_ascii_digit)
}

/// KuGou's lyrics, timed word by word, which fill in many Chinese and
/// Japanese songs Apple Music lacks: three requests to China, in turn.
pub(super) async fn kugou(track: &Track) -> Option<Lyrics> {
    let artist = matching::artist(track)?;
    let duration = track.duration?;
    let keyword = format!("{} {artist}", matching::title(&track.title));
    let query = [
        ("format", "json"),
        ("keyword", &keyword),
        ("page", "1"),
        ("pagesize", "20"),
        ("showtype", "1"),
    ];
    let hash = kugou_hash(
        &get("mobileservice.kugou.com", "/api/v3/search/song", &query).await?,
        track,
    )?;
    // The hash names the recording; a search by name alone often finds
    // another version's lyrics.
    let ms = (u64::from(duration) * 1000).to_string();
    let query = [
        ("ver", "1"),
        ("man", "yes"),
        ("client", "mobi"),
        ("keyword", ""),
        ("duration", &ms),
        ("hash", &hash),
    ];
    let (id, key) = kugou_candidate(&get("krcs.kugou.com", "/search", &query).await?, track)?;
    let query = [
        ("ver", "1"),
        ("client", "pc"),
        ("id", &id),
        ("accesskey", &key),
        ("fmt", "krc"),
        ("charset", "utf8"),
    ];
    let file: Value =
        serde_json::from_slice(&get("krcs.kugou.com", "/download", &query).await?).ok()?;
    formats::krc(&formats::krc_text(file.get("content")?.as_str()?)?, KUGOU)
}

#[derive(Deserialize)]
struct KuGouSearch {
    data: KuGouSongs,
}

#[derive(Deserialize)]
struct KuGouSongs {
    info: Vec<KuGouSong>,
}

#[derive(Deserialize)]
struct KuGouSong {
    songname: String,
    singername: String,
    duration: f64,
    hash: String,
}

/// The hash of the nearest-length result that is the track's own recording.
fn kugou_hash(data: &[u8], track: &Track) -> Option<String> {
    let length = f64::from(track.duration?);
    let found: KuGouSearch = serde_json::from_slice(data).ok()?;
    found
        .data
        .info
        .into_iter()
        .filter(|song| {
            !song.hash.is_empty()
                && song.hash.bytes().all(|c| c.is_ascii_hexdigit())
                && matching::same_song(track, &song.songname, &song.singername, Some(song.duration))
        })
        .min_by(|a, b| {
            (a.duration - length)
                .abs()
                .total_cmp(&(b.duration - length).abs())
        })
        .map(|song| song.hash)
}

#[derive(Deserialize)]
struct KuGouCandidates {
    candidates: Vec<KuGouCandidate>,
}

#[derive(Deserialize)]
struct KuGouCandidate {
    /// A string or a number.
    id: Value,
    accesskey: String,
    song: String,
    singer: String,
    duration: f64,
}

/// The first lyrics filed under the track's song, as the hash also brings up
/// uploads of others.
fn kugou_candidate(data: &[u8], track: &Track) -> Option<(String, String)> {
    let found: KuGouCandidates = serde_json::from_slice(data).ok()?;
    found.candidates.into_iter().find_map(|candidate| {
        let id = match candidate.id {
            Value::String(id) => id,
            Value::Number(id) => id.as_u64()?.to_string(),
            _ => return None,
        };
        let seconds = Some(candidate.duration / 1000.0);
        matching::same_song(track, &candidate.song, &candidate.singer, seconds)
            .then_some((id, candidate.accesskey))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ArtistRef;

    fn track(title: &str, artist: Option<&str>, duration: Option<u32>) -> Track {
        Track {
            id: "dummyVideo1".into(),
            title: title.into(),
            artists: artist
                .map(|name| ArtistRef {
                    id: None,
                    name: name.into(),
                })
                .into_iter()
                .collect(),
            duration,
            ..Track::default()
        }
    }

    #[tokio::test]
    async fn reads_bodies_up_to_the_cap() {
        let answer = |status: u16, body: Vec<u8>| {
            reqwest::Response::from(
                ::http::Response::builder()
                    .status(status)
                    .body(body)
                    .unwrap(),
            )
        };
        assert_eq!(
            read(answer(200, b"Alpha".into())).await,
            Some(b"Alpha".into())
        );
        assert_eq!(read(answer(404, b"Alpha".into())).await, None);
        assert_eq!(read(answer(200, vec![0; MAX_BODY + 1])).await, None);
    }

    fn lrclib_answer(lyrics: &str) -> Record {
        serde_json::from_str(&format!(
            r#"{{"id":390,"trackName":"Dummy Song","artistName":"Dummy Artist","albumName":"Dummy Album","duration":200.0,{lyrics},"hasWordSync":false,"lyricsfile":null}}"#
        ))
        .unwrap()
    }

    fn dummy() -> Track {
        track("Dummy Song", Some("Dummy Artist"), Some(200))
    }

    #[test]
    fn lrclib_prefers_synced_lyrics() {
        let answer = lrclib_answer(
            r#""instrumental":false,"plainLyrics":"Alpha\nBeta","syncedLyrics":"[00:13.13]Alpha\n[00:16.56]\n[00:27.16]Beta""#,
        );
        let found = record(answer, &dummy()).unwrap();
        assert_eq!(found.lines, ["Alpha", "♪", "Beta"]);
        assert_eq!(found.starts, [13.13, 16.56, 27.16]);
        assert_eq!(found.source, LRCLIB);
    }

    #[test]
    fn lrclib_falls_back_to_plain_lyrics() {
        let answer = lrclib_answer(
            r#""instrumental":false,"plainLyrics":"Alpha\n\nBeta\n","syncedLyrics":null"#,
        );
        assert_eq!(
            record(answer, &dummy()),
            Some(Lyrics::plain("Alpha\n\nBeta", LRCLIB.into()))
        );
    }

    #[test]
    fn lrclib_instrumentals_and_other_songs_are_none() {
        let none = lrclib_answer(r#""instrumental":true,"plainLyrics":null,"syncedLyrics":null"#);
        assert_eq!(record(none, &dummy()), None);
        let stamp =
            lrclib_answer(r#""instrumental":true,"plainLyrics":"","syncedLyrics":"[00:00.00]""#);
        assert_eq!(record(stamp, &dummy()), None);
        let other = lrclib_answer(
            r#""instrumental":false,"plainLyrics":"Alpha","syncedLyrics":"[00:01.00]Alpha""#,
        );
        assert_eq!(
            record(
                other,
                &track("Another Tune Entirely", Some("Dummy Artist"), Some(200))
            ),
            None
        );
    }

    #[test]
    fn lrclib_searches_for_the_first_timed_record_of_the_song() {
        let records = serde_json::from_str(
            r#"[{"trackName":"Dummy Song","artistName":"Dummy Artist","duration":198.4,"instrumental":false,"plainLyrics":"Plain","syncedLyrics":null},
                {"trackName":"Dummy Song","artistName":"Dummy Artist","duration":230,"instrumental":false,"plainLyrics":"Far","syncedLyrics":"[00:01.00]Far"},
                {"trackName":"Dummy Song","artistName":"Dummy Artist","duration":201.6,"instrumental":false,"plainLyrics":"Timed","syncedLyrics":"[00:01.00]Timed"}]"#,
        )
        .unwrap();
        let found = searched(records, &dummy()).unwrap();
        assert_eq!(
            (found.lines, found.starts),
            (vec!["Timed".to_owned()], vec![1.0])
        );
        assert_eq!(searched(Vec::new(), &dummy()), None);
    }

    fn lrc_red_results(items: &[String]) -> Vec<u8> {
        format!(
            r#"{{"results":[{}],"source":"HIT-LRC-RED","total":{}}}"#,
            items.join(","),
            items.len()
        )
        .into_bytes()
    }

    fn lrc_red_result(title: &str, duration: u32, isrc: &str, timing: &str) -> String {
        format!(
            r#"{{"album_name":"Album","artist_name":"BTS","duration":{duration},"id":1,"isrc":"{isrc}","lyricsUrl":"https://example.com/elsewhere.ttml","timing_type":"{timing}","track_name":"{title}"}}"#
        )
    }

    #[test]
    fn lrc_red_takes_the_first_result_of_the_same_recording() {
        let data = lrc_red_results(&[
            lrc_red_result("Dynamite (Live)", 199, "KRAAA2000001", "word"),
            lrc_red_result("Dynamite", 230, "KRAAA2000002", "word"),
            lrc_red_result("Dynamite", 199, "KRAAA2000009", "none"),
            lrc_red_result("Dynamite", 199, "kraaa2000003", "word"),
            lrc_red_result("Dynamite", 199, "KRAAA2000004", "word"),
        ]);
        let dynamite = track("Dynamite", Some("BTS"), Some(199));
        assert_eq!(isrc(&data, &dynamite).as_deref(), Some("KRAAA2000003"));
    }

    #[test]
    fn lrc_red_refuses_an_isrc_unfit_for_the_path() {
        let dynamite = track("Dynamite", Some("BTS"), Some(199));
        for bad in ["../KRAAA20000", "KRAAA200000", "KRAAA20000é1"] {
            let data = lrc_red_results(&[lrc_red_result("Dynamite", 199, bad, "word")]);
            assert_eq!(isrc(&data, &dynamite), None, "{bad}");
        }
    }

    #[tokio::test]
    async fn lrc_red_misses_are_none() {
        let dynamite = track("Dynamite", Some("BTS"), Some(199));
        assert_eq!(isrc(&lrc_red_results(&[]), &dynamite), None);
        assert_eq!(isrc(b"<html>", &dynamite), None);
        assert_eq!(
            lrc_red(&track("Song", None, Some(200))).await,
            None,
            "no artist, so not asked"
        );
        assert_eq!(
            lrc_red(&track("Song", Some("Band"), None)).await,
            None,
            "no length, so not asked"
        );
    }

    fn sunny() -> Track {
        track("晴天", Some("周杰倫"), Some(270))
    }

    #[test]
    fn kugou_picks_the_nearest_recording_of_the_song() {
        let results = r#"{"data":{"info":[
            {"songname":"晴天 (Live)","singername":"周杰伦","duration":270,"hash":"AAAA0001"},
            {"songname":"晴天","singername":"Someone Else","duration":270,"hash":"AAAA0002"},
            {"songname":"晴天","singername":"周杰伦","duration":272,"hash":"AAAA0003"},
            {"songname":"晴天","singername":"周杰伦、Another","duration":269,"hash":"AAAA0004"},
            {"songname":"晴天","singername":"周杰伦","duration":269,"hash":"../evil"}
        ]}}"#;
        assert_eq!(
            kugou_hash(results.as_bytes(), &sunny()).as_deref(),
            Some("AAAA0004")
        );
    }

    /// Asks each site for a well-known song, so a change in their answers
    /// shows: `cargo test --locked live_sites_still_answer -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore = "asks lrc.red, LRCLIB and KuGou"]
    async fn live_sites_still_answer() {
        let (dynamite, sunny) = (track("Dynamite", Some("BTS"), Some(199)), sunny());
        let (lrc_red, lrclib, kugou) =
            tokio::join!(lrc_red(&dynamite), lrclib(&dynamite), kugou(&sunny));
        for (site, found, by_word) in [
            ("lrc.red", lrc_red, true),
            ("LRCLIB", lrclib, false),
            ("KuGou", kugou, true),
        ] {
            let found = found.unwrap_or_else(|| panic!("{site} found nothing"));
            println!(
                "{site}: {} lines, word timed: {}",
                found.lines.len(),
                found.is_word_synced()
            );
            assert!(found.is_synced() && found.lines.len() > 10, "{site}");
            assert_eq!(found.is_word_synced(), by_word, "{site}");
        }
    }

    #[test]
    fn kugou_takes_the_first_lyrics_filed_under_the_song() {
        let results = r#"{"candidates":[
            {"id":"1","accesskey":"K1","song":"晴天 (Cover)","singer":"Someone Else","duration":270000},
            {"id":2,"accesskey":"K2","song":"晴天","singer":"周杰伦","duration":269792},
            {"id":"3","accesskey":"K3","song":"晴天","singer":"周杰伦","duration":270000}
        ]}"#;
        assert_eq!(
            kugou_candidate(results.as_bytes(), &sunny()),
            Some(("2".into(), "K2".into()))
        );
        assert_eq!(kugou_candidate(b"{}", &sunny()), None);
    }
}
