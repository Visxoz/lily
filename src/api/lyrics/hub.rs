//! Better Lyrics' hub: Apple Music's, Spicy Lyrics', lrc.red's, QQ Music's,
//! KuGou's and LRCLIB's lyrics in one answer, behind a day's pass that only
//! Cloudflare's check in a browser gives, which `crate::lyrics_pass` runs.

use std::{
    borrow::Cow,
    sync::{LazyLock, Mutex, MutexGuard},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::Engine as _;
use serde::Deserialize;
use serde_json::Value;

use super::{Lyrics, formats, sites};
use crate::api::Track;

pub const HOST: &str = "lyrics.api.dacubeking.com";
pub const CHALLENGE_URL: &str = "https://lyrics.api.dacubeking.com/challenge";
const LYRICS_URL: &str = "https://lyrics.api.dacubeking.com/v2/lyrics";
const VERIFY_URL: &str = "https://lyrics.api.dacubeking.com/verify-turnstile";
/// The answer streams in as each site replies, as Better Lyrics allows.
const DEADLINE: Duration = Duration::from_secs(20);
/// Checked again this long before the pass lapses, so songs never go without.
const RENEW_BEFORE: u64 = 60 * 60;
/// Used until this long before it lapses.
const USE_UNTIL: u64 = 60;
/// After a check that didn't pass, or a pass refused at once, so a failing
/// check isn't run song after song; doubled for each failure in a row, up to
/// `MAX_DOUBLINGS` times.
const BACK_OFF: Duration = Duration::from_secs(15 * 60);
const MAX_DOUBLINGS: u32 = 4;

/// The sites' lyrics, best first in each kind.
#[derive(Debug, Default, PartialEq)]
pub(in crate::api) struct Found {
    /// Apple Music's lyrics and those like them.
    pub apple: Vec<Lyrics>,
    pub others: Vec<Lyrics>,
    /// Asked already, by the hub or for lrc.red by `other_sites`, with lyrics
    /// or without, so not to be asked again.
    pub asked_lrc_red: bool,
    pub asked_lrclib: bool,
}

/// `None` without a pass, which is then checked for the songs after, or when
/// the hub fails. Events are read as they arrive, until nothing still to come
/// could be chosen over what has, so a slow site or a stall keeps what came
/// before it.
pub(super) async fn lyrics(track: &Track) -> Option<Found> {
    let jwt = current()?;
    let artist = track.artist_names();
    let duration = track.duration.map(|d| d.to_string());
    let mut form = vec![
        ("videoId", track.id.as_str()),
        ("song", &track.title),
        ("artist", &artist),
        ("token", &jwt),
    ];
    form.extend(duration.as_deref().map(|d| ("duration", d)));
    let deadline = tokio::time::Instant::now() + DEADLINE;
    let send = crate::http::lyrics_client()
        .post(LYRICS_URL)
        .form(&form)
        .send();
    let mut response = tokio::time::timeout_at(deadline, send)
        .await
        .ok()?
        .map_err(|e| log::debug!("lyrics hub: {e}"))
        .ok()?;
    if response.status() == reqwest::StatusCode::FORBIDDEN {
        refused(&jwt);
        return None;
    }
    let mut answer = Answer::default();
    let (mut buffer, mut total) = (Vec::new(), 0);
    while !answer.settled()
        && total <= sites::MAX_BODY
        && let Ok(Ok(Some(chunk))) = tokio::time::timeout_at(deadline, response.chunk()).await
    {
        total += chunk.len();
        answer.feed(&mut buffer, &chunk);
    }
    // The last event may end without its blank line.
    answer.read(&String::from_utf8_lossy(&buffer));
    Some(answer.found())
}

#[derive(Deserialize)]
struct Event<'a> {
    #[serde(borrow)]
    provider: Cow<'a, str>,
    #[serde(default)]
    results: Value,
}

/// Providers in the order their lyrics are preferred, Apple Music's kind first.
const PROVIDERS: [&str; 6] = ["golyrics", "spicy", "binimum", "qq", "kugou", "lrclib"];

#[derive(Default)]
struct Answer {
    slots: [Option<Lyrics>; PROVIDERS.len()],
    heard: [bool; PROVIDERS.len()],
}

impl Answer {
    /// Word timing from a site every one ranked above answered without, which
    /// `chosen` prefers in this order. Unchecked yet, so one that disagrees
    /// costs the later sites.
    fn settled(&self) -> bool {
        for (slot, &heard) in self.slots.iter().zip(&self.heard) {
            if !heard {
                return false;
            }
            if slot.as_ref().is_some_and(Lyrics::is_word_synced) {
                return true;
            }
        }
        false
    }

