//! Reports plays and the time heard to YouTube, so they reach the
//! account's history and recommendations.

use std::{
    hash::{BuildHasher, BuildHasherDefault, DefaultHasher, Hasher, RandomState},
    sync::Mutex,
    time::{Duration, Instant},
};

use rustypipe::client::{ClientType, answered_signed_out};
use serde::Deserialize;
use serde_json::value::RawValue;
use url::Url;

use super::{Api, Result, err};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Player {
    playback_tracking: Option<Tracking>,
}

/// A player answer's `playbackTracking`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Tracking {
    videostats_playback_url: Option<TrackingUrl>,
    videostats_watchtime_url: Option<TrackingUrl>,
    #[serde(default)]
    videostats_scheduled_flush_walltime_seconds: Vec<u32>,
    videostats_default_flush_interval_seconds: Option<u32>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TrackingUrl {
    base_url: String,
}

fn tracking(json: &str) -> Option<Tracking> {
    serde_json::from_str::<Player>(json).ok()?.playback_tracking
}

/// Prefetched streams can resolve minutes before they play; tracking older
/// than this is asked for again.
const TRACKING_FRESH_FOR: Duration = Duration::from_secs(10 * 60);
/// Songs prefetched and then skipped leave theirs behind.
const TRACKING_LIMIT: usize = 32;

/// The tracking of signed-in web player answers the Premium path had, so
/// `report_play` needn't ask again. Each is taken once, so a replay asks.
static KEPT: Mutex<Kept> = Mutex::new(Kept(Vec::new()));

struct Entry {
    video_id: String,
    login: u64,
    at: Instant,
    tracking: Tracking,
}

/// Oldest first.
struct Kept(Vec<Entry>);

impl Kept {
    fn keep(&mut self, entry: Entry) {
        self.0.retain(|e| {
            e.video_id != entry.video_id
                && entry.at.saturating_duration_since(e.at) < TRACKING_FRESH_FOR
        });
        if self.0.len() >= TRACKING_LIMIT {
            self.0.remove(0);
        }
        self.0.push(entry);
    }

    /// `video_id`'s tracking, while fresh and for the login `cookie` is.
    fn take(&mut self, video_id: &str, cookie: &str, now: Instant) -> Option<Tracking> {
        let entry = self
            .0
            .remove(self.0.iter().position(|e| e.video_id == video_id)?);
        (Some(entry.login) == login(cookie)
            && now.saturating_duration_since(entry.at) < TRACKING_FRESH_FOR)
            .then_some(entry.tracking)
    }
}

/// Tells apart the logins cookies are for, without keeping the session's
/// secret.
fn login(cookie: &str) -> Option<u64> {
    crate::streams::premium::sapisid(cookie)
        .map(|id| BuildHasherDefault::<DefaultHasher>::default().hash_one(id))
}

/// What a play's report can reuse of `json`, a web player answer asked with
/// `cookie`, with its `tracking`. Not an answer YouTube gave signed out:
/// rustypipe's sign-in watch never sees this request, so it is checked here.
fn entry(
    video_id: &str,
    cookie: &str,
    json: &str,
    tracking: Option<Tracking>,
    at: Instant,
) -> Option<Entry> {
    let tracking = tracking.filter(|t| t.videostats_playback_url.is_some())?;
    if answered_signed_out(json) {
        return None;
    }
    Some(Entry {
        video_id: video_id.to_owned(),
        login: login(cookie)?,
        at,
        tracking,
    })
}

/// Keeps the tracking of the Premium path's answer for `video_id`, so the
/// song's play is reported without asking for it again. `tracking` is the
/// answer's `playbackTracking` as its parse left it.
pub fn keep_tracking(video_id: &str, cookie: &str, json: &str, tracking: Option<&RawValue>) {
    let tracking = tracking.and_then(|raw| serde_json::from_str(raw.get()).ok());
    if let Some(entry) = entry(video_id, cookie, json, tracking, Instant::now()) {
        KEPT.lock().unwrap().keep(entry);
    }
}

/// Signing in or out leaves nothing of the last session's to reuse.
pub fn forget_tracking() {
    KEPT.lock().unwrap().0.clear();
}

