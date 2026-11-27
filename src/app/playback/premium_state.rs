//! What the Premium path keeps across songs: whether the account has
//! Premium, the backoff after failures, and the player script, rechecked as
//! YouTube moves on.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    time::{Duration, Instant},
};

use crate::streams::premium;

pub(in crate::app) const PREMIUM_UNKNOWN: u8 = 0;
pub(in crate::app) const PREMIUM_YES: u8 = 1;
pub(in crate::app) const PREMIUM_NO: u8 = 2;
/// Songs in a row without Premium formats before an account not yet known
/// to have Premium counts as without it, as some songs simply have no
/// Premium version.
const NOT_PREMIUM_AFTER: u8 = 3;
/// The same for an account known to have Premium: only a long run of
/// uploads reaches it, and a lapsed subscription soon does.
const LAPSED_AFTER: u8 = 20;
/// Cool-off after the player fails to prepare or Premium keeps failing,
/// between background checks of an account taken as without Premium, and
/// before a failing Premium stream has the player checked again.
const PREMIUM_BACKOFF: Duration = Duration::from_secs(10 * 60);
const PREMIUM_MAX_ERRORS: u8 = 3;
/// How often the held player is checked against YouTube's current one.
const PLAYER_RECHECK: Duration = Duration::from_secs(60 * 60);

/// Premium audio state shared with background tasks.
pub(in crate::app) struct PremiumShared {
    /// The cut-down player script, swapped when YouTube moves to a new one.
    player: tokio::sync::Mutex<Option<Arc<premium::Player>>>,
    /// When the held player was loaded or last checked.
    player_checked: Mutex<Instant>,
    /// A Premium stream failed since the last check, which an outdated
    /// player can cause.
    player_suspect: AtomicBool,
    status: AtomicU8,
    /// Songs in a row that offered no Premium formats.
    misses: AtomicU8,
    /// Premium requests in a row that failed outright.
    errors: AtomicU8,
    /// When preparing the player last failed, to avoid refetching it per song.
    load_failed: Mutex<Option<Instant>>,
    /// The Premium path is skipped until then, after repeated errors.
    paused_until: Mutex<Option<Instant>>,
    /// When an account taken as without Premium is next checked in the
    /// background, as a run of music videos can make a Premium one look so.
    recheck_at: Mutex<Option<Instant>>,
}

impl PremiumShared {
    /// `known`: served Premium formats before; `none`: last taken as without
    /// Premium, which is rechecked on the first song as `recheck_at` starts unset.
    pub(in crate::app) fn new(known: bool, none: bool) -> Self {
        let status = match (known, none) {
            (true, _) => PREMIUM_YES,
            (false, true) => PREMIUM_NO,
            (false, false) => PREMIUM_UNKNOWN,
        };
        Self {
            player: tokio::sync::Mutex::new(None),
            player_checked: Mutex::new(Instant::now()),
            player_suspect: AtomicBool::new(false),
            status: AtomicU8::new(status),
            misses: AtomicU8::new(0),
            errors: AtomicU8::new(0),
            load_failed: Mutex::new(None),
            paused_until: Mutex::new(None),
            recheck_at: Mutex::new(None),
        }
    }

    pub(in crate::app) fn status(&self) -> u8 {
        self.status.load(Ordering::Relaxed)
    }

    /// Signing in or out says nothing yet about Premium, and leaves none of
    /// the last session's play tracking.
    pub(in crate::app) fn reset(&self) {
        self.status.store(PREMIUM_UNKNOWN, Ordering::Relaxed);
        self.misses.store(0, Ordering::Relaxed);
        *self.recheck_at.lock().unwrap() = None;
        crate::api::forget_tracking();
    }

    /// Whether to go straight to the free stream.
    pub(super) fn skipped(&self) -> bool {
        self.status() == PREMIUM_NO
            || self
                .paused_until
                .lock()
                .unwrap()
                .is_some_and(|until| Instant::now() < until)
    }

