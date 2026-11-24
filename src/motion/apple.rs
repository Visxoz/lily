//! Apple Music's catalogue, read as its web player reads it, with the token
//! shipped in that player's script, for albums' motion covers.

use std::{
    path::PathBuf,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::Value;
use tokio::sync::Mutex;
use url::Url;

use super::{hls, names};
use crate::http::motion_allowed;

/// How long Apple is left alone after its script held no token, as each try
/// costs the 3 MB script; also how long a token counts as fresh, so a refusal
/// of it is final.
const BACKOFF: Duration = Duration::from_secs(10 * 60);
const REFUSED: &str = "Apple Music refused its web player's token.";
/// The smallest page naming the player's script: 76 KB, where /us/new is 2.4 MB.
const PAGE: &str = "https://music.apple.com/us/home";
/// Far past any page, playlist or search answer, so a hostile one can't fill
/// the memory.
const ANSWER_LIMIT: usize = 4 << 20;
/// The script is 3.3 MB, read only until the token, about 0.9 MB in.
const SCRIPT_LIMIT: usize = 16 << 20;

pub struct Apple {
    /// Windows' region, until Apple says it has no storefront there.
    storefront: std::sync::Mutex<String>,
    /// Kept across launches, so a launch needn't fetch the script.
    token_file: PathBuf,
    /// Held through a fetch, so searches waiting share its token.
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    token: Option<Token>,
    /// When Apple's page or script last held no token.
    failed: Option<Instant>,
    /// Apple refused a token fresh from its own script, so it isn't asked
    /// again this run.
    withdrawn: bool,
}

/// Shipped to every visitor of music.apple.com, so no secret, but never
/// logged all the same.
#[derive(Clone)]
struct Token {
    jwt: String,
    /// Seconds since 1970, as a number, so a hostile one can't overflow a time.
    expires: f64,
    /// When it was read from Apple's script; None for the last run's file.
    fetched: Option<Instant>,
}

impl Apple {
    pub fn new(storefront: String, token_file: PathBuf) -> Self {
        Self {
            storefront: std::sync::Mutex::new(storefront),
            token_file,
            state: Mutex::default(),
        }
    }

    /// The album's square motion cover (editorialVideo.motionDetailSquare.video,
    /// the master playlist): Ok(None) when Apple surely has none that matches,
    /// Err when it couldn't be asked.
    pub async fn square_cover(
        &self,
        client: &reqwest::Client,
        album: &str,
        artist: &str,
    ) -> Result<Option<Url>, String> {
        let failed =
            |e: reqwest::Error| format!("Apple Music's search failed: {}", e.without_url());
        // The storefront's fallback, apart from the token's retry.
        let mut refreshed = false;
        for _ in 0..3 {
            let url = self.search_url(&format!("{artist} {album}"))?;
            let token = self.bearer(client).await?;
            let response = client
                .get(url.clone())
                .bearer_auth(&token.jwt)
                // Apple refuses the token from anywhere but its own player.
                .header(reqwest::header::ORIGIN, "https://music.apple.com")
                .send()
                .await
                .map_err(failed)?;
            // Apple serves no storefront in some regions, so the US one stands in.
            if response.status() == reqwest::StatusCode::BAD_REQUEST
                && let Ok(mut storefront) = self.storefront.lock()
                && *storefront != "us"
            {
                *storefront = "us".to_owned();
                continue;
            }
            if response.status() != reqwest::StatusCode::UNAUTHORIZED {
                let body =
                    crate::http::body(response.error_for_status().map_err(failed)?, ANSWER_LIMIT)
                        .await
                        .map_err(|e| format!("Apple Music's search failed: {e}"))?;
                let answer: Value = serde_json::from_slice(&body)
                    .map_err(|_| "Apple Music's search answer isn't JSON.".to_owned())?;
                let albums = answer
                    .pointer("/results/albums/data")
                    .and_then(Value::as_array);
                return Ok(square(albums.map_or(&[], Vec::as_slice), album, artist));
            }
            // A token kept a while may have been withdrawn, so the script's
            // current one is tried; a fresh one refused is final.
            let mut state = self.state.lock().await;
            if state
                .token
                .as_ref()
                .is_some_and(|kept| kept.jwt == token.jwt)
            {
                state.token = None;
                let _ = std::fs::remove_file(&self.token_file);
            }
            if refreshed || token.fetched.is_some_and(|at| at.elapsed() <= BACKOFF) {
                state.withdrawn = true;
                break;
            }
            refreshed = true;
        }
        Err(REFUSED.to_owned())
    }

    fn search_url(&self, term: &str) -> Result<Url, String> {
        let mut url = Url::parse(&format!(
            "https://amp-api.music.apple.com/v1/catalog/{}/search",
            self.storefront
                .lock()
                .map_or_else(|_| "us".to_owned(), |s| s.clone())
        ))
        .map_err(|_| "The search's address couldn't be made.".to_owned())?;
        // Form-encoded, so a "+" in a name goes as %2B rather than a space.
        url.query_pairs_mut()
            .append_pair("term", term)
            .append_pair("types", "albums")
            .append_pair("limit", "10")
            .append_pair("extend", "editorialVideo")
            // Only what the match reads: a quarter of the answer.
            .append_pair("fields[albums]", "name,artistName,editorialVideo");
        Ok(url)
    }

    /// The kept token while it lasts, else the saved one, else the one in the
    /// web player's script, which its page names; none while backing off.
    async fn bearer(&self, client: &reqwest::Client) -> Result<Token, String> {
        let mut state = self.state.lock().await;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0.0, |d| d.as_secs_f64());
        if let Some(token) = state.token.as_ref().filter(|t| t.expires > now) {
            return Ok(token.clone());
        }
        if state.withdrawn {
            return Err(REFUSED.to_owned());
        }
        if state.failed.is_some_and(|at| at.elapsed() < BACKOFF) {
            return Err("Apple Music's player script held no token a moment ago.".to_owned());
        }
        if let Ok(jwt) = std::fs::read_to_string(&self.token_file)
            && let Some(expires) = web_play_expiry(&jwt).filter(|&expires| expires > now)
        {
            let token = Token {
                jwt,
                expires,
                fetched: None,
            };
            state.token = Some(token.clone());
            return Ok(token);
        }
        // Only a script without a live token is waited out; a network that
        // failed may be back by the next song. The token is the same in every
        // storefront.
        let html = fetch(client, PAGE).await?;
        let Some(path) = script_path(&html) else {
            state.failed = Some(Instant::now());
            return Err("Apple Music's player script wasn't found.".to_owned());
        };
        let script = get(client, &format!("https://music.apple.com{path}")).await?;
        let Some((jwt, expires)) = script_token(script)
            .await?
            .filter(|&(_, expires)| expires > now)
        else {
            state.failed = Some(Instant::now());
            return Err("Apple Music's player script held no token.".to_owned());
        };
        if let Some(dir) = self.token_file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = crate::settings::write_atomic(&self.token_file, jwt.as_bytes());
        let token = Token {
            jwt,
            expires,
            fetched: Some(Instant::now()),
        };
        state.token = Some(token.clone());
        Ok(token)
    }
}

