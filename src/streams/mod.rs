//! Resolves stream URLs from YouTube's player endpoint, asking as clients that
//! get plain, unscrambled URLs. They are tried in order, as YouTube tends to
//! break one client at a time. Premium streams go through `premium`.

mod head;
pub mod premium;
mod response;

use std::sync::{Arc, Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use serde_json::json;

pub use head::warm;
use response::parse;
pub(crate) use response::{Format, PlayerResponse, best_audio, parse_response};

/// A client YouTube hands plain URLs to, as yt-dlp describes it.
struct Client {
    name: &'static str,
    id: &'static str,
    version: &'static str,
    user_agent: &'static str,
    device_make: &'static str,
    device_model: &'static str,
    os_name: &'static str,
    os_version: &'static str,
    android_sdk: Option<u32>,
}

/// In order of trial. The first is the usual one; the rest only answer
/// once it fails, so they cost nothing while it works.
const CLIENTS: &[Client] = &[
    Client {
        name: "VISIONOS",
        id: "101",
        version: "1.02",
        user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 15_7_3) AppleWebKit/605.1.15 \
            (KHTML, like Gecko) Version/26.0 Safari/605.1.15",
        device_make: "Apple",
        device_model: "RealityDevice17,1",
        os_name: "visionOS",
        os_version: "26.5.23O471",
        android_sdk: None,
    },
    Client {
        name: "ANDROID_VR",
        id: "28",
        version: "1.65.10",
        user_agent: "com.google.android.apps.youtube.vr.oculus/1.65.10 (Linux; U; Android 12L; \
            eureka-user Build/SQ3A.220605.009.A1) gzip",
        device_make: "Oculus",
        device_model: "Quest 3",
        os_name: "Android",
        os_version: "12L",
        android_sdk: Some(32),
    },
    Client {
        name: "IOS",
        id: "5",
        version: "20.10.4",
        user_agent: "com.google.ios.youtube/20.10.4 (iPhone16,2; U; CPU iOS 18_3_2 like Mac OS X;)",
        device_make: "Apple",
        device_model: "iPhone16,2",
        os_name: "iPhone",
        os_version: "18.3.2.22D82",
        android_sdk: None,
    },
];

pub const CLIENT_COUNT: usize = CLIENTS.len();

/// Indices in `CLIENTS` whose URLs googlevideo cuts off past the first
/// megabyte, as where it wants a PO token, refused longest ago first.
static CUT_OFF: Mutex<Vec<usize>> = Mutex::new(Vec::new());

/// Asks `client` last for the rest of the session.
pub fn cut_off(client: &str) {
    let Some(i) = CLIENTS.iter().position(|c| c.name == client) else {
        return;
    };
    if mark(&mut CUT_OFF.lock().unwrap(), i) {
        log::warn!("{client} streams are cut off here; asking other clients first from now on");
    }
}

pub fn is_cut_off(client: &str) -> bool {
    let cut = CUT_OFF.lock().unwrap();
    CLIENTS
        .iter()
        .position(|c| c.name == client)
        .is_some_and(|i| cut.contains(&i))
}

/// Moves client `i` to the back of `cut`, returning whether it is new there.
fn mark(cut: &mut Vec<usize>, i: usize) -> bool {
    let new = !cut.contains(&i);
    cut.retain(|&c| c != i);
    cut.push(i);
    new
}

/// The clients not cut off, then those that are, as one may still resolve a
/// song the others refuse; a download walking on from refusal to refusal
/// reaches each before coming back to any.
fn clients(cut: &[usize]) -> Vec<&'static Client> {
    (0..CLIENTS.len())
        .filter(|i| !cut.contains(i))
        .chain(cut.iter().copied())
        .map(|i| &CLIENTS[i])
        .collect()
}

