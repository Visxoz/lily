//! Submitting listens to ListenBrainz, with the user token from the
//! user's ListenBrainz settings.

use reqwest::{RequestBuilder, header};
use serde::{Deserialize, Serialize};

use crate::scrobbling::{Error, Play, Session, USER_AGENT, failed};

const SUBMIT_URL: &str = "https://api.listenbrainz.org/1/submit-listens";
const VALIDATE_URL: &str = "https://api.listenbrainz.org/1/validate-token";

#[derive(Serialize)]
struct Submission<'a> {
    listen_type: &'static str,
    payload: &'a [Listen<'a>],
}

#[derive(Serialize)]
struct Listen<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    listened_at: Option<i64>,
    track_metadata: Metadata<'a>,
}

#[derive(Serialize)]
struct Metadata<'a> {
    artist_name: &'a str,
    track_name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    release_name: Option<&'a str>,
    additional_info: Info,
}

#[derive(Serialize)]
struct Info {
    media_player: &'static str,
    submission_client: &'static str,
    submission_client_version: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    music_service: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_ms: Option<u64>,
}

impl<'a> Listen<'a> {
    fn new(play: &'a Play, listened_at: Option<i64>) -> Self {
        Self {
            listened_at,
            track_metadata: Metadata {
                artist_name: &play.artist,
                track_name: &play.track,
                release_name: play.album.as_deref(),
                additional_info: Info {
                    media_player: "Lily",
                    submission_client: "Lily",
                    submission_client_version: env!("CARGO_PKG_VERSION"),
                    music_service: (!play.local).then_some("music.youtube.com"),
                    // Unknown rather than zero, as a tag under half a second reads.
                    duration_ms: play
                        .duration
                        .filter(|&secs| secs > 0)
                        .map(|secs| u64::from(secs) * 1000),
                },
            },
        }
    }
}

/// Sends `request` with `token`, returning the body of a success.
async fn call(request: RequestBuilder, token: &str) -> Result<String, Error> {
    #[derive(Deserialize)]
    struct Failure {
        error: String,
    }
    let mut authorisation =
        header::HeaderValue::from_str(&format!("Token {token}")).map_err(failed)?;
    authorisation.set_sensitive(true);
    let response = request
        .header(header::AUTHORIZATION, authorisation)
        .header(header::USER_AGENT, USER_AGENT)
        .timeout(crate::scrobbling::TIMEOUT)
        .send()
        .await
        .map_err(failed)?;
    let status = response.status();
    let body = response.text().await.map_err(failed)?;
    if status.is_success() {
        return Ok(body);
    }
    let reason = serde_json::from_str::<Failure>(&body).map_or_else(|_| String::new(), |f| f.error);
    let message = format!("ListenBrainz answered {status}: {reason}");
    Err(match status.as_u16() {
        401 => Error::SignedOut,
        // Only the listens it can't read; rate limits and outages wait.
        400 => Error::Rejected(message),
        _ => Error::Failed(message),
    })
}

async fn send(
    session: &Session,
    listen_type: &'static str,
    payload: &[Listen<'_>],
) -> Result<(), Error> {
    let body = serde_json::to_vec(&Submission {
        listen_type,
        payload,
    })
    .map_err(failed)?;
    let request = crate::http::client()
        .post(SUBMIT_URL)
        .header(header::CONTENT_TYPE, "application/json")
        .body(body);
    call(request, &session.key).await.map(drop)
}

/// Checks a token pasted by the user, returning the account it is for.
pub async fn validate(token: String) -> Result<Session, Error> {
    #[derive(Deserialize)]
    struct Reply {
        valid: bool,
        #[serde(default)]
        user_name: String,
    }
    // Tokens are UUIDs; anything else is refused before it reaches a header.
    if token.is_empty()
        || !token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(Error::SignedOut);
    }
    let request = crate::http::client().get(VALIDATE_URL);
    let reply: Reply = serde_json::from_str(&call(request, &token).await?).map_err(failed)?;
    if !reply.valid || reply.user_name.is_empty() {
        return Err(Error::SignedOut);
    }
    Ok(Session {
        name: reply.user_name,
        key: token,
    })
}

pub async fn now_playing(session: &Session, play: &Play) -> Result<(), Error> {
    send(session, "playing_now", &[Listen::new(play, None)]).await
}

pub async fn scrobble(session: &Session, plays: &[Play]) -> Result<(), Error> {
    let listen_type = if plays.len() == 1 { "single" } else { "import" };
    let payload: Vec<_> = plays
        .iter()
        .map(|play| Listen::new(play, Some(play.timestamp)))
        .collect();
    send(session, listen_type, &payload).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_a_listen_as_listenbrainz_reads_it() {
        let play = Play {
            artist: "Artist".into(),
            track: "Track".into(),
            album: None,
            duration: Some(185),
            timestamp: 1_700_000_000,
            local: false,
        };
        let json = serde_json::to_value(Submission {
            listen_type: "single",
            payload: &[Listen::new(&play, Some(play.timestamp))],
        })
        .unwrap();
        let listen = &json["payload"][0];
        assert_eq!(listen["listened_at"], 1_700_000_000);
        let metadata = &listen["track_metadata"];
        assert_eq!(metadata["artist_name"], "Artist");
        assert_eq!(metadata["track_name"], "Track");
        assert!(metadata.get("release_name").is_none(), "no album, no key");
        let info = &metadata["additional_info"];
        assert_eq!(info["duration_ms"], 185_000);
        assert_eq!(info["music_service"], "music.youtube.com");

        let local = Play {
            duration: Some(0),
            local: true,
            ..play
        };
        let now = serde_json::to_value(Listen::new(&local, None)).unwrap();
        assert!(now.get("listened_at").is_none(), "playing now has no time");
        let info = &now["track_metadata"]["additional_info"];
        assert!(info.get("music_service").is_none(), "a local file");
        assert!(info.get("duration_ms").is_none(), "no zero length");
    }
}