    /// Reads the events `chunk` completes, keeping an unfinished one in `buffer`.
    fn feed(&mut self, buffer: &mut Vec<u8>, chunk: &[u8]) {
        // The blank line ending an event may straddle the chunks.
        let mut scanned = buffer.len().saturating_sub(1);
        buffer.extend(chunk.iter().filter(|&&b| b != b'\r'));
        let mut start = 0;
        while let Some(at) = buffer[scanned..].windows(2).position(|w| w == b"\n\n") {
            let end = scanned + at;
            self.read(&String::from_utf8_lossy(&buffer[start..end]));
            (start, scanned) = (end + 2, end + 2);
        }
        // Once per chunk, so the rest isn't shifted down for every event.
        buffer.drain(..start);
    }

    /// An event's `data:` lines of `{"provider": …, "results": {…}}`.
    fn read(&mut self, event: &str) {
        // JSON skips the space after `data:`. One line, the usual, is borrowed.
        let mut data = event.lines().filter_map(|line| line.strip_prefix("data:"));
        let Some(first) = data.next() else {
            return;
        };
        let data: Cow<str> = match data.next() {
            None => first.into(),
            Some(second) => [first, second].into_iter().chain(data).collect(),
        };
        let Ok(event) = serde_json::from_str::<Event>(&data) else {
            return;
        };
        let Some(slot) = PROVIDERS.iter().position(|&p| p == event.provider) else {
            return;
        };
        self.heard[slot] = true;
        let results = &event.results;
        let text = |key| results.get(key).and_then(Value::as_str);
        self.slots[slot] = match PROVIDERS[slot] {
            "golyrics" => text("lyrics")
                .and_then(|t| formats::ttml(&unwrapped(t, "ttml"), "Source: Better Lyrics")),
            "spicy" => text("lyrics").and_then(|t| formats::ttml(t, "Source: Spicy Lyrics")),
            "binimum" => text("lyrics").and_then(|t| formats::ttml(t, sites::LRC_RED)),
            // QRC, bare or in QQ Music's XML.
            "qq" => text("lyrics").and_then(|t| {
                let t = unwrapped(t, "lyrics");
                formats::qrc(
                    &formats::qrc_content(&t).map_or(t, Cow::Owned),
                    "Source: QQ Music",
                )
            }),
            "kugou" => {
                text("lyrics").and_then(|t| formats::lrc(&unwrapped(t, "lyrics"), sites::KUGOU))
            }
            _ => sites::lrclib_lyrics(text("synced"), text("plain")),
        };
    }

    fn found(self) -> Found {
        let [golyrics, spicy, binimum, qq, kugou, lrclib] = self.slots;
        let [_, _, asked_lrc_red, _, _, asked_lrclib] = self.heard;
        Found {
            apple: [golyrics, spicy, binimum].into_iter().flatten().collect(),
            others: [qq, kugou, lrclib].into_iter().flatten().collect(),
            asked_lrc_red,
            asked_lrclib,
        }
    }
}

/// Some sites' lyrics come as JSON holding them under `key`.
fn unwrapped<'a>(text: &'a str, key: &str) -> Cow<'a, str> {
    if text.starts_with('{')
        && let Ok(Value::Object(mut json)) = serde_json::from_str(text)
        && let Some(Value::String(inner)) = json.remove(key)
    {
        return Cow::Owned(inner);
    }
    Cow::Borrowed(text)
}

/// The hub's pass: a JWT good for a day from one address.
#[derive(Debug, PartialEq)]
struct Pass {
    jwt: String,
    /// Seconds since the epoch.
    expiry: u64,
}

impl Pass {
    /// `None` unless the JWT says when it was issued and when it lapses.
    fn read(jwt: String) -> Option<Self> {
        #[derive(Deserialize)]
        struct Claims {
            iat: f64,
            exp: f64,
        }
        let mut parts = jwt.split('.');
        let claims = parts.nth(1)?.trim_end_matches('=');
        if parts.count() != 1 {
            return None;
        }
        let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(claims)
            .ok()?;
        let Claims { iat, exp } = serde_json::from_slice(&claims).ok()?;
        (iat >= 0.0 && exp > iat).then_some(Self {
            jwt,
            expiry: exp as u64,
        })
    }

    /// Good for a while yet.
    fn usable(&self) -> bool {
        self.expiry > now() + USE_UNTIL
    }
}

#[derive(Default)]
struct State {
    pass: Option<Pass>,
    /// When this run got the pass, by its own clock, which the hub's needn't match.
    got_at: Option<Instant>,
    retry_after: Option<Instant>,
    failures: u32,
}

