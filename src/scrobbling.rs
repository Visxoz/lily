//! What Last.fm and ListenBrainz share: the play, the rule for when it
//! counts, the errors, the account and the plays kept until taken.

use std::{collections::VecDeque, path::PathBuf, time::Duration};

use serde::{Deserialize, Serialize};

use crate::{lastfm, listenbrainz};

pub const USER_AGENT: &str = concat!("Lily/", env!("CARGO_PKG_VERSION"));
pub const TIMEOUT: Duration = Duration::from_secs(20);
/// Most plays sent in one request, Last.fm's limit.
const BATCH: usize = 50;
/// About a fortnight of heavy listening; Last.fm refuses older plays anyway.
const PENDING_LIMIT: usize = 3000;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Service {
    LastFm,
    ListenBrainz,
}

/// Last.fm's session key or a ListenBrainz token. Neither expires, but
/// either can be revoked (`Error::SignedOut`).
#[derive(Clone, Serialize, Deserialize)]
pub struct Session {
    pub name: String,
    pub key: String,
}

impl Service {
    pub const ALL: [Service; 2] = [Service::LastFm, Service::ListenBrainz];

    pub fn name(self) -> &'static str {
        match self {
            Service::LastFm => "Last.fm",
            Service::ListenBrainz => "ListenBrainz",
        }
    }

    fn session_path(self) -> PathBuf {
        crate::settings::data_dir().join(match self {
            Service::LastFm => "lastfm.bin",
            Service::ListenBrainz => "listenbrainz.bin",
        })
    }

    /// The connected account, encrypted like the YouTube sign-in.
    pub fn load_session(self) -> Option<Session> {
        serde_json::from_str(&crate::platform::secret::read(&self.session_path())?).ok()
    }

    pub fn save_session(self, session: &Session) {
        let saved = serde_json::to_string(session)
            .map_err(std::io::Error::other)
            .and_then(|json| crate::platform::secret::write(&self.session_path(), &json));
        if let Err(e) = saved {
            log::warn!("could not save the {} account: {e}", self.name());
        }
    }

    pub fn remove_session(self) {
        let _ = std::fs::remove_file(self.session_path());
    }

    pub fn pending(self) -> Pending {
        let file = match self {
            Service::LastFm => "scrobbles.json",
            Service::ListenBrainz => "listens.json",
        };
        Pending {
            file,
            ..crate::settings::read_json(&crate::settings::data_dir().join(file))
                .unwrap_or_default()
        }
    }

    pub async fn now_playing(self, session: &Session, play: &Play) -> Result<(), Error> {
        match self {
            Service::LastFm => lastfm::now_playing(session, play).await,
            Service::ListenBrainz => listenbrainz::now_playing(session, play).await,
        }
    }

    /// Records `plays`, a batch from `Pending::batch`.
    pub async fn scrobble(self, session: &Session, plays: &[Play]) -> Result<(), Error> {
        match self {
            Service::LastFm => lastfm::scrobble(session, plays).await,
            Service::ListenBrainz => listenbrainz::scrobble(session, plays).await,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Play {
    pub artist: String,
    pub track: String,
    pub album: Option<String>,
    /// Length in seconds.
    pub duration: Option<u32>,
    /// When the play started, in Unix seconds.
    pub timestamp: i64,
    /// A local file rather than a YouTube Music song.
    #[serde(default)]
    pub local: bool,
}

#[derive(Debug)]
pub enum Error {
    /// The account's access was revoked; it has to connect again.
    SignedOut,
    /// The user has not yet allowed access in the browser.
    NotYetAllowed,
    /// Refused as sent (bad parameters, an expired token); retrying won't help.
    Rejected(String),
    /// The service could not be reached or could not answer; try again later.
    Failed(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::SignedOut => f.write_str("access refused"),
            Error::NotYetAllowed => f.write_str("access not yet allowed"),
            Error::Rejected(e) | Error::Failed(e) => f.write_str(e),
        }
    }
}

pub fn failed(e: impl std::fmt::Display) -> Error {
    Error::Failed(e.to_string())
}

/// Whether a play heard for `heard` of a track `length` long counts: the
/// track is over 30 seconds, and half of it or four minutes has played.
pub fn counts(heard: Duration, length: Duration) -> bool {
    length > Duration::from_secs(30) && heard >= (length / 2).min(Duration::from_secs(240))
}

/// Plays a service has yet to accept, oldest first, kept across runs so
/// none are lost while it cannot be reached.
#[derive(Default, Serialize, Deserialize)]
pub struct Pending {
    plays: VecDeque<Play>,
    /// Who the plays were heard as, so no other account gets them.
    #[serde(default)]
    account: Option<String>,
    #[serde(skip)]
    file: &'static str,
    #[serde(skip)]
    changed: bool,
}

impl Pending {
    pub fn is_empty(&self) -> bool {
        self.plays.is_empty()
    }

    pub fn push(&mut self, play: Play) {
        if self.plays.len() == PENDING_LIMIT {
            self.plays.pop_front();
        }
        self.plays.push_back(play);
        self.changed = true;
    }

    /// Drops every play, as when the account is disconnected.
    pub fn clear(&mut self) {
        self.changed |= !self.plays.is_empty();
        self.plays.clear();
    }

    /// Holds the plays for `account`, dropping any heard under another.
    pub fn for_account(&mut self, account: &str) {
        if self.account.as_deref() == Some(account) {
            return;
        }
        if self.account.is_some() {
            self.clear();
        }
        self.account = Some(account.to_owned());
        self.changed = true;
    }

    /// The oldest plays, as many as go in one request, or just the oldest.
    pub fn batch(&self, one: bool) -> Vec<Play> {
        let size = if one { 1 } else { BATCH };
        self.plays.iter().take(size).cloned().collect()
    }

    /// Drops `sent`, taken or refused: the front, less any the limit dropped
    /// meanwhile.
    pub fn sent(&mut self, sent: &[Play]) {
        let n = self.plays.iter().take_while(|p| sent.contains(p)).count();
        self.plays.drain(..n);
        self.changed |= n > 0;
    }

    pub fn save(&mut self) {
        let path = crate::settings::data_dir().join(self.file);
        if self.changed && crate::settings::save_json(&path, self, "the scrobbles") {
            self.changed = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_half_a_track_or_four_minutes() {
        let secs = Duration::from_secs;
        assert!(!counts(secs(20), secs(30)), "30 seconds is too short");
        assert!(!counts(secs(89), secs(180)));
        assert!(counts(secs(90), secs(180)));
        assert!(counts(secs(240), secs(1200)), "four minutes of a long one");
        assert!(!counts(secs(239), secs(1200)));
    }

    #[test]
    fn keeps_pending_plays_in_order_within_the_limit() {
        let play = |n: i64| Play {
            artist: "A".into(),
            track: n.to_string(),
            album: None,
            duration: None,
            timestamp: n,
            local: false,
        };
        let mut pending = Pending::default();
        for n in 0..PENDING_LIMIT as i64 + 2 {
            pending.push(play(n));
        }
        assert_eq!(pending.plays.len(), PENDING_LIMIT);
        let batch = pending.batch(false);
        assert_eq!(batch.len(), BATCH);
        assert_eq!(batch[0].timestamp, 2, "the oldest were dropped");
        assert_eq!(pending.batch(true).len(), 1);

        // A new play pushes out the oldest while the batch is on its way.
        pending.push(play(PENDING_LIMIT as i64 + 2));
        pending.sent(&batch);
        assert_eq!(
            pending.batch(true)[0].timestamp,
            2 + BATCH as i64,
            "only the sent plays are gone"
        );
        // The pushed-out play was one of the batch, so one fewer goes.
        assert_eq!(pending.plays.len(), PENDING_LIMIT - BATCH + 1);

        pending.for_account("a");
        assert!(
            !pending.is_empty(),
            "untagged plays from before go to the account"
        );
        pending.for_account("a");
        assert!(!pending.is_empty(), "the same account keeps its plays");
        pending.for_account("b");
        assert!(pending.is_empty(), "another account gets none of them");
    }
}