/// Saved with a download without what only streaming needs, so it plays
/// from the file as it was resolved.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResolvedStream {
    #[serde(skip)]
    pub url: String,
    pub size: u64,
    pub mime: String,
    #[serde(skip)]
    pub headers: Vec<(String, String)>,
    pub codec: String,
    pub bitrate_kbps: f64,
    pub duration: Option<f64>,
    pub loudness_db: Option<f32>,
    /// Not set for free formats the web client gives a Premium account, nor
    /// for a file, which has nothing to retry.
    #[serde(skip)]
    pub premium: bool,
    /// YouTube's format number: 774 and 141 are the Premium ones.
    pub itag: u32,
    /// The file it plays from instead, downloaded or the user's own.
    #[serde(skip)]
    pub file: Option<std::path::PathBuf>,
    /// The plain-URL client it came from, if one.
    #[serde(skip)]
    pub client: Option<&'static str>,
    /// Its first bytes, once `warm` has them, for its download to start from.
    #[serde(skip)]
    pub head: Arc<OnceLock<Vec<u8>>>,
}

/// With `itag`, that format only, so a download can carry on from another
/// client's URL. The first client's error is reported when none works, as
/// it is the usual one.
pub async fn resolve(
    video_id: &str,
    visitor_data: &str,
    max_kbps: Option<u64>,
    itag: Option<u32>,
) -> Result<ResolvedStream, String> {
    let mut first_error = None;
    let clients = clients(&CUT_OFF.lock().unwrap());
    for client in clients {
        match resolve_with(client, video_id, visitor_data, max_kbps, itag).await {
            Ok(stream) => {
                if first_error.is_some() {
                    log::info!("{video_id}: the {} client served the stream", client.name);
                }
                return Ok(stream);
            }
            Err(e) => {
                log::debug!("{video_id}: {} client: {e}", client.name);
                first_error.get_or_insert(e);
            }
        }
    }
    Err(first_error.unwrap_or_else(|| "no clients".into()))
}

async fn resolve_with(
    client: &Client,
    video_id: &str,
    visitor_data: &str,
    max_kbps: Option<u64>,
    itag: Option<u32>,
) -> Result<ResolvedStream, String> {
    let mut context = json!({
        "clientName": client.name,
        "clientVersion": client.version,
        "deviceMake": client.device_make,
        "deviceModel": client.device_model,
        "userAgent": client.user_agent,
        "osName": client.os_name,
        "osVersion": client.os_version,
        "hl": "en",
        "visitorData": visitor_data,
    });
    if let Some(sdk) = client.android_sdk {
        context["androidSdkVersion"] = sdk.into();
    }
    let body = json!({
        "context": { "client": context },
        "videoId": video_id,
        "contentCheckOk": true,
        "racyCheckOk": true,
    });
    let request = player_post(
        "https://www.youtube.com",
        client.user_agent,
        (client.id, client.version),
        visitor_data,
    )
    .body(body.to_string());
    let text = crate::http::text(request, "player request failed").await?;
    let stream = parse(&text, client.user_agent, max_kbps, itag)?;
    Ok(ResolvedStream {
        client: Some(client.name),
        ..stream
    })
}

/// A player request from `origin` as the client numbered and versioned by
/// `client`, as innertube wants it.
fn player_post(
    origin: &str,
    user_agent: &str,
    (id, version): (&str, &str),
    visitor_data: &str,
) -> reqwest::RequestBuilder {
    crate::http::client()
        .post(format!("{origin}/youtubei/v1/player?prettyPrint=false"))
        .header(reqwest::header::USER_AGENT, user_agent)
        .header("Content-Type", "application/json")
        .header("Origin", origin)
        .header("X-YouTube-Client-Name", id)
        .header("X-YouTube-Client-Version", version)
        .header("X-Goog-Visitor-Id", visitor_data)
}

#[cfg(test)]
mod tests {
    use super::head::first_bytes;
    use super::*;

    #[test]
    fn asks_cut_off_clients_last_refused_longest_ago_first() {
        let names = |cut: &[usize]| clients(cut).iter().map(|c| c.name).collect::<Vec<_>>();
        assert_eq!(names(&[]), ["VISIONOS", "ANDROID_VR", "IOS"]);
        assert_eq!(names(&[1]), ["VISIONOS", "IOS", "ANDROID_VR"]);

        // Only iOS works, and was refused once before: a download walking
        // on from visionOS and Android VR still reaches it.
        let mut cut = vec![2];
        assert!(mark(&mut cut, 0));
        assert_eq!(names(&cut)[0], "ANDROID_VR");
        assert!(mark(&mut cut, 1));
        assert_eq!(names(&cut), ["IOS", "VISIONOS", "ANDROID_VR"]);
        assert!(!mark(&mut cut, 2), "already marked");
        assert_eq!(cut, [0, 1, 2], "but now refused last");
    }

