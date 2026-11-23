//! Spotify's Canvas, the looping clip behind a song, read as its web player
//! reads it with the user's `sp_dc` cookie. Token codes are made with a key
//! from the player's script, which Spotify rotates, so a refused code has the
//! page checked for a new script.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use url::Url;

use crate::http::motion_allowed;
use crate::motion::names::{credits, normalised};

const PLAYER: &str = "https://open.spotify.com/";
const SCRIPT_PREFIX: &str = "https://open.spotifycdn.com/cdn/build/web-player/web-player.";
const SEARCH: &str = "https://api-partner.spotify.com/pathfinder/v2/query";
const CANVASES: &str = "https://spclient.wg.spotify.com/canvaz-cache/v0/canvases";
/// How long a read of the player's script stands, found or not.
const BACKOFF: Duration = Duration::from_secs(10 * 60);
/// The script is about 5 MB; the page (300 KB) and every answer are smaller.
const SCRIPT_LIMIT: usize = 16 << 20;
const ANSWER_LIMIT: usize = 2 << 20;

/// What the web player's script holds: the newest key its token codes are
/// made with and the search's hash, with the script's address, which changes
/// with its content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Config {
    version: u64,
    secret: String,
    search: String,
    #[serde(default)]
    script: String,
}

#[derive(Clone)]
struct Token {
    bearer: String,
    expires: SystemTime,
    /// The `sp_dc` it was minted with, so another account's is never used.
    cookie: String,
}

#[derive(Clone)]
struct Session {
    config: Config,
    token: Token,
}

/// A song as Spotify's search answers it.
struct Candidate {
    uri: String,
    name: String,
    artists: Vec<String>,
    album: Option<String>,
    seconds: Option<f64>,
}

#[derive(Default)]
struct State {
    config: Option<Config>,
    /// When the script was last read and whether it held a config: a key
    /// refused after a good read is final, one from before it isn't.
    read: Option<(Instant, bool)>,
    token: Option<Token>,
    /// Spotify refused what was fresh from its script, so it isn't asked
    /// again until signing in or a restart.
    withdrawn: bool,
    /// Bumped by `forget`, so a mint under way then doesn't keep its token.
    generation: u64,
}

pub struct Spotify {
    /// Kept across runs, so a run needn't fetch the script.
    config_file: PathBuf,
    state: Mutex<State>,
    /// One mint at a time, as each may read the script.
    minting: tokio::sync::Mutex<()>,
}

#[derive(Debug)]
pub enum CanvasError {
    /// Spotify took the cookie as signed out, so the app should forget it.
    SignedOut,
    Failed(String),
}