/// A client playback nonce: 16 random characters the web player sends
/// with each ping, from the same alphabet.
fn cpn() -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let state = RandomState::new();
    (0..16)
        .map(|i| {
            let mut hasher = state.build_hasher();
            hasher.write_usize(i);
            char::from(ALPHABET[(hasher.finish() % 64) as usize])
        })
        .collect()
}

/// A stats URL from the player response, for the play `cpn`.
fn stats_url(url: &str, cpn: &str) -> Result<Url> {
    let mut url = Url::parse(url).map_err(err)?;
    // It carries the sign-in cookie, so only to YouTube itself.
    if !crate::http::within(&url, &["youtube.com"]) {
        return Err("the stats URL is not on youtube.com".into());
    }
    url.query_pairs_mut()
        .append_pair("ver", "2")
        .append_pair("c", "WEB_REMIX")
        .append_pair("cpn", cpn);
    Ok(url)
}

/// Where a play's watch time goes, and when: the web player pings at the
/// scheduled seconds, then at the interval.
pub struct Watchtime {
    url: Url,
    scheduled: Vec<u32>,
    every: u32,
}

impl Watchtime {
    /// When ping `n` is due, since the play started, with ping `n - 1`
    /// sent at `sent`.
    pub fn due(&self, n: usize, sent: Duration) -> Duration {
        self.scheduled.get(n).map_or(
            sent + Duration::from_secs(self.every.max(1).into()),
            |&at| Duration::from_secs(at.into()),
        )
    }

    /// The ping reporting `watched`.
    pub fn url(&self, watched: &Watched) -> Url {
        let secs = |s: f64| format!("{s:.3}");
        let join = |end: fn(&(f64, f64)) -> f64| {
            let ends: Vec<String> = watched.segments.iter().map(|s| secs(end(s))).collect();
            ends.join(",")
        };
        let mut url = self.url.clone();
        {
            let mut query = url.query_pairs_mut();
            // The last ping may have nothing new to add.
            if !watched.segments.is_empty() {
                query
                    .append_pair("st", &join(|s| s.0))
                    .append_pair("et", &join(|s| s.1));
            }
            query
                .append_pair("cmt", &secs(watched.position))
                .append_pair("rt", &secs(watched.elapsed.as_secs_f64()))
                .append_pair("state", if watched.paused { "paused" } else { "playing" });
            if watched.last {
                query.append_pair("final", "1");
            }
        }
        url
    }
}

/// What a watch-time ping reports.
pub struct Watched {
    /// Stretches heard since the last ping, in song seconds.
    pub segments: Vec<(f64, f64)>,
    /// Where the sound got to.
    pub position: f64,
    /// Since the play started.
    pub elapsed: Duration,
    pub paused: bool,
    /// The play has ended.
    pub last: bool,
}

async fn ping(url: Url, cookie: &str) -> Result<()> {
    crate::http::client()
        .get(url)
        .header(
            reqwest::header::USER_AGENT,
            crate::streams::premium::WEB_USER_AGENT,
        )
        .header(reqwest::header::COOKIE, cookie)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        // Its URL carries the session's tracking tokens.
        .map_err(|e| err(e.without_url()))?;
    Ok(())
}

impl Api {
    /// Adds `video_id` to the signed-in account's history, as YouTube
    /// Music's web player does: the account's player response for the song
    /// carries a stats URL, which is then requested with the session's
    /// cookie. Returns where the play's watch time goes, should it say. A
    /// play the Premium path just resolved for this login reuses its answer's.
    pub async fn report_play(&self, video_id: &str) -> Result<Option<Watchtime>> {
        let cookie = self.cookie().ok_or("signed out")?;
        let kept = KEPT.lock().unwrap().take(video_id, &cookie, Instant::now());
        let tracking = match kept {
            Some(tracking) => tracking,
            None => {
                let json = self
                    .q()
                    .authenticated()
                    .raw(
                        ClientType::DesktopMusic,
                        "player",
                        &serde_json::json!({ "videoId": super::video_id(video_id)? }),
                    )
                    .await
                    .map_err(err)?;
                tracking(&json).ok_or("the player response has no playback tracking")?
            }
        };
        let playback = tracking
            .videostats_playback_url
            .ok_or("the player response has no playback stats URL")?;
        // Both pings carry the same nonce, which ties the watch time to the play.
        let cpn = cpn();
        ping(stats_url(&playback.base_url, &cpn)?, &cookie).await?;
        let Some(url) = tracking
            .videostats_watchtime_url
            .and_then(|u| stats_url(&u.base_url, &cpn).ok())
        else {
            return Ok(None);
        };
        let scheduled = match tracking.videostats_scheduled_flush_walltime_seconds {
            scheduled if scheduled.is_empty() => vec![10, 20, 30],
            scheduled => scheduled,
        };
        Ok(Some(Watchtime {
            url,
            scheduled,
            every: tracking
                .videostats_default_flush_interval_seconds
                .unwrap_or(40),
        }))
    }

