//! Scrobbling to Last.fm. The key and secret are built in from
//! `LASTFM_API_KEY` and `LASTFM_API_SECRET`; a build without them hides it.

use md5::{Digest, Md5};
use serde::{Deserialize, de::DeserializeOwned};

use crate::scrobbling::{Error, Play, Session, failed};

const API_URL: &str = "https://ws.audioscrobbler.com/2.0/";
const AUTH_URL: &str = "https://www.last.fm/api/auth/";
const API_KEY: Option<&str> = option_env!("LASTFM_API_KEY");
const API_SECRET: Option<&str> = option_env!("LASTFM_API_SECRET");

fn keys() -> Option<(&'static str, &'static str)> {
    let (key, secret) = (API_KEY?, API_SECRET?);
    (!key.is_empty() && !secret.is_empty()).then_some((key, secret))
}

pub fn available() -> bool {
    keys().is_some()
}

type Params = Vec<(String, String)>;

/// Adds `api_sig`: the MD5 of every parameter's name and value in name
/// order, then the secret, as Last.fm requires of every signed call.
fn sign(params: &mut Params, secret: &str) {
    params.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let mut md5 = Md5::new();
    for (name, value) in params.iter() {
        md5.update(name);
        md5.update(value);
    }
    md5.update(secret);
    params.push(("api_sig".into(), format!("{:x}", md5.finalize())));
}

#[derive(Deserialize)]
struct Failure {
    error: u32,
    #[serde(default)]
    message: String,
}

async fn call<T: DeserializeOwned>(method: &str, mut params: Params) -> Result<T, Error> {
    let (key, secret) = keys().ok_or_else(|| failed("Last.fm isn't available in this build"))?;
    params.push(("method".into(), method.into()));
    params.push(("api_key".into(), key.into()));
    sign(&mut params, secret);
    // Unsigned, as Last.fm leaves it out of the signature.
    params.push(("format".into(), "json".into()));
    let response = crate::http::client()
        .post(API_URL)
        .header(reqwest::header::USER_AGENT, crate::scrobbling::USER_AGENT)
        .form(&params)
        .timeout(crate::scrobbling::TIMEOUT)
        .send()
        .await
        .map_err(failed)?;
    let status = response.status();
    let text = response.text().await.map_err(failed)?;
    // Errors come as `{"error": code, "message": ...}`, usually on a 4xx
    // status but not always, so the body is read before the status.
    if let Ok(failure) = serde_json::from_str::<Failure>(&text) {
        let message = format!("Last.fm error {}: {}", failure.error, failure.message);
        return Err(match failure.error {
            // An invalid or expired token: connecting has to start again.
            4 | 15 if method.starts_with("auth.") => Error::Rejected(message),
            // Invalid parameters.
            6 => Error::Rejected(message),
            // Invalid session key.
            9 => Error::SignedOut,
            // Unauthorised token.
            14 => Error::NotYetAllowed,
            // Anything else, 4 on a scrobble among them, is about the account
            // or Last.fm rather than the play, so the plays wait for later.
            _ => Error::Failed(message),
        });
    }
    // Something other than Last.fm answering, as a gateway's JSON error,
    // which would otherwise pass for a scrobble taken.
    if !status.is_success() {
        return Err(failed(format!("Last.fm answered {status}")));
    }
    serde_json::from_str(&text).map_err(failed)
}

/// Starts connecting an account, returning the page where the user allows
/// access and the token to pass to `session` once they have.
pub async fn authorise() -> Result<(url::Url, String), Error> {
    #[derive(Deserialize)]
    struct Token {
        token: String,
    }
    let Token { token } = call("auth.getToken", Params::new()).await?;
    let (key, _) = keys().ok_or_else(|| failed("Last.fm isn't available in this build"))?;
    let page = url::Url::parse_with_params(AUTH_URL, [("api_key", key), ("token", &token)])
        .map_err(failed)?;
    Ok((page, token))
}