impl State {
    fn back_off(&mut self) {
        let wait = BACK_OFF * 2u32.pow(self.failures.min(MAX_DOUBLINGS));
        self.retry_after = Some(Instant::now() + wait);
        self.failures += 1;
    }
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| {
    let pass = crate::platform::secret::read(&pass_path()).and_then(Pass::read);
    Mutex::new(State {
        pass,
        ..State::default()
    })
});

fn state() -> MutexGuard<'static, State> {
    STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn pass_path() -> std::path::PathBuf {
    crate::settings::data_dir().join("lyrics-pass.bin")
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn current() -> Option<String> {
    let state = state();
    state
        .pass
        .as_ref()
        .filter(|pass| pass.usable())
        .map(|pass| pass.jwt.clone())
}

/// Whether Cloudflare's check should run now: the pass is missing or lapsing
/// soon, and no check failed lately.
pub fn wants_check() -> bool {
    let state = state();
    state
        .pass
        .as_ref()
        .is_none_or(|pass| pass.expiry < now() + RENEW_BEFORE)
        && state
            .retry_after
            .is_none_or(|after| Instant::now() >= after)
}

/// A check that didn't pass, or a pass the hub wouldn't give for its token.
pub fn check_failed() {
    state().back_off();
}

/// Trades the check's token for a pass, kept for the next start: whether songs
/// went without the hub until now, or `None` once `check_failed` is due.
pub async fn verify(token: String) -> Option<bool> {
    let response = crate::http::lyrics_client()
        .post(VERIFY_URL)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(serde_json::json!({ "token": token }).to_string())
        .send()
        .await
        .map_err(|e| log::debug!("lyrics hub pass: {e}"))
        .ok()?;
    let answer: Value = serde_json::from_slice(&sites::read(response).await?).ok()?;
    let pass = Pass::read(answer.get("jwt")?.as_str()?.to_owned())?;
    if let Err(e) = crate::platform::secret::write(&pass_path(), &pass.jwt) {
        log::warn!("could not save the lyrics pass: {e}");
    }
    let mut state = state();
    let went_without = !state.pass.as_ref().is_some_and(Pass::usable);
    state.pass = Some(pass);
    state.got_at = Some(Instant::now());
    state.retry_after = None;
    state.failures = 0;
    Some(went_without)
}

/// The hub refused `jwt`, as it does once the address changes.
fn refused(jwt: &str) {
    let mut state = state();
    if state.pass.take_if(|pass| pass.jwt == jwt).is_none() {
        return;
    }
    if state
        .got_at
        .take()
        .is_some_and(|at| at.elapsed() < Duration::from_secs(60))
    {
        state.back_off();
    }
    drop(state);
    let _ = std::fs::remove_file(pass_path());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(provider: &str, results: Value) -> String {
        format!(
            "event: provider\ndata: {}\n\n",
            serde_json::json!({ "provider": provider, "results": results })
        )
    }

    #[test]
    fn reads_each_sites_lyrics_best_first() {
        let stream = [
            "event: metadata\r\ndata: {\"song\":\"Dummy Song\"}\r\n\r\n".to_owned(),
            event(
                "lrclib",
                serde_json::json!({ "synced": "[00:01.00]Alpha", "plain": "Alpha" }),
            ),
            event(
                "qq",
                serde_json::json!({ "lyrics": serde_json::json!({ "lyrics": QRC }).to_string() }),
            ),
            event(
                "binimum",
                serde_json::json!({ "lyrics": LINE_TIMED, "timingType": "line" }),
            ),
            event(
                "golyrics",
                serde_json::json!({ "lyrics": serde_json::json!({ "ttml": LINE_TIMED }).to_string() }),
            ),
            // Split over two `data:` lines, which join.
            event(
                "kugou",
                serde_json::json!({ "lyrics": r#"{"lyrics":"[00:01.00]Alpha"}"# }),
            )
            .replacen(r#","results""#, "\ndata: ,\"results\"", 1),
            event("someday", serde_json::json!({ "lyrics": "Alpha" })),
            "data: [DONE]\n\n".to_owned(),
        ]
        .concat();
        assert!(stream.contains("\ndata: ,\"results\""));
        // In chunks that split lines and blank lines.
        let read_all = |stream: &str| {
            let (mut answer, mut buffer) = (Answer::default(), Vec::new());
            for chunk in stream.as_bytes().chunks(7) {
                answer.feed(&mut buffer, chunk);
            }
            answer.read(&String::from_utf8_lossy(&buffer));
            answer.found()
        };
        let found = read_all(&stream);
        assert_eq!(
            sources(&found.apple),
            ["Source: Better Lyrics", "Source: lrc.red"]
        );
        assert_eq!(
            sources(&found.others),
            ["Source: QQ Music", "Source: KuGou", "Source: LRCLIB"]
        );
        assert!(found.others[0].is_word_synced());
        assert!(
            found
                .apple
                .iter()
                .chain(&found.others)
                .all(|l| l.lines == ["Alpha"] && l.starts == [1.0])
        );
        assert!(found.asked_lrc_red && found.asked_lrclib);
        assert_eq!(read_all("<html>busy</html>"), Found::default());
    }

    const LINE_TIMED: &str = r#"<tt xmlns="http://www.w3.org/ns/ttml"><body><div><p begin="1.0" end="2.0">Alpha</p></div></body></tt>"#;
    const WORD_TIMED: &str = r#"<tt xmlns="http://www.w3.org/ns/ttml"><body><div><p begin="1.0" end="2.0"><span begin="1.0" end="1.5">Al</span><span begin="1.5" end="2.0">pha</span></p></div></body></tt>"#;
    const QRC: &str =
        r#"<QrcInfos><Lyric_1 LyricContent="[1000,1000]Alpha(1000,1000)"/></QrcInfos>"#;

    fn sources(list: &[Lyrics]) -> Vec<&str> {
        list.iter().map(|l| l.source.as_str()).collect()
    }

    #[test]
    fn settles_once_nothing_to_come_could_be_chosen() {
        assert!(formats::ttml(WORD_TIMED, "").is_some_and(|l| l.is_word_synced()));
        let (mut answer, mut buffer) = (Answer::default(), Vec::new());
        let events = [
            event("binimum", serde_json::json!({ "lyrics": LINE_TIMED })),
            event("golyrics", serde_json::json!({})),
            event("spicy", serde_json::json!({ "lyrics": WORD_TIMED })),
        ];
        for (i, event) in events.iter().enumerate() {
            assert!(!answer.settled(), "settled before event {i}");
            answer.feed(&mut buffer, event.replace('\n', "\r\n").as_bytes());
        }
        assert!(answer.settled());
        // A half-sent event, as a stall leaves, adds nothing to what came.
        answer.feed(&mut buffer, b"data: {\"provi");
        answer.read(&String::from_utf8_lossy(&buffer));
        let found = answer.found();
        assert_eq!(
            sources(&found.apple),
            ["Source: Spicy Lyrics", "Source: lrc.red"]
        );
        assert!(found.asked_lrc_red && !found.asked_lrclib);
    }

    #[test]
    fn reads_on_while_a_better_site_may_answer() {
        let golyrics = serde_json::json!({ "ttml": WORD_TIMED }).to_string();
        let (mut answer, mut buffer) = (Answer::default(), Vec::new());
        answer.feed(
            &mut buffer,
            event("spicy", serde_json::json!({ "lyrics": WORD_TIMED })).as_bytes(),
        );
        assert!(!answer.settled(), "Apple Music's may still come");
        answer.feed(
            &mut buffer,
            event("golyrics", serde_json::json!({ "lyrics": golyrics })).as_bytes(),
        );
        assert!(answer.settled());
        assert_eq!(
            sources(&answer.found().apple),
            ["Source: Better Lyrics", "Source: Spicy Lyrics"]
        );
    }

    #[test]
    fn settles_on_other_sites_word_timing_once_apple_musics_kind_has_none() {
        let (mut answer, mut buffer) = (Answer::default(), Vec::new());
        let qq = serde_json::json!({ "lyrics": QRC });
        answer.feed(&mut buffer, event("qq", qq).as_bytes());
        for provider in ["golyrics", "spicy"] {
            answer.feed(
                &mut buffer,
                event(provider, serde_json::json!({})).as_bytes(),
            );
            assert!(!answer.settled(), "binimum may still come");
        }
        let binimum = serde_json::json!({ "lyrics": LINE_TIMED });
        answer.feed(&mut buffer, event("binimum", binimum).as_bytes());
        assert!(answer.settled(), "KuGou's and LRCLIB's rank below QQ's");
    }

    #[test]
    fn reads_when_a_pass_was_issued_and_lapses() {
        let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(r#"{"iat":1795411254,"exp":1795497654,"ip":"192.0.2.1"}"#);
        let pass = Pass::read(format!("header.{claims}.signature")).unwrap();
        assert_eq!(pass.expiry, 1_795_497_654);
        assert_eq!(Pass::read("not a token".into()), None);
        assert_eq!(Pass::read("a.e30.c".into()), None, "no times");
    }
}