    /// Sends a watch-time ping from `Watchtime::url`.
    pub async fn report_watchtime(&self, url: Url) -> Result<()> {
        let cookie = self.cookie().ok_or("signed out")?;
        ping(url, &cookie).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_stats_urls_and_schedule() {
        let json = r#"{"playabilityStatus":{"status":"OK"},"playbackTracking":{
            "videostatsPlaybackUrl":{"baseUrl":"https://s.youtube.com/api/stats/playback?ns=yt&docid=abc"},
            "videostatsWatchtimeUrl":{"baseUrl":"https://s.youtube.com/api/stats/watchtime?ns=yt"},
            "videostatsScheduledFlushWalltimeSeconds":[10,20,30],
            "videostatsDefaultFlushIntervalSeconds":40}}"#;
        let tracking = tracking(json).unwrap();
        assert_eq!(
            tracking.videostats_playback_url.unwrap().base_url,
            "https://s.youtube.com/api/stats/playback?ns=yt&docid=abc"
        );
        assert_eq!(
            tracking.videostats_watchtime_url.unwrap().base_url,
            "https://s.youtube.com/api/stats/watchtime?ns=yt"
        );
        assert_eq!(
            tracking.videostats_scheduled_flush_walltime_seconds,
            [10, 20, 30]
        );
        assert_eq!(tracking.videostats_default_flush_interval_seconds, Some(40));
        assert!(super::tracking(r#"{"playabilityStatus":{}}"#).is_none());
        assert!(super::tracking("not json").is_none());
    }

    const COOKIE: &str = "SID=a; SAPISID=one";
    const PLAYER: &str = r#"{"streamingData":{"adaptiveFormats":[]},"playbackTracking":{
        "videostatsPlaybackUrl":{"baseUrl":"https://s.youtube.com/api/stats/playback?ns=yt"}}}"#;

    /// As the Premium path's parse hands it over.
    fn answer(video_id: &str, cookie: &str, json: &str, at: Instant) -> Option<Entry> {
        entry(video_id, cookie, json, tracking(json), at)
    }

    #[test]
    fn reuses_the_premium_answers_tracking_once_for_the_same_login() {
        let now = Instant::now();
        let mut kept = Kept(Vec::new());
        let keep = |kept: &mut Kept, id: &str| kept.keep(answer(id, COOKIE, PLAYER, now).unwrap());
        keep(&mut kept, "a");
        let rotated = "SID=b; SAPISID=one";
        let taken = kept
            .take("a", rotated, now + Duration::from_secs(5))
            .unwrap();
        assert_eq!(
            taken.videostats_playback_url.unwrap().base_url,
            "https://s.youtube.com/api/stats/playback?ns=yt"
        );
        assert!(kept.take("a", COOKIE, now).is_none(), "a replay asks again");

        keep(&mut kept, "b");
        assert!(
            kept.take("b", "SAPISID=two", now).is_none(),
            "another login"
        );
        assert!(kept.take("b", COOKIE, now).is_none(), "and gone with it");
        keep(&mut kept, "c");
        assert!(kept.take("c", "SID=a", now).is_none(), "no login");
        keep(&mut kept, "d");
        assert!(
            kept.take("d", COOKIE, now + TRACKING_FRESH_FOR).is_none(),
            "stale"
        );
    }

    #[test]
    fn keeps_only_tracking_a_report_can_use() {
        let now = Instant::now();
        assert!(answer("a", "SID=a", PLAYER, now).is_none(), "no login");
        let no_stats = r#"{"playbackTracking":{"videostatsWatchtimeUrl":{"baseUrl":"https://s.youtube.com/x"}}}"#;
        assert!(answer("a", COOKIE, no_stats, now).is_none(), "no stats URL");
        let signed_out = r#"{"responseContext":{"serviceTrackingParams":[{"params":[
            {"key":"logged_in","value":"0"}]}]},
            "playbackTracking":{"videostatsPlaybackUrl":{"baseUrl":"https://s.youtube.com/x"}}}"#;
        assert!(
            answer("a", COOKIE, signed_out, now).is_none(),
            "answered signed out"
        );
    }

    #[test]
    fn keeps_only_so_many_tracking_answers() {
        let now = Instant::now();
        let mut kept = Kept(Vec::new());
        for n in 0..=TRACKING_LIMIT {
            let at = now + Duration::from_millis(n as u64);
            kept.keep(answer(&n.to_string(), COOKIE, PLAYER, at).unwrap());
        }
        assert!(kept.take("0", COOKIE, now).is_none(), "the oldest goes");
        assert!(
            kept.take(&TRACKING_LIMIT.to_string(), COOKIE, now)
                .is_some()
        );
    }

    #[test]
    fn sends_the_cookie_only_to_youtube() {
        assert!(stats_url("https://s.youtube.com/api/stats/playback", "x").is_ok());
        assert!(stats_url("https://evil.example/api/stats/playback", "x").is_err());
    }

    #[test]
    fn pings_on_the_schedule_then_at_the_interval() {
        let watch = Watchtime {
            url: Url::parse("https://s.youtube.com/api/stats/watchtime").unwrap(),
            scheduled: vec![10, 20, 30],
            every: 40,
        };
        let secs = Duration::from_secs;
        assert_eq!(watch.due(0, Duration::ZERO), secs(10));
        assert_eq!(
            watch.due(1, secs(5)),
            secs(20),
            "a pause ping keeps the schedule"
        );
        assert_eq!(watch.due(3, secs(31)), secs(71), "then 40 s after the last");
    }

    #[test]
    fn reports_each_stretch_heard() {
        let watch = Watchtime {
            url: stats_url("https://s.youtube.com/api/stats/watchtime?ns=yt", "nonce").unwrap(),
            scheduled: Vec::new(),
            every: 40,
        };
        let watched = |segments, last| Watched {
            segments,
            position: 64.25,
            elapsed: Duration::from_millis(20_500),
            paused: false,
            last,
        };
        let query = |watched| -> Vec<(String, String)> {
            watch.url(&watched).query_pairs().into_owned().collect()
        };
        let get = |query: &[(String, String)], k: &str| {
            query.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
        };
        let ping = query(watched(vec![(0.0, 12.5), (60.0, 64.25)], false));
        assert_eq!(get(&ping, "cpn").as_deref(), Some("nonce"));
        assert_eq!(get(&ping, "st").as_deref(), Some("0.000,60.000"));
        assert_eq!(get(&ping, "et").as_deref(), Some("12.500,64.250"));
        assert_eq!(get(&ping, "cmt").as_deref(), Some("64.250"));
        assert_eq!(get(&ping, "rt").as_deref(), Some("20.500"));
        assert_eq!(get(&ping, "state").as_deref(), Some("playing"));
        assert_eq!(get(&ping, "final"), None);

        let last = query(watched(Vec::new(), true));
        assert_eq!(get(&last, "final").as_deref(), Some("1"));
        assert_eq!(get(&last, "st"), None, "nothing new heard");
        assert_eq!(get(&last, "cmt").as_deref(), Some("64.250"));
    }

    #[test]
    fn makes_nonces_like_the_web_player() {
        let (a, b) = (cpn(), cpn());
        assert_eq!(a.len(), 16);
        assert!(
            a.bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        );
        assert_ne!(a, b, "random each time");
    }
}