/// The one MP4 file the master's chosen variant is byte ranges of.
pub async fn clip_url(client: &reqwest::Client, master: &Url) -> Result<Url, String> {
    let variant = hls::variant(&fetch(client, master.as_str()).await?, master)
        .filter(motion_allowed)
        .ok_or("The cover's playlist has no H.264 video.")?;
    hls::media_file(&fetch(client, variant.as_str()).await?, &variant)
        .filter(motion_allowed)
        .ok_or_else(|| "The cover's video isn't one file.".to_owned())
}

/// Windows' region as Apple's storefront, lowercase two letters, "us" if
/// unknown.
pub fn storefront() -> String {
    use windows_sys::Win32::Globalization::GetUserDefaultGeoName;
    let mut name = [0u16; 16];
    // SAFETY: the buffer is valid for writes of the length passed with it.
    let written = unsafe { GetUserDefaultGeoName(name.as_mut_ptr(), name.len() as i32) };
    // The count includes the closing nul; 0 is a failure.
    let name = usize::try_from(written)
        .ok()
        .and_then(|n| name.get(..n.checked_sub(1)?));
    // Regions without a country code come as UN numbers, as "001" for the world.
    match name.map(String::from_utf16_lossy) {
        Some(code) if code.len() == 2 && code.bytes().all(|b| b.is_ascii_alphabetic()) => {
            code.to_ascii_lowercase()
        }
        _ => "us".to_owned(),
    }
}