    /// Checks every plain-URL client and the Premium player trim against
    /// YouTube itself; the canary workflow runs it daily.
    #[test]
    #[ignore = "talks to YouTube; run by the canary workflow"]
    fn canary_every_stream_path_still_works() {
        // Never going away.
        const VIDEO: &str = "dQw4w9WgXcQ";
        let rt = tokio::runtime::Runtime::new().expect("runtime starts");
        let failures = rt.block_on(async {
            let mut failures = Vec::new();
            let api = crate::api::Api::new().expect("client starts");
            let visitor = match api.visitor_data(true).await {
                Ok(visitor) => visitor,
                Err(e) => return vec![format!("visitor data: {e}")],
            };
            let mut whole = 0;
            for client in CLIENTS {
                match resolve_with(client, VIDEO, &visitor, None, None).await {
                    Ok(stream) => match probe(&stream).await {
                        Ok(()) => {
                            eprint!(
                                "{}: {} {} kbps, serves bytes",
                                client.name,
                                stream.codec,
                                stream.bitrate_kbps.round()
                            );
                            // Where YouTube wants a PO token, URLs 403 after a megabyte.
                            match probe_past_head(&stream).await {
                                Ok(()) => {
                                    whole += 1;
                                    eprintln!(" past the first megabyte too");
                                }
                                Err(e) => eprintln!(", but {e} (cut off here)"),
                            }
                        }
                        Err(e) => failures.push(format!("{}: resolved but {e}", client.name)),
                    },
                    Err(e) => failures.push(format!("{}: {e}", client.name)),
                }
            }
            // Not a failure: from a datacentre's addresses YouTube may want PO
            // tokens of every client, whatever works for listeners.
            if whole == 0 {
                eprintln!("every client is cut off past the first megabyte here");
            }
            match premium::load_current().await {
                Ok(player) => eprintln!("player {}: trims and solves", player.id),
                Err(e) => failures.push(format!("player: {e}")),
            }
            failures
        });
        assert!(failures.is_empty(), "\n{}", failures.join("\n"));
    }

    /// Fetches the first bytes of a stream, as a URL can resolve and still
    /// be refused by the CDN.
    async fn probe(stream: &ResolvedStream) -> Result<(), String> {
        let (url, headers) = (stream.url.clone(), stream.headers.clone());
        let bytes = tokio::task::spawn_blocking(move || first_bytes(&url, &headers))
            .await
            .map_err(|e| e.to_string())??;
        (!bytes.is_empty())
            .then_some(())
            .ok_or_else(|| "the CDN sent no bytes".to_owned())
    }

    /// Fetches a range past the first megabyte, which a cut-off URL refuses.
    async fn probe_past_head(stream: &ResolvedStream) -> Result<(), String> {
        let (url, headers) = (stream.url.clone(), stream.headers.clone());
        tokio::task::spawn_blocking(move || {
            let mut request = crate::http::blocking_client()
                .get(&url)
                .header(reqwest::header::RANGE, "bytes=1048576-1114111");
            for (name, value) in &headers {
                request = request.header(name, value);
            }
            let response = request
                .send()
                .and_then(|r| r.error_for_status())
                .map_err(|e| format!("refused past the first megabyte ({})", e.without_url()))?;
            let partial = response.status() == reqwest::StatusCode::PARTIAL_CONTENT;
            let bytes = response.bytes().map_err(|e| e.without_url().to_string())?;
            (partial && !bytes.is_empty())
                .then_some(())
                .ok_or_else(|| "sent nothing past the first megabyte".to_owned())
        })
        .await
        .map_err(|e| e.to_string())?
    }
}