    /// Whether an account taken as without Premium is due a background
    /// check, claiming it so only one runs.
    pub(super) fn take_recheck(&self) -> bool {
        if self.status() != PREMIUM_NO {
            return false;
        }
        let now = Instant::now();
        let mut at = self.recheck_at.lock().unwrap();
        if at.is_some_and(|at| now < at) {
            return false;
        }
        *at = Some(now + PREMIUM_BACKOFF);
        true
    }

    pub(super) fn record_premium(&self) {
        self.status.store(PREMIUM_YES, Ordering::Relaxed);
        self.misses.store(0, Ordering::Relaxed);
        self.errors.store(0, Ordering::Relaxed);
    }

    pub(super) fn record_miss(&self) {
        let misses = self
            .misses
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        let limit = if self.status() == PREMIUM_YES {
            LAPSED_AFTER
        } else {
            NOT_PREMIUM_AFTER
        };
        if misses >= limit {
            self.status.store(PREMIUM_NO, Ordering::Relaxed);
            *self.recheck_at.lock().unwrap() = Some(Instant::now() + PREMIUM_BACKOFF);
        }
    }

    /// Whether the held player is due a check against YouTube's current
    /// one, hourly or sooner after a Premium failure, claiming it so only
    /// one runs.
    fn take_player_check(&self, now: Instant) -> bool {
        let mut checked = self.player_checked.lock().unwrap();
        let age = now.saturating_duration_since(*checked);
        if age < PLAYER_RECHECK
            && !(age >= PREMIUM_BACKOFF && self.player_suspect.load(Ordering::Relaxed))
        {
            return false;
        }
        *checked = now;
        self.player_suspect.store(false, Ordering::Relaxed);
        true
    }

    /// A Premium stream failed, so check the player sooner.
    pub(in crate::app) fn suspect_player(&self) {
        self.player_suspect.store(true, Ordering::Relaxed);
    }

    pub(super) fn record_error(&self) {
        self.suspect_player();
        if self.errors.fetch_add(1, Ordering::Relaxed) + 1 >= PREMIUM_MAX_ERRORS {
            self.errors.store(0, Ordering::Relaxed);
            *self.paused_until.lock().unwrap() = Some(Instant::now() + PREMIUM_BACKOFF);
            premium::diag("Premium requests keep failing; using free streams for 10 minutes");
        }
    }
}

/// `YTM_PREMIUM_DRYRUN=1` times the Premium path while playing free streams.
pub(in crate::app) fn premium_dry_run() -> bool {
    std::env::var_os("YTM_PREMIUM_DRYRUN").is_some()
}

/// The held player, checked behind the song when due, as songs a running
/// session solves with an old one start failing once YouTube moves on.
pub(super) async fn premium_player(
    shared: &Arc<PremiumShared>,
) -> Result<Arc<premium::Player>, String> {
    let mut slot = shared.player.lock().await;
    if let Some(player) = slot.as_ref() {
        if shared.take_player_check(Instant::now()) {
            check_behind(shared, player, false);
        }
        return Ok(Arc::clone(player));
    }
    // At launch the saved player plays while YouTube's version is checked;
    // a stale one costs the songs it solves meanwhile Premium, as they retry free.
    if let Some(saved) = premium::saved_player() {
        let saved = Arc::new(saved);
        *shared.player_checked.lock().unwrap() = Instant::now();
        check_behind(shared, &saved, true);
        *slot = Some(Arc::clone(&saved));
        return Ok(saved);
    }
    if shared
        .load_failed
        .lock()
        .unwrap()
        .is_some_and(|at| at.elapsed() < PREMIUM_BACKOFF)
    {
        return Err("waiting before preparing the player again".into());
    }
    match premium::load_current().await {
        Ok(player) => {
            let player = Arc::new(player);
            *slot = Some(Arc::clone(&player));
            *shared.player_checked.lock().unwrap() = Instant::now();
            Ok(player)
        }
        Err(e) => {
            *shared.load_failed.lock().unwrap() = Some(Instant::now());
            Err(e)
        }
    }
}