/// The URL's answer, from an allowed host only.
async fn get(client: &reqwest::Client, url: &str) -> Result<reqwest::Response, String> {
    let url = Url::parse(url)
        .ok()
        .filter(motion_allowed)
        .ok_or("A motion cover's address isn't allowed.")?;
    let host = url.host_str().unwrap_or_default().to_owned();
    client
        .get(url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| format!("{host} failed: {}", e.without_url()))
}

/// The URL's body as text.
async fn fetch(client: &reqwest::Client, url: &str) -> Result<String, String> {
    let body = crate::http::body(get(client, url).await?, ANSWER_LIMIT)
        .await
        .map_err(|e| format!("A motion cover's answer failed: {e}"))?;
    Ok(String::from_utf8(body)
        .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()))
}

/// The web player's token, read from the script as it arrives and stopping
/// once it is in, so neither the whole script nor its download is needed.
async fn script_token(mut script: reqwest::Response) -> Result<Option<(String, f64)>, String> {
    let mut read = 0;
    let mut scan = TokenScan::default();
    while let Some(chunk) = script
        .chunk()
        .await
        .map_err(|e| format!("Apple Music's player script failed: {}", e.without_url()))?
    {
        read += chunk.len();
        if read > SCRIPT_LIMIT {
            return Err("Apple Music's player script was far too large.".to_owned());
        }
        if let Some(found) = scan.push(&chunk) {
            return Ok(Some(found));
        }
    }
    Ok(web_play_token(&scan.0))
}

/// Holds only what may be a token cut off by a piece's end.
#[derive(Default)]
struct TokenScan(Vec<u8>);

impl TokenScan {
    /// Far past the web player's token, about 270 characters.
    const TOKEN_MAX: usize = 4096;

    fn push(&mut self, piece: &[u8]) -> Option<(String, f64)> {
        self.0.extend_from_slice(piece);
        let whole = self
            .0
            .iter()
            .rposition(|b| !token_byte(b))
            .map_or(0, |i| i + 1);
        if let Some(found) = self.0.get(..whole).and_then(web_play_token) {
            return Some(found);
        }
        // A run of token characters longer than any token holds one only at its end.
        let keep_from = whole.max(self.0.len().saturating_sub(Self::TOKEN_MAX));
        self.0.drain(..keep_from);
        None
    }
}

fn token_byte(b: &u8) -> bool {
    b.is_ascii_alphanumeric() || b"-_.".contains(b)
}

/// The album whose name and artist surely match, first among those with a
/// motion cover: a wrong cover is worse than none, so nothing looser is tried.
fn square(albums: &[Value], album: &str, artist: &str) -> Option<Url> {
    let album = names::normalised(album);
    let artist = names::normalised(artist);
    // A name of only symbols, as "÷", would match every other such name.
    if album.is_empty() || artist.is_empty() {
        return None;
    }
    albums.iter().find_map(|item| {
        let attributes = item.get("attributes")?;
        let name = attributes.get("name")?.as_str()?;
        let by = attributes.get("artistName")?.as_str()?;
        if names::normalised(name) != album || !names::credits(by).contains(&artist) {
            return None;
        }
        let video = attributes
            .pointer("/editorialVideo/motionDetailSquare/video")?
            .as_str()?;
        Url::parse(video).ok().filter(motion_allowed)
    })
}

/// The player's module script; the legacy one for old browsers is passed over.
fn script_path(html: &str) -> Option<&str> {
    html.split('"').find(|part| {
        part.starts_with("/assets/index") && part.ends_with(".js") && !part.contains("legacy")
    })
}

/// The script holds several tokens, as `eyJ…` header.payload.signature;
/// Apple's catalogue takes only the web player's.
fn web_play_token(script: &[u8]) -> Option<(String, f64)> {
    let mut rest = script;
    while let Some(start) = rest.windows(3).position(|w| w == b"eyJ") {
        let tail = rest.get(start..)?;
        let (jwt, after) =
            tail.split_at_checked(tail.iter().take_while(|b| token_byte(b)).count())?;
        // Token characters are ASCII, so this never fails.
        if let Ok(jwt) = std::str::from_utf8(jwt)
            && let Some(expires) = web_play_expiry(jwt)
        {
            return Some((jwt.to_owned(), expires));
        }
        rest = after;
    }
    None
}

