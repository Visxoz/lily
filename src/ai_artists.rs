//! Artists a community list names as making AI-generated music, kept for a
//! day in the data folder. Nothing is fetched or read until `load` is called.

use std::{path::PathBuf, time::Duration};

use crate::settings::{Listed, data_dir, time_left, write_atomic};

const URL: &str =
    "https://raw.githubusercontent.com/CennoxX/spotify-ai-blocker/main/SpotifyAiArtists.csv";

/// A copy this old is asked for again.
pub const FRESH_FOR: Duration = Duration::from_secs(24 * 60 * 60);

/// Ten times the list's size today.
const LIMIT: usize = 4 << 20;

/// Fewer is a cut-off or wrong answer, which never replaces a good copy.
const MIN_NAMES: usize = 100;

fn path() -> PathBuf {
    data_dir().join("ai-artists.csv")
}

/// The copy kept while under a day old, or however old while none is `held`,
/// else a fresh one. With when to fetch next: at once past a day old.
pub async fn load(held: bool) -> Result<(Listed, Duration), String> {
    // Off Tokio's workers, as a sleeping disk can take a while.
    let due = tokio::task::spawn_blocking(kept_due).await.ok().flatten();
    if (due.is_some() || !held)
        && let Some(listed) = read_kept().await
    {
        return Ok((listed, due.unwrap_or_default()));
    }
    let (listed, csv) = fetch().await?;
    tokio::task::spawn_blocking(move || {
        if let Err(e) = write_atomic(&path(), &csv) {
            log::warn!("could not save the list of AI artists: {e}");
        }
    });
    Ok((listed, FRESH_FOR))
}

/// When the copy kept is due, or none once it is.
fn kept_due() -> Option<Duration> {
    due_after(time_left(&path(), FRESH_FOR))
}

/// A copy within a minute of lapsing is due now, so the timer never wakes to
/// one still fresh.
fn due_after(left: Duration) -> Option<Duration> {
    (left > Duration::from_secs(60)).then_some(left)
}

async fn read_kept() -> Option<Listed> {
    let read = || parse(&std::fs::read_to_string(path()).ok()?);
    tokio::task::spawn_blocking(read).await.ok().flatten()
}

async fn fetch() -> Result<(Listed, Vec<u8>), String> {
    let response = (crate::http::ai_artists_client().get(URL).send().await)
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| format!("the list of AI artists: {}", e.without_url()))?;
    let csv = (crate::http::body(response, LIMIT).await)
        .map_err(|e| format!("the list of AI artists: {e}"))?;
    let listed = (std::str::from_utf8(&csv).ok())
        .and_then(parse)
        .ok_or("the list of AI artists did not read")?;
    Ok((listed, csv))
}

fn parse(csv: &str) -> Option<Listed> {
    let listed: Listed = names(csv).collect();
    (listed.len() >= MIN_NAMES).then_some(listed)
}

/// `artist,id` lines under a header, the ID Spotify's; split at the last
/// comma, as a name may hold one.
fn names(csv: &str) -> impl Iterator<Item = &str> {
    (csv.lines().skip(1))
        .filter_map(|line| Some(line.rsplit_once(',')?.0.trim()))
        .filter(|name| !name.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_names_but_never_a_short_list() {
        let csv = "artist,id\n Aria Vale\u{3000},1a\nSmith, Jones,2b\n,3c\nno id\r\nLast,4d\n";
        assert_eq!(
            names(csv).collect::<Vec<_>>(),
            ["Aria Vale", "Smith, Jones", "Last"]
        );
        assert!(parse(csv).is_none(), "too few to trust");
        let long: String = std::iter::once("artist,id\n".to_owned())
            .chain((0..MIN_NAMES).map(|n| format!("Artist {n},{n}\n")))
            .collect();
        assert_eq!(parse(&long).map(|l| l.len()), Some(MIN_NAMES));
    }

    #[test]
    fn a_kept_copy_is_due_within_a_minute_of_lapsing() {
        let hour = Duration::from_secs(60 * 60);
        assert_eq!(due_after(hour), Some(hour));
        assert_eq!(due_after(Duration::from_secs(30)), None);
        assert_eq!(due_after(Duration::ZERO), None, "lapsed or missing");
    }

    /// `cargo test --locked live_list_still_reads -- --ignored`
    #[tokio::test]
    #[ignore = "fetches the list from GitHub"]
    async fn live_list_still_reads() {
        let (listed, _) = fetch().await.unwrap();
        assert!(listed.len() > 1_000, "{} names", listed.len());
    }
}