/// Checks `held` against YouTube's current player behind the song. With
/// `saved`, never checked yet, a failed check is due again after the
/// back-off rather than in an hour.
fn check_behind(shared: &Arc<PremiumShared>, held: &Arc<premium::Player>, saved: bool) {
    let (shared, held) = (Arc::clone(shared), Arc::clone(held));
    tokio::spawn(async move {
        if let Err(e) = recheck_player(&shared, &held).await {
            if saved {
                shared.suspect_player();
            }
            premium::diag(format!("could not check the player: {e}"));
        }
    });
}

/// Swaps in YouTube's current player should it have moved on from `held`.
/// Songs already resolving keep the one they took.
async fn recheck_player(shared: &PremiumShared, held: &premium::Player) -> Result<(), String> {
    let id = premium::current_version().await?;
    if id != held.id {
        let player = premium::load_player(id).await?;
        premium::diag(format!("player {} replaces {}", player.id, held.id));
        *shared.player.lock().await = Some(Arc::new(player));
    } else {
        log::debug!("the Premium player {id} is still current");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_long_run_of_misses_turns_off_a_known_premium_account() {
        let shared = PremiumShared::new(true, false);
        for _ in 1..LAPSED_AFTER {
            shared.record_miss();
        }
        assert_eq!(shared.status(), PREMIUM_YES, "a run of uploads");
        shared.record_premium();
        for _ in 1..LAPSED_AFTER {
            shared.record_miss();
        }
        assert_eq!(
            shared.status(),
            PREMIUM_YES,
            "a Premium song starts the count again"
        );

        shared.record_miss();
        assert_eq!(shared.status(), PREMIUM_NO, "a lapsed subscription");
        assert!(shared.skipped());
        assert!(!shared.take_recheck(), "not due until the cool-off ends");
    }

    #[test]
    fn checks_again_in_the_background_after_taking_an_account_as_without_premium() {
        let shared = PremiumShared::new(false, false);
        shared.record_miss();
        shared.record_miss();
        assert!(!shared.skipped(), "two songs say nothing yet");
        assert!(!shared.take_recheck(), "nothing to check while unknown");
        shared.record_miss();
        assert_eq!(shared.status(), PREMIUM_NO);
        assert!(shared.skipped(), "songs go straight to the free stream");
        assert!(!shared.take_recheck(), "not due yet");

        *shared.recheck_at.lock().unwrap() = Some(Instant::now());
        assert!(shared.take_recheck(), "due once the cool-off ends");
        assert!(!shared.take_recheck(), "only one check at a time");
        assert!(shared.skipped(), "songs still play free meanwhile");

        shared.record_premium();
        assert_eq!(shared.status(), PREMIUM_YES);
        assert!(!shared.skipped());
        shared.record_miss();
        assert_eq!(shared.status(), PREMIUM_YES);
    }

    #[test]
    fn checks_the_player_hourly_or_sooner_after_a_failure() {
        // Times run forward from the load, as an `Instant` before boot panics.
        let shared = PremiumShared::new(true, false);
        let loaded = *shared.player_checked.lock().unwrap();
        assert!(!shared.take_player_check(loaded), "just loaded");
        shared.suspect_player();
        assert!(
            !shared.take_player_check(loaded),
            "a failure waits out the floor"
        );

        let first = loaded + PREMIUM_BACKOFF;
        assert!(
            shared.take_player_check(first),
            "a failure brings it forward"
        );
        assert!(!shared.take_player_check(first), "only one check at a time");

        assert!(
            !shared.take_player_check(first + PREMIUM_BACKOFF),
            "the check cleared the failure"
        );
        assert!(
            shared.take_player_check(first + PLAYER_RECHECK),
            "due hourly regardless"
        );
    }

    #[test]
    fn a_new_sign_in_starts_unknown() {
        let shared = PremiumShared::new(false, false);
        for _ in 0..3 {
            shared.record_miss();
        }
        shared.reset();
        assert_eq!(shared.status(), PREMIUM_UNKNOWN);
        assert!(!shared.skipped());
    }

    #[test]
    fn a_remembered_no_plays_free_and_is_checked_at_once() {
        let shared = PremiumShared::new(false, true);
        assert!(shared.skipped(), "no Premium request before the first song");
        assert!(
            shared.take_recheck(),
            "checked in the background straight away"
        );
        assert!(!shared.take_recheck());
    }
}