/// When the web player's token lapses, in seconds since 1970, from its
/// payload; None for any other token.
fn web_play_expiry(jwt: &str) -> Option<f64> {
    let mut parts = jwt.split('.');
    let (Some(_), Some(payload), Some(_), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()?;
    (claims.get("iss")?.as_str()? == "AMPWebPlay").then_some(())?;
    claims.get("exp")?.as_f64()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn albums(items: &[String]) -> Vec<Value> {
        serde_json::from_str(&format!("[{}]", items.join(","))).unwrap()
    }

    fn album(name: &str, artist: &str, video: Option<&str>) -> String {
        let editorial = video.map_or(String::new(), |v| {
            format!(
                r#","editorialVideo":{{"motionDetailSquare":{{"video":"https://mvod.itunes.apple.com/{v}/square.m3u8"}},"motionDetailTall":{{"video":"https://mvod.itunes.apple.com/{v}/tall.m3u8"}}}}"#
            )
        });
        format!(
            r#"{{"id":"1","attributes":{{"name":"{name}","artistName":"{artist}"{editorial}}}}}"#
        )
    }

    fn found(albums: &[Value], name: &str, artist: &str) -> Option<String> {
        square(albums, name, artist).map(String::from)
    }

    #[test]
    fn takes_the_exact_album_with_a_video_among_its_versions() {
        let found = found(
            &albums(&[
                album("SOS Deluxe: LANA", "SZA", Some("deluxe")),
                album("SOS", "SZA", None),
                album("SOS", "SZA", Some("explicit")),
                album("SOS", "SZA", Some("later")),
            ]),
            "SOS",
            "SZA",
        );
        assert_eq!(
            found.as_deref(),
            Some("https://mvod.itunes.apple.com/explicit/square.m3u8")
        );
    }

    #[test]
    fn never_takes_another_edition() {
        assert_eq!(
            found(
                &albums(&[album("SOS Deluxe: LANA", "SZA", Some("d"))]),
                "SOS",
                "SZA"
            ),
            None
        );
        assert_eq!(
            found(
                &albums(&[album("SOS", "SZA", Some("s"))]),
                "SOS Deluxe",
                "SZA"
            ),
            None
        );
    }

    #[test]
    fn reads_singles_and_eps_by_their_own_name() {
        assert!(
            found(
                &albums(&[album("Snooze - Single", "SZA", Some("s"))]),
                "Snooze",
                "SZA"
            )
            .is_some()
        );
        assert!(
            found(
                &albums(&[album("Hitchhiker - EP", "SZA", Some("e"))]),
                "Hitchhiker",
                "SZA"
            )
            .is_some()
        );
        assert_eq!(
            names::normalised("good days (Remix)"),
            names::normalised("Good Days - Remix")
        );
    }

    #[test]
    fn needs_the_tracks_artist_in_apples_artist() {
        let one = |name, by, artist| found(&albums(&[album(name, by, Some("v"))]), name, artist);
        assert_eq!(one("SOS", "SZA", "ABBA"), None);
        assert!(one("Good News", "Metro Boomin, Future & SZA", "Future").is_some());
        assert!(one("Bookends", "Simon & Garfunkel", "Simon & Garfunkel").is_some());
        assert_eq!(one("Christmas", "Flowers", "Low"), None);
    }

    #[test]
    fn matches_nothing_on_names_of_only_symbols() {
        assert_eq!(
            found(
                &albums(&[album("÷", "Ed Sheeran", Some("divide"))]),
                "+",
                "Ed Sheeran"
            ),
            None
        );
    }

    #[test]
    fn drops_videos_off_the_allowed_hosts() {
        let elsewhere = r#"{"attributes":{"name":"SOS","artistName":"SZA","editorialVideo":{"motionDetailSquare":{"video":"https://example.com/square.m3u8"}}}}"#;
        let found = found(
            &albums(&[elsewhere.to_owned(), album("SOS", "SZA", Some("sos"))]),
            "SOS",
            "SZA",
        );
        assert_eq!(
            found.as_deref(),
            Some("https://mvod.itunes.apple.com/sos/square.m3u8")
        );
    }

    #[test]
    fn encodes_a_plus_apart_from_a_space() {
        let apple = Apple::new("gb".to_owned(), PathBuf::new());
        let url = apple.search_url("Ed Sheeran +").unwrap();
        assert_eq!(
            url.as_str(),
            "https://amp-api.music.apple.com/v1/catalog/gb/search?term=Ed+Sheeran+%2B&types=albums&limit=10\
             &extend=editorialVideo&fields%5Balbums%5D=name%2CartistName%2CeditorialVideo"
        );
    }

    fn token(kid: &str, issuer: &str, expires: u64) -> String {
        let header = format!(r#"{{"alg":"ES256","typ":"JWT","kid":"{kid}"}}"#);
        let payload = format!(r#"{{"iss":"{issuer}","iat":1793900708,"exp":{expires}}}"#);
        format!(
            "{}.{}.MEUCIQDx-_sig",
            URL_SAFE_NO_PAD.encode(header),
            URL_SAFE_NO_PAD.encode(payload)
        )
    }

    #[test]
    fn picks_the_web_players_token_from_the_script() {
        let refused = token("97DQU9QUD6", "5IKPP2IECQ", 1_799_948_708);
        // Payloads of different lengths, so the base64 padding left off differs.
        for expires in [1_799_948_708, 17_999_487_080] {
            let web_play = token("WebPlayKid", "AMPWebPlay", expires);
            let script =
                format!(r#"var a="{refused}";const b={{token:"{web_play}"}},c="eyJnot a token";"#);
            let want = Some((web_play, expires as f64));
            assert_eq!(web_play_token(script.as_bytes()), want);
            // Fed in pieces of every size, so tokens are cut at every place.
            for size in 1..script.len() {
                let mut scan = TokenScan::default();
                let found = script
                    .as_bytes()
                    .chunks(size)
                    .find_map(|piece| scan.push(piece))
                    .or_else(|| web_play_token(&scan.0));
                assert_eq!(found, want, "pieces of {size}");
            }
        }
        let mut scan = TokenScan::default();
        let long = token("k", "AMPWebPlay", 2_000_000_000);
        assert_eq!(scan.push(&[b'a'; 10_000]), None);
        assert!(
            scan.0.len() <= TokenScan::TOKEN_MAX,
            "a long run isn't kept whole"
        );
        assert!(scan.push(long.as_bytes()).is_none());
        assert_eq!(scan.push(b";"), Some((long, 2e9)));
        assert_eq!(web_play_expiry(&refused), None);
        assert_eq!(web_play_token(b"eyJ.eyJ."), None);
        assert_eq!(web_play_token(b"eyJ"), None);
        assert_eq!(
            web_play_expiry(&token("k", "AMPWebPlay", 1).replace(".MEUCIQDx", ".a.b")),
            None
        );
    }

    #[test]
    fn finds_the_players_module_script() {
        let page = r#"<link rel="stylesheet" href="/assets/index~Cz.css"><script nomodule src="/assets/index-legacy~Dq.js"></script><script type="module" crossorigin src="/assets/index~B8RKPfmaCI.js"></script>"#;
        assert_eq!(script_path(page), Some("/assets/index~B8RKPfmaCI.js"));
        assert_eq!(script_path("<html></html>"), None);
    }

    #[test]
    fn storefront_is_two_lowercase_letters() {
        let code = storefront();
        assert!(
            code.len() == 2 && code.bytes().all(|b| b.is_ascii_lowercase()),
            "{code}"
        );
    }

    fn live() -> (&'static reqwest::Client, Apple) {
        let file = std::env::temp_dir().join("lily-motion-apple-token");
        (
            crate::http::motion_client(),
            Apple::new("gb".to_owned(), file),
        )
    }

    /// Asks Apple: a known album's cover resolves to an MP4 clip, and a
    /// made-up album by a real artist has none.
    #[tokio::test]
    #[ignore]
    async fn live_finds_and_resolves_a_motion_cover() {
        let (client, apple) = live();
        let master = apple
            .square_cover(client, "SOS", "SZA")
            .await
            .unwrap()
            .expect("SOS has a motion cover");
        let clip = clip_url(client, &master).await.unwrap();
        let head = client
            .get(clip)
            .header(reqwest::header::RANGE, "bytes=0-63")
            .send()
            .await
            .unwrap();
        let head = head.error_for_status().unwrap().bytes().await.unwrap();
        assert_eq!(head.get(4..8), Some(&b"ftyp"[..]), "the clip is an MP4");
        let none = apple
            .square_cover(client, "Quiet Marmalade Afternoons", "SZA")
            .await
            .unwrap();
        assert_eq!(none, None);
    }
}