/// The session for `token`, once the user has allowed access.
pub async fn session(token: &str) -> Result<Session, Error> {
    #[derive(Deserialize)]
    struct Reply {
        session: Session,
    }
    let params = vec![("token".into(), token.into())];
    Ok(call::<Reply>("auth.getSession", params).await?.session)
}

/// The parameters describing `play`, with `suffix` after each name, as
/// `[0]` in a batch.
fn push_play(params: &mut Params, play: &Play, suffix: &str) {
    params.push((format!("artist{suffix}"), play.artist.clone()));
    params.push((format!("track{suffix}"), play.track.clone()));
    if let Some(album) = &play.album {
        params.push((format!("album{suffix}"), album.clone()));
    }
    if let Some(duration) = play.duration {
        params.push((format!("duration{suffix}"), duration.to_string()));
    }
}

pub async fn now_playing(session: &Session, play: &Play) -> Result<(), Error> {
    let mut params = vec![("sk".into(), session.key.clone())];
    push_play(&mut params, play, "");
    call::<serde::de::IgnoredAny>("track.updateNowPlaying", params)
        .await
        .map(drop)
}

pub async fn scrobble(session: &Session, plays: &[Play]) -> Result<(), Error> {
    let mut params = vec![("sk".into(), session.key.clone())];
    for (i, play) in plays.iter().enumerate() {
        let suffix = format!("[{i}]");
        push_play(&mut params, play, &suffix);
        params.push((format!("timestamp{suffix}"), play.timestamp.to_string()));
    }
    let reply: serde_json::Value = call("track.scrobble", params).await?;
    // Ignored plays will never be taken, so they go like the others, but
    // are logged so missing scrobbles can be explained.
    for reason in ignored(&reply) {
        log::warn!("Last.fm ignored a play: {reason}");
    }
    Ok(())
}

/// Why Last.fm ignored each play in a `track.scrobble` reply it ignored:
/// an artist or track it filters, a timestamp too old or new, the daily
/// limit. One play comes back as an object and more as a list, with the
/// codes as strings.
fn ignored(reply: &serde_json::Value) -> Vec<String> {
    let plays = &reply["scrobbles"]["scrobble"];
    let plays = plays
        .as_array()
        .map_or(std::slice::from_ref(plays), Vec::as_slice);
    plays
        .iter()
        .filter_map(|play| {
            let message = &play["ignoredMessage"];
            let code = match &message["code"] {
                serde_json::Value::String(code) => code.parse().ok(),
                code => code.as_u64(),
            }?;
            let text = message["#text"].as_str().unwrap_or_default();
            (code != 0).then(|| format!("code {code}: {text}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signs_parameters_in_name_order() {
        let mut params: Params = vec![
            ("token".into(), "tok\u{e9}".into()),
            ("method".into(), "auth.getSession".into()),
            ("api_key".into(), "key".into()),
        ];
        sign(&mut params, "secret");
        assert_eq!(
            params.last(),
            Some(&(
                "api_sig".to_owned(),
                "d918bc2b2785863c1487e7a79435acb2".to_owned()
            )),
            "md5 of api_keykeymethodauth.getSessiontokentok\u{e9}secret"
        );
    }

    #[test]
    fn reads_why_plays_were_ignored() {
        let one: serde_json::Value = serde_json::from_str(
            r##"{"scrobbles":{"scrobble":{"ignoredMessage":{"code":"5","#text":"Daily scrobble limit exceeded"}},"@attr":{"accepted":0,"ignored":1}}}"##,
        )
        .unwrap();
        assert_eq!(ignored(&one), ["code 5: Daily scrobble limit exceeded"]);

        let many: serde_json::Value = serde_json::from_str(
            r##"{"scrobbles":{"scrobble":[{"ignoredMessage":{"code":"0","#text":""}},{"ignoredMessage":{"code":"3","#text":"Timestamp too old"}}]}}"##,
        )
        .unwrap();
        assert_eq!(ignored(&many), ["code 3: Timestamp too old"]);

        assert!(ignored(&serde_json::json!({})).is_empty(), "an odd reply");
    }
}