enum Fail {
    NotSession,
    SearchMoved,
    Parse(&'static str),
    /// For good this run: a key or hash fresh from the script refused, or a
    /// URL off the allowlist.
    Refused(String),
    Status(u16, &'static str),
    Network(String),
}

impl Fail {
    fn sentence(&self) -> String {
        match self {
            Fail::NotSession => "Spotify took the cookie as signed out.".into(),
            Fail::SearchMoved => "Spotify's search has moved.".into(),
            Fail::Parse(sentence) => (*sentence).into(),
            Fail::Refused(sentence) | Fail::Network(sentence) => sentence.clone(),
            Fail::Status(status, what) => format!("{what} answered {status}."),
        }
    }
}

impl From<Fail> for CanvasError {
    fn from(fail: Fail) -> Self {
        match fail {
            Fail::NotSession => CanvasError::SignedOut,
            fail => CanvasError::Failed(fail.sentence()),
        }
    }
}

impl Spotify {
    pub fn new(config_file: PathBuf) -> Self {
        Self {
            config_file,
            state: Mutex::default(),
            minting: tokio::sync::Mutex::const_new(()),
        }
    }

    /// The song's Canvas MP4 with the cookie's sp_dc value: Ok(None) when
    /// Spotify surely has none.
    pub async fn canvas(
        &self,
        client: &reqwest::Client,
        cookie: &str,
        title: &str,
        artists: &[String],
        album: Option<&str>,
        duration: Option<u32>,
    ) -> Result<Option<Url>, CanvasError> {
        if self.state().withdrawn {
            return Err(CanvasError::Failed(
                "Spotify refused Canvas earlier this run.".into(),
            ));
        }
        let Some(artist) = artists.first() else {
            return Ok(None);
        };
        let mut stale = None;
        let mut retried = false;
        let fail = loop {
            let session = match self.session(client, cookie, stale.as_ref()).await {
                Ok(session) => session,
                Err(fail) => break fail,
            };
            match lookup(client, &session, title, artist, album, duration).await {
                Ok(found) => return Ok(found),
                Err(Fail::SearchMoved) if !retried => stale = Some(session.config),
                // A token can lapse before its time, so a fresh one is tried.
                Err(Fail::Status(401, _)) if !retried => {
                    let lapsed = |token: &mut Token| token.bearer == session.token.bearer;
                    self.state().token.take_if(lapsed);
                }
                Err(fail) => break fail,
            }
            retried = true;
        };
        if matches!(
            fail,
            Fail::Refused(_) | Fail::Status(401, _) | Fail::SearchMoved
        ) {
            self.state().withdrawn = true;
        }
        Err(fail.into())
    }

    /// Mints a token with the cookie, failing unless Spotify answers it
    /// signed in; for the sign-in window.
    pub async fn check(&self, client: &reqwest::Client, cookie: &str) -> Result<(), CanvasError> {
        let _minting = self.minting.lock().await;
        self.mint(client, cookie, None).await?;
        self.state().withdrawn = false;
        Ok(())
    }

    /// Drops the token, as on signing in or out.
    pub fn forget(&self) {
        let mut state = self.state();
        state.token = None;
        state.withdrawn = false;
        state.generation += 1;
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The token held for this cookie, unless lapsed or under `stale`, a
    /// config Spotify's search no longer takes.
    fn cached(&self, cookie: &str, stale: Option<&Config>) -> Option<Session> {
        let state = self.state();
        let config = state.config.as_ref().filter(|&c| Some(c) != stale)?;
        let token = state
            .token
            .as_ref()
            .filter(|token| token.cookie == cookie && token.expires > SystemTime::now())?;
        Some(Session {
            config: config.clone(),
            token: token.clone(),
        })
    }

    async fn session(
        &self,
        client: &reqwest::Client,
        cookie: &str,
        stale: Option<&Config>,
    ) -> Result<Session, Fail> {
        if let Some(session) = self.cached(cookie, stale) {
            return Ok(session);
        }
        let _minting = self.minting.lock().await;
        // Another caller may have minted while this one waited.
        if let Some(session) = self.cached(cookie, stale) {
            return Ok(session);
        }
        self.mint(client, cookie, stale).await
    }

    /// Under `minting`.
    async fn mint(
        &self,
        client: &reqwest::Client,
        cookie: &str,
        stale: Option<&Config>,
    ) -> Result<Session, Fail> {
        let generation = self.state().generation;
        let Some(mut config) = self.config(client, stale).await? else {
            return Err(Fail::Refused(
                "Spotify refused its web player's search.".into(),
            ));
        };
        let token = loop {
            match token(client, Some(cookie), &config).await {
                // A code refused may mean a newer key.
                Err(Fail::Status(status @ (400 | 401 | 403), _)) => {
                    let Some(fresh) = self.config(client, Some(&config)).await? else {
                        // Refused with the script's own key: unauthorised
                        // means the cookie no longer signs in.
                        return Err(if status == 401 {
                            Fail::NotSession
                        } else {
                            Fail::Refused(format!(
                                "Spotify refused its web player's key ({status})."
                            ))
                        });
                    };
                    config = fresh;
                }
                token => break token?,
            }
        };
        let mut state = self.state();
        if state.generation == generation {
            state.token = Some(token.clone());
        }
        Ok(Session { config, token })
    }

    /// The config held unless `stale`, else last run's, else the script's;
    /// None when the script was just read and holds only `stale`.
    async fn config(
        &self,
        client: &reqwest::Client,
        stale: Option<&Config>,
    ) -> Result<Option<Config>, Fail> {
        {
            let mut state = self.state();
            if state.config.is_none() {
                state.config = self.load();
            }
            if let Some(config) = state.config.as_ref().filter(|&c| Some(c) != stale) {
                return Ok(Some(config.clone()));
            }
            if let Some((at, found)) = state.read
                && at.elapsed() < BACKOFF
            {
                return match found {
                    true => Ok(None),
                    false => Err(Fail::Parse(
                        "Spotify's player script was unreadable moments ago.",
                    )),
                };
            }
        }
        let read = self.scrape(client).await;
        let mut state = self.state();
        match read {
            Ok(fresh) => {
                state.read = Some((Instant::now(), true));
                state.config = Some(fresh.clone());
                Ok(Some(fresh).filter(|fresh| Some(fresh) != stale))
            }
            Err(fail) => {
                // Only a script without a key is waited out; a network or
                // server that failed may be back by the next song.
                if matches!(fail, Fail::Parse(_) | Fail::Refused(_)) {
                    state.read = Some((Instant::now(), false));
                }
                Err(fail)
            }
        }
    }

    fn load(&self) -> Option<Config> {
        serde_json::from_slice(&std::fs::read(&self.config_file).ok()?).ok()
    }

    /// The script's config, without downloading it again while the page
    /// names the one the held config came from.
    async fn scrape(&self, client: &reqwest::Client) -> Result<Config, Fail> {
        let page = fetch(client.get(PLAYER), "Spotify's web player", ANSWER_LIMIT).await?;
        let script = script_url(&String::from_utf8_lossy(&page))
            .ok_or(Fail::Parse("Spotify's player script wasn't found."))?;
        drop(page);
        if !motion_allowed(&script) {
            return Err(Fail::Refused(
                "Spotify's player script is on a host Lily doesn't use.".into(),
            ));
        }
        let held = self.state().config.clone();
        if let Some(held) = held.filter(|held| held.script == script.as_str()) {
            return Ok(held);
        }
        let body = fetch(
            client.get(script.clone()),
            "Spotify's player script",
            SCRIPT_LIMIT,
        )
        .await?;
        let config = Config {
            script: script.into(),
            ..config_in_script(&body)
                .ok_or_else(|| Fail::Refused("Spotify's player script held no key.".into()))?
        };
        drop(body);
        if let Some(dir) = self.config_file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let saved = serde_json::to_vec(&config)
            .map_err(std::io::Error::other)
            .and_then(|json| crate::settings::write_atomic(&self.config_file, &json));
        if let Err(e) = saved {
            log::warn!("couldn't save Spotify's player config: {e}");
        }
        Ok(config)
    }
}

/// Checks the status, and words a failure without the URL, which may carry a
/// token code.
async fn fetch(
    request: reqwest::RequestBuilder,
    what: &'static str,
    limit: usize,
) -> Result<Vec<u8>, Fail> {
    let unreachable = |e: String| Fail::Network(format!("{what} couldn't be reached: {e}"));
    let response = request
        .send()
        .await
        .map_err(|e| unreachable(e.without_url().to_string()))?;
    let status = response.status();
    if !status.is_success() {
        return Err(Fail::Status(status.as_u16(), what));
    }
    super::body(response, limit).await.map_err(unreachable)
}

/// A token for the cookie, or an anonymous one without.
async fn token(
    client: &reqwest::Client,
    cookie: Option<&str>,
    config: &Config,
) -> Result<Token, Fail> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let code = totp(&key_from_secret(&config.secret), now);
    let address = format!(
        "{PLAYER}api/token?reason=init&productType=web-player&totp={code}&totpServer={code}&totpVer={}",
        config.version
    );
    let mut request = client.get(address);
    if let Some(cookie) = cookie {
        request = request.header(reqwest::header::COOKIE, format!("sp_dc={cookie}"));
    }
    let unread = || Fail::Parse("Spotify's token answer couldn't be read.");
    let answer: Value =
        serde_json::from_slice(&fetch(request, "Spotify's token", ANSWER_LIMIT).await?)
            .map_err(|_| unread())?;
    if cookie.is_some() && answer["isAnonymous"].as_bool() == Some(true) {
        return Err(Fail::NotSession);
    }
    let bearer = answer["accessToken"].as_str().ok_or_else(unread)?;
    // Dropped a minute early, so none lapses mid-request.
    let expires = answer["accessTokenExpirationTimestampMs"]
        .as_f64()
        .and_then(|ms| Duration::try_from_secs_f64(ms / 1000.0).ok())
        .and_then(|since| UNIX_EPOCH.checked_add(since))
        .and_then(|at| at.checked_sub(Duration::from_secs(60)))
        .ok_or_else(unread)?;
    Ok(Token {
        bearer: bearer.to_owned(),
        expires,
        cookie: cookie.unwrap_or_default().to_owned(),
    })
}

/// The song's Canvas through `session`, None when Spotify surely has none.
async fn lookup(
    client: &reqwest::Client,
    session: &Session,
    title: &str,
    artist: &str,
    album: Option<&str>,
    duration: Option<u32>,
) -> Result<Option<Url>, Fail> {
    let found = search(client, &format!("{title} {artist}"), session).await?;
    let uris = matches(&found, title, artist, album, duration);
    if uris.is_empty() {
        return Ok(None);
    }
    canvas_for(client, &uris, &session.token.bearer).await
}

async fn search(
    client: &reqwest::Client,
    query: &str,
    session: &Session,
) -> Result<Vec<Candidate>, Fail> {
    let body = json!({
        "operationName": "searchSuggestions",
        "variables": {
            "query": query, "limit": 10, "numberOfTopResults": 10, "offset": 0,
            "includeAuthors": false, "includeEpisodeContentRatingsV2": false,
        },
        "extensions": {"persistedQuery": {"version": 1, "sha256Hash": session.config.search}},
    });
    let request = client
        .post(SEARCH)
        .bearer_auth(&session.token.bearer)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header("App-Platform", "WebPlayer")
        .body(body.to_string());
    let data = match fetch(request, "Spotify's search", ANSWER_LIMIT).await {
        // Spotify answers a hash it no longer knows with 412 "Invalid query hash".
        Err(Fail::Status(400 | 412, _)) => return Err(Fail::SearchMoved),
        data => data?,
    };
    let unread = || Fail::Parse("Spotify's search answer couldn't be read.");
    let answer: Value = serde_json::from_slice(&data).map_err(|_| unread())?;
    let moved = answer["errors"].as_array().is_some_and(|errors| {
        errors.iter().any(|error| {
            error["message"]
                .as_str()
                .is_some_and(|message| message.contains("PersistedQueryNotFound"))
        })
    });
    if moved {
        return Err(Fail::SearchMoved);
    }
    candidates(&answer).ok_or_else(unread)
}

/// The tracks among the top results; None for an answer in another shape, so
/// it isn't taken for none.
fn candidates(answer: &Value) -> Option<Vec<Candidate>> {
    let items = answer
        .pointer("/data/searchV2/topResultsV2/itemsV2")?
        .as_array()?;
    let text = |value: &Value, path: &str| value.pointer(path)?.as_str().map(str::to_owned);
    Some(
        items
            .iter()
            .filter_map(|item| {
                let track = item.pointer("/item/data")?;
                if track["__typename"].as_str() != Some("Track") {
                    return None;
                }
                let artists = track.pointer("/artists/items").and_then(Value::as_array);
                Some(Candidate {
                    uri: text(track, "/uri")?,
                    name: text(track, "/name")?,
                    artists: artists
                        .into_iter()
                        .flatten()
                        .filter_map(|artist| text(artist, "/profile/name"))
                        .collect(),
                    album: text(track, "/albumOfTrack/name"),
                    seconds: track
                        .pointer("/duration/totalMilliseconds")
                        .and_then(Value::as_f64)
                        .map(|ms| ms / 1000.0),
                })
            })
            .collect(),
    )
}

/// The tracks that surely are the song, its album's first: a wrong Canvas is
/// worse than none.
fn matches(
    candidates: &[Candidate],
    title: &str,
    artist: &str,
    album: Option<&str>,
    duration: Option<u32>,
) -> Vec<String> {
    let title = normalised(title);
    let artist = normalised(artist);
    // A name of only symbols would match every other such name.
    if title.is_empty() || artist.is_empty() {
        return Vec::new();
    }
    let sure: Vec<&Candidate> = candidates
        .iter()
        .filter(|candidate| {
            normalised(&candidate.name) == title
                && candidate
                    .artists
                    .iter()
                    .any(|name| credits(name).contains(&artist))
                && duration.is_none_or(|duration| {
                    candidate
                        .seconds
                        .is_some_and(|seconds| (seconds - f64::from(duration)).abs() <= 3.0)
                })
        })
        .collect();
    let album = album.map(normalised).filter(|album| !album.is_empty());
    let mut seen = HashSet::new();
    // On-album first; `seen` drops their second copy and Spotify's own repeats.
    sure.iter()
        .filter(|candidate| candidate.album.as_deref().map(normalised) == album)
        .chain(&sure)
        .filter(|candidate| seen.insert(candidate.uri.as_str()))
        .map(|candidate| candidate.uri.clone())
        .collect()
}

/// One request for every match; the first of them, in order, with a looping
/// video.
async fn canvas_for(
    client: &reqwest::Client,
    uris: &[String],
    bearer: &str,
) -> Result<Option<Url>, Fail> {
    let request = client
        .post(CANVASES)
        .bearer_auth(bearer)
        .header(reqwest::header::CONTENT_TYPE, "application/x-protobuf")
        .header(reqwest::header::ACCEPT, "application/protobuf")
        .body(canvas_request(uris));
    let canvases = canvases(&fetch(request, "Spotify's Canvas", ANSWER_LIMIT).await?)
        .ok_or(Fail::Parse("Spotify's Canvas answer couldn't be read."))?;
    Ok(uris.iter().find_map(|uri| canvases.get(uri)).cloned())
}

/// The value from what the user or the sign-in window gives: trims, strips
/// "sp_dc=", and refuses anything that could end or split a header.
pub fn cookie_value(text: &str) -> Option<String> {
    let text = text.trim();
    let value = text.strip_prefix("sp_dc=").unwrap_or(text);
    let safe = |byte: u8| (0x21..0x7f).contains(&byte) && byte != b';' && byte != b',';
    (!value.is_empty() && value.bytes().all(safe)).then(|| value.to_owned())
}

/// The web player's main script, as its page names it; its stylesheet of the
/// same name comes first.
fn script_url(html: &str) -> Option<Url> {
    let mut rest = html;
    while let Some(at) = rest.find(SCRIPT_PREFIX) {
        rest = rest.get(at + SCRIPT_PREFIX.len()..)?;
        let (name, after) = rest.split_at_checked(
            rest.find(|c: char| !c.is_ascii_alphanumeric())
                .unwrap_or(rest.len()),
        )?;
        if !name.is_empty() && after.starts_with(".js") {
            return Url::parse(&format!("{SCRIPT_PREFIX}{name}.js")).ok();
        }
        rest = after;
    }
    None
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// The newest of the script's `{secret:'…',version:N}` keys and the search's
/// hash.
fn config_in_script(script: &[u8]) -> Option<Config> {
    let mut best: Option<(u64, Vec<u8>)> = None;
    let mut rest = script;
    while let Some(at) = find(rest, b"secret:") {
        rest = rest.get(at + b"secret:".len()..)?;
        // Most are a variable's name, not a literal.
        let Some(&quote) = rest
            .first()
            .filter(|&&quote| quote == b'\'' || quote == b'"')
        else {
            continue;
        };
        let mut secret = Vec::new();
        let mut end = None;
        let mut bytes = rest.iter().enumerate().skip(1);
        while let Some((i, &byte)) = bytes.next() {
            if byte == quote {
                end = Some(i);
                break;
            }
            // An escaped character is taken as it is.
            let byte = match byte {
                b'\\' => match bytes.next() {
                    Some((_, &escaped)) => escaped,
                    None => break,
                },
                byte => byte,
            };
            secret.push(byte);
        }
        let Some(end) = end else { break };
        rest = rest.get(end + 1..).unwrap_or_default();
        let Some(after) = rest.strip_prefix(b",version:".as_slice()) else {
            continue;
        };
        let digits = after
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        let version = std::str::from_utf8(after.get(..digits)?)
            .ok()?
            .parse::<u64>()
            .ok();
        if let Some(version) = version
            && best.as_ref().is_none_or(|(newest, _)| version > *newest)
        {
            best = Some((version, secret));
        }
    }
    let (version, secret) = best?;
    let marker = br#""searchSuggestions","query",""#;
    let hash = script
        .get(find(script, marker)? + marker.len()..)?
        .get(..65)?;
    let (&closing, hex) = hash.split_last()?;
    let lower_hex = |byte: &u8| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte);
    (closing == b'"' && hex.iter().all(lower_hex)).then(|| Config {
        version,
        secret: String::from_utf8_lossy(&secret).into_owned(),
        search: String::from_utf8_lossy(hex).into_owned(),
        script: String::new(),
    })
}

/// As the web player derives it: each character's code XOR a rolling 9 to
/// 41, written out in decimal.
fn key_from_secret(secret: &str) -> Vec<u8> {
    secret
        .encode_utf16()
        .enumerate()
        .map(|(i, unit)| (unit ^ (i % 33 + 9) as u16).to_string())
        .collect::<String>()
        .into_bytes()
}

/// RFC 2104, as nothing else in the app needs an HMAC crate.
fn hmac_sha1(key: &[u8], message: &[u8]) -> [u8; 20] {
    let mut block = [0u8; 64];
    if key.len() > block.len() {
        block[..20].copy_from_slice(&Sha1::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let inner = Sha1::new()
        .chain_update(block.map(|byte| byte ^ 0x36))
        .chain_update(message)
        .finalize();
    Sha1::new()
        .chain_update(block.map(|byte| byte ^ 0x5c))
        .chain_update(inner)
        .finalize()
        .into()
}

/// RFC 6238 with SHA-1, 30 s steps and six digits.
fn totp(key: &[u8], unix_secs: u64) -> String {
    let mac = hmac_sha1(key, &(unix_secs / 30).to_be_bytes());
    // SHA-1 is 20 bytes, so the offset's four bytes are always within.
    let offset = usize::from(mac[19] & 0x0f);
    let code = u32::from_be_bytes([
        mac[offset],
        mac[offset + 1],
        mac[offset + 2],
        mac[offset + 3],
    ]) & 0x7fff_ffff;
    format!("{:06}", code % 1_000_000)
}

/// `EntityCanvazRequest`: an `Entity { entity_uri = 1 }` in field 1 for each
/// track.
fn canvas_request(uris: &[String]) -> Vec<u8> {
    uris.iter()
        .flat_map(|uri| field(1, &field(1, uri.as_bytes())))
        .collect()
}

/// `EntityCanvazResponse`'s looping videos by track; None when it isn't
/// protobuf. Spotify's 9:16 stills are passed over.
fn canvases(response: &[u8]) -> Option<HashMap<String, Url>> {
    let mut found = HashMap::new();
    for (number, value) in fields(response)? {
        let (1, Wire::Bytes(canvas)) = (number, value) else {
            continue;
        };
        let (mut video, mut uri, mut kind) = (None, None, 0);
        for field in fields(canvas)? {
            match field {
                (2, Wire::Bytes(bytes)) => video = Url::parse(&String::from_utf8_lossy(bytes)).ok(),
                (4, Wire::Varint(value)) => kind = value,
                (5, Wire::Bytes(bytes)) => uri = Some(String::from_utf8_lossy(bytes).into_owned()),
                _ => {}
            }
        }
        // 1 to 3 are videos; 0 is a still image and 4 a GIF.
        if let (Some(uri), Some(video)) = (uri, video)
            && (1..=3).contains(&kind)
            && video.path().ends_with(".mp4")
            && motion_allowed(&video)
        {
            found.entry(uri).or_insert(video);
        }
    }
    Some(found)
}

/// Just the protobuf Canvas needs; malformed or hostile bytes give None.
enum Wire<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
}

fn fields(message: &[u8]) -> Option<Vec<(u64, Wire<'_>)>> {
    let mut fields = Vec::new();
    let mut rest = message;
    while !rest.is_empty() {
        let key = varint(&mut rest)?;
        match key & 7 {
            0 => fields.push((key >> 3, Wire::Varint(varint(&mut rest)?))),
            1 => rest = rest.get(8..)?,
            5 => rest = rest.get(4..)?,
            2 => {
                let length = usize::try_from(varint(&mut rest)?).ok()?;
                let (value, after) = rest.split_at_checked(length)?;
                fields.push((key >> 3, Wire::Bytes(value)));
                rest = after;
            }
            _ => return None,
        }
    }
    Some(fields)
}

/// At most ten bytes, as a 64-bit value takes.
fn varint(rest: &mut &[u8]) -> Option<u64> {
    let mut value = 0;
    for shift in (0..70).step_by(7) {
        let (&byte, after) = rest.split_first()?;
        *rest = after;
        value |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            return Some(value);
        }
    }
    None
}

/// A length-delimited field, as every field Canvas is sent is.
fn field(number: u8, value: &[u8]) -> Vec<u8> {
    let mut data = vec![number << 3 | 2];
    let mut length = value.len();
    while length >= 0x80 {
        data.push((length & 0x7f) as u8 | 0x80);
        length >>= 7;
    }
    data.push(length as u8);
    data.extend_from_slice(value);
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    // Codes

    #[test]
    fn hmac_meets_rfc_2202() {
        let hex = |mac: [u8; 20]| mac.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let long = "Test Using Larger Than Block-Size Key";
        let first = format!("{long} - Hash Key First");
        let both = format!("{long} and Larger Than One Block-Size Data");
        let cases: [(&[u8], &[u8], &str); 4] = [
            (
                &[0x0b; 20],
                b"Hi There",
                "b617318655057264e28bc0b6fb378c8ef146be00",
            ),
            (
                b"Jefe",
                b"what do ya want for nothing?",
                "effcdf6ae5eb2fa2d27416d5f184df9c259a7c79",
            ),
            // Keys past a block are hashed first, as Spotify's derived ones are.
            (
                &[0xaa; 80],
                first.as_bytes(),
                "aa4ae5e15272d00e95705637ce8a3b55ed402112",
            ),
            (
                &[0xaa; 80],
                both.as_bytes(),
                "e8e99d0f45237d786d6bbaa7965c7808bbff1a91",
            ),
        ];
        for (key, message, mac) in cases {
            assert_eq!(hex(hmac_sha1(key, message)), mac);
        }
    }

    #[test]
    fn makes_rfc_6238s_sha1_codes() {
        let key = b"12345678901234567890";
        assert_eq!(totp(key, 59), "287082");
        // Zero-padded.
        assert_eq!(totp(key, 1_111_111_109), "081804");
    }

    #[test]
    fn derives_the_key_as_the_web_player_does() {
        // 65 ^ 9, 122 ^ 10, 33 ^ 11.
        assert_eq!(key_from_secret("Az!"), b"7211242");
        // The XOR rolls over after 33 characters.
        let long = String::from_utf8(key_from_secret(&"a".repeat(34))).unwrap();
        assert!(long.starts_with("104") && long.ends_with("72104"));
    }

    // Script

    fn hash() -> String {
        "0123456789abcdef".repeat(4)
    }

    #[test]
    fn reads_the_newest_key_and_the_search_hash_from_the_script() {
        let hash = hash();
        let script = format!(
            r#"x.generate({{secret:e,algorithm:t}});let ej=[{{secret:'q\'w',version:12}},{{secret:"a\\b\"c",version:15}},
            {{secret:'old',version:9}}].map(e=>e);let hJ=new hz.l("searchSuggestions","query","{hash}",null);"#
        );
        let expected = Config {
            version: 15,
            secret: r#"a\b"c"#.into(),
            search: hash.clone(),
            script: String::new(),
        };
        assert_eq!(config_in_script(script.as_bytes()), Some(expected));
        let single =
            format!(r#"{{secret:'q\'w\\',version:3}};("searchSuggestions","query","{hash}""#);
        assert_eq!(config_in_script(single.as_bytes()).unwrap().secret, r"q'w\");
    }

    #[test]
    fn wants_a_key_and_a_whole_lowercase_hash() {
        let hash = hash();
        let script = |key: &str, hash: &str| {
            format!(r#"{{secret:{key},version:3}};("searchSuggestions","query","{hash}""#)
        };
        assert_eq!(config_in_script(script("e", &hash).as_bytes()), None);
        assert_eq!(
            config_in_script(script("'k'", &hash.to_uppercase()).as_bytes()),
            None
        );
        assert_eq!(
            config_in_script(script("'k'", &hash[..63]).as_bytes()),
            None
        );
        assert_eq!(config_in_script(b"{secret:'unterminated"), None);
    }

    #[test]
    fn finds_the_main_script_in_the_page() {
        let page = r#"
            <link href="https://open.spotifycdn.com/cdn/build/web-player/web-player.9f8e7d6c.css"/>
            <script src="https://open.spotifycdn.com/cdn/build/web-player/web-player.1a2b3c4d.js"></script>
            "#;
        assert_eq!(
            script_url(page).unwrap().as_str(),
            "https://open.spotifycdn.com/cdn/build/web-player/web-player.1a2b3c4d.js"
        );
        assert_eq!(script_url("web-player.1a2b.css"), None);
    }

    #[test]
    fn reuses_the_last_runs_config() {
        let file = std::env::temp_dir().join(format!("lily-spotify-{}.json", std::process::id()));
        let saved = Config {
            version: 7,
            secret: "k".into(),
            search: hash(),
            script: "https://open.spotifycdn.com/cdn/build/web-player/web-player.1a.js".into(),
        };
        crate::settings::write_atomic(&file, &serde_json::to_vec(&saved).unwrap()).unwrap();
        let spotify = Spotify::new(file.clone());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        // No proxy or timeouts, so a request would fail rather than be needed.
        let config = runtime.block_on(spotify.config(&reqwest::Client::new(), None));
        let _ = std::fs::remove_file(&file);
        assert!(matches!(config, Ok(Some(config)) if config == saved));
    }

    // Protobuf

    #[test]
    fn encodes_the_canvas_request() {
        let uri = "spotify:track:a";
        let mut expected = vec![0x0A, 17, 0x0A, 15];
        expected.extend_from_slice(uri.as_bytes());
        assert_eq!(canvas_request(&[uri.into()]), expected);
        // Lengths past 127 take two bytes.
        assert_eq!(field(1, &[0; 200])[..3], [0x0A, 0xC8, 0x01]);
    }

    fn varint_field(number: u8, value: u8) -> Vec<u8> {
        vec![number << 3, value]
    }

    fn text(number: u8, value: &str) -> Vec<u8> {
        field(number, value.as_bytes())
    }

    fn canvas(uri: &str, kind: u8, url: &str, still: Option<&str>) -> Vec<u8> {
        // A fixed64 and a fixed32 field the reader must step over.
        let mut body = text(1, "id");
        body.extend([3 << 3 | 1, 7, 7, 7, 7, 7, 7, 7, 7, 10 << 3 | 5, 1, 2, 3, 4]);
        body.extend(text(2, url));
        body.extend(varint_field(4, kind));
        body.extend(text(5, uri));
        // A `Thumbnail`, which the reader steps over.
        if let Some(still) = still {
            let thumbnail = [varint_field(1, 120), varint_field(2, 60), text(3, still)].concat();
            body.extend(field(13, &thumbnail));
        }
        field(1, &body)
    }

    fn response() -> Vec<u8> {
        let canvaz = |name: &str| format!("https://canvaz.scdn.co/upload/{name}");
        [
            canvas("spotify:track:image", 0, &canvaz("a.jpg"), None),
            canvas(
                "spotify:track:song",
                3,
                &canvaz("b.cnvs.mp4"),
                Some("https://i.scdn.co/image/large"),
            ),
            canvas("spotify:track:other", 1, &canvaz("c.cnvs.mp4"), None),
            canvas(
                "spotify:track:foreign",
                2,
                "https://example.com/upload/d.cnvs.mp4",
                None,
            ),
            varint_field(2, 60),
        ]
        .concat()
    }

    #[test]
    fn reads_looping_videos_by_track_without_spotifys_stills() {
        let found = canvases(&response()).unwrap();
        // An image and a foreign host are passed over.
        let mut tracks: Vec<_> = found.keys().map(String::as_str).collect();
        tracks.sort_unstable();
        assert_eq!(tracks, ["spotify:track:other", "spotify:track:song"]);
        let song = found.get("spotify:track:song").map(Url::as_str);
        assert_eq!(song, Some("https://canvaz.scdn.co/upload/b.cnvs.mp4"));
    }

    #[test]
    fn reads_hostile_bytes_as_malformed_without_panicking() {
        let whole = response();
        for end in 0..whole.len() {
            let _ = canvases(&whole[..end]);
        }
        // A length past the end.
        assert_eq!(canvases(&[0x0A, 0x05, 0x01]), None);
        // A huge length.
        assert_eq!(
            canvases(&[[0x0A].as_slice(), &[0xFF; 9], &[0x01]].concat()),
            None
        );
        // A varint past ten bytes.
        assert_eq!(canvases(&[[0x08].as_slice(), &[0x80; 11]].concat()), None);
        // Groups aren't read.
        assert_eq!(canvases(&[0x0B, 0x0C]), None);
        // xorshift, as nothing else needs a random crate.
        let mut seed = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..500 {
            let length = next() % 64;
            let bytes: Vec<u8> = (0..length).map(|_| next() as u8).collect();
            let _ = canvases(&bytes);
        }
    }

    // Search and match

    #[test]
    fn reads_the_tracks_among_the_top_results() {
        let answer = r#"
            {"data":{"searchV2":{"topResultsV2":{"itemsV2":[
            {"item":{"data":{"__typename":"Album","uri":"spotify:album:x","name":"Song"}}},
            {"item":{"data":{"__typename":"Track","uri":"spotify:track:1","name":"Song",
              "artists":{"items":[{"profile":{"name":"The Band"}}]},"albumOfTrack":{"name":"Hits"},
              "duration":{"totalMilliseconds":201800}}}}]}}}}
            "#;
        let found = candidates(&serde_json::from_str(answer).unwrap()).unwrap();
        let [track] = found.as_slice() else {
            panic!("{} tracks", found.len());
        };
        assert_eq!(track.uri, "spotify:track:1");
        assert_eq!(track.artists, ["The Band"]);
        assert_eq!(track.album.as_deref(), Some("Hits"));
        assert_eq!(track.seconds, Some(201.8));
        // Another shape isn't none.
        assert!(candidates(&serde_json::from_str(r#"{"data":{}}"#).unwrap()).is_none());
    }

    #[test]
    fn matches_only_the_song_its_album_first() {
        let remaster = "Song - Remastered 2009";
        let candidate = |uri: &str, name: &str, artist: &str, album: &str, seconds| Candidate {
            uri: uri.into(),
            name: name.into(),
            artists: vec!["Guest".into(), artist.into()],
            album: Some(album.into()),
            seconds: Some(seconds),
        };
        let original = "Original Album";
        let found = [
            candidate(
                "compilation",
                "Song (Remastered 2009)",
                "The Band",
                "Greatest Hits",
                200.4,
            ),
            candidate("wrong-artist", remaster, "Other Band", original, 200.0),
            candidate("album", remaster, "The Band & Friend", original, 198.0),
            candidate("long", remaster, "The Band", original, 205.0),
            candidate("album", remaster, "The Band", original, 198.0),
            candidate("other-song", "Other Song", "The Band", original, 200.0),
        ];
        let on_album = matches(&found, remaster, "The Band", Some(original), Some(200));
        assert_eq!(on_album, ["album", "compilation"]);
        let any = matches(&found, remaster, "The Band", None, None);
        assert_eq!(any, ["compilation", "album", "long"]);
        assert!(matches(&found, "!!!", "The Band", None, None).is_empty());
    }

    #[test]
    fn takes_only_a_cookie_fit_for_a_header() {
        assert_eq!(
            cookie_value("  sp_dc=AQBx-y_z9  \n").as_deref(),
            Some("AQBx-y_z9")
        );
        assert_eq!(cookie_value("AQBx").as_deref(), Some("AQBx"));
        for bad in ["", "sp_dc=", "a b", "a;b", "a,b", "a\r\nX-Evil: 1", "é"] {
            assert_eq!(cookie_value(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn its_futures_can_run_on_the_runtime() {
        fn send<T: Send>(_: T) {}
        let spotify = Spotify::new(PathBuf::new());
        let client = reqwest::Client::new();
        send(spotify.canvas(&client, "", "", &[], None, None));
        send(spotify.check(&client, ""));
    }

    // Live

    /// Without an account: the config, an anonymous token (which proves the
    /// codes and the key), and the search and Canvas with it. Canvas answers
    /// an anonymous token with none at all, hence the cookie.
    #[tokio::test]
    #[ignore]
    async fn live_reads_the_web_player_anonymously() {
        let client = crate::http::motion_client();
        let file = std::env::temp_dir().join("lily-spotify-live-anonymous.json");
        let _ = std::fs::remove_file(&file);
        let config = Spotify::new(file.clone())
            .scrape(client)
            .await
            .unwrap_or_else(|f| panic!("{}", f.sentence()));
        let (version, length) = (config.version, config.secret.len());
        println!(
            "config: version {version}, {length}-character secret, search {}",
            config.search
        );
        let wrong = Config {
            secret: format!("{}x", config.secret),
            ..config.clone()
        };
        let answered = |result: Result<Token, Fail>| match result {
            Ok(_) => "a token".to_owned(),
            Err(fail) => fail.sentence(),
        };
        println!(
            "a wrong code: {}",
            answered(token(client, None, &wrong).await)
        );
        println!(
            "a junk sp_dc: {}",
            answered(token(client, Some("junk"), &config).await)
        );
        // The saved config's script is still the page's, so this re-reads
        // only the page.
        let written = || std::fs::metadata(&file).and_then(|m| m.modified()).ok();
        let before = written().expect("the config wasn't saved");
        let checked = Spotify::new(file.clone()).check(client, "junk").await;
        assert_eq!(written(), Some(before), "the script was downloaded again");
        let _ = std::fs::remove_file(&file);
        assert!(
            matches!(checked, Err(CanvasError::SignedOut)),
            "{checked:?}"
        );
        let token = token(client, None, &config)
            .await
            .unwrap_or_else(|f| panic!("{}", f.sentence()));
        let left = token
            .expires
            .duration_since(SystemTime::now())
            .unwrap_or_default();
        println!("no cookie: a token, {} s left", left.as_secs());
        let session = Session { config, token };
        let uris = match search(client, "Snooze SZA", &session).await {
            Ok(found) => {
                for c in &found {
                    println!(
                        "  {} {:?} {:?} {:?} {:?}",
                        c.uri, c.name, c.artists, c.album, c.seconds
                    );
                }
                matches(&found, "Snooze", "SZA", Some("SOS"), Some(202))
            }
            Err(fail) => {
                println!("search: {}", fail.sentence());
                Vec::new()
            }
        };
        println!("matches: {uris:?}");
        // Snooze's own, so Canvas is asked whatever the search answered.
        let uris = match uris.is_empty() {
            true => vec!["spotify:track:4iZ4pt7kvcaH6Yo8UoZ4s2".to_owned()],
            false => uris,
        };
        match canvas_for(client, &uris, &session.token.bearer).await {
            Ok(Some(url)) => save(client, url).await,
            Ok(None) => println!("Canvas: none for {uris:?}"),
            Err(fail) => println!("Canvas: {}", fail.sentence()),
        }
    }

    /// The whole path with the cookie in `LILY_SP_DC`, skipped without one.
    #[tokio::test]
    #[ignore]
    async fn live_finds_snoozes_canvas() {
        let Some(cookie) = std::env::var("LILY_SP_DC")
            .ok()
            .as_deref()
            .and_then(cookie_value)
        else {
            return println!("skipped: no LILY_SP_DC");
        };
        let client = crate::http::motion_client();
        let file = std::env::temp_dir().join("lily-spotify-live.json");
        let spotify = Spotify::new(file.clone());
        let checked = spotify.check(client, &cookie).await;
        assert!(checked.is_ok(), "{checked:?}");
        let artists = ["SZA".to_owned()];
        let found = spotify
            .canvas(client, &cookie, "Snooze", &artists, Some("SOS"), Some(202))
            .await;
        let _ = std::fs::remove_file(&file);
        match found {
            Ok(Some(url)) => save(client, url).await,
            other => panic!("Snooze: {other:?}"),
        }
    }

    /// Into the temporary folder, saying only its host and size.
    async fn save(client: &reqwest::Client, url: Url) {
        println!("Canvas: on {:?}", url.host_str());
        let clip = fetch(client.get(url), "the clip", 64 << 20).await;
        let clip = clip.unwrap_or_else(|f| panic!("{}", f.sentence()));
        let dir = std::env::temp_dir().join("lily-motion-sample");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("canvas.mp4"), &clip).unwrap();
        println!("clip: {} bytes", clip.len());
    }
}
