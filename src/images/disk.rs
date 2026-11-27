//! Downloaded artwork kept on disk as it was sent, read before the network,
//! so the next launch shows it without downloading.

use std::{
    path::{Path, PathBuf},
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use sha1::{Digest, Sha1};

/// Downloaded artwork kept between runs, as it was sent.
const DISK_BUDGET: u64 = 64 * 1024 * 1024;
/// Artwork can change under the same URL, so prunes drop any older than this.
const DISK_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// Pruned again after this much is written, as the app can run for days.
const PRUNE_EVERY: u64 = DISK_BUDGET / 8;

pub(super) fn disk_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| crate::settings::data_dir().join("artwork"))
}

pub(super) fn disk_name(url: &str) -> String {
    format!("{:x}", Sha1::digest(url.as_bytes()))
}

pub(super) fn save(dir: &Path, url: &str, bytes: &[u8]) {
    static WRITTEN: AtomicU64 = AtomicU64::new(0);
    let saved = std::fs::create_dir_all(dir)
        .and_then(|()| crate::settings::write_atomic(&dir.join(disk_name(url)), bytes));
    if let Err(e) = saved {
        log::debug!("could not keep artwork: {e}");
    } else if WRITTEN.fetch_add(bytes.len() as u64, Ordering::Relaxed) >= PRUNE_EVERY {
        WRITTEN.store(0, Ordering::Relaxed);
        prune(dir, DISK_BUDGET, DISK_MAX_AGE);
    }
}

/// Run once at startup, off the UI thread.
pub fn prune_disk() {
    prune(disk_dir(), DISK_BUDGET, DISK_MAX_AGE);
}

fn prune(dir: &Path, budget: u64, max_age: Duration) {
    let remove = |path: &Path| std::fs::remove_file(path).is_ok();
    crate::settings::prune(dir, budget, Some(max_age), remove, |_| true);
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::*;

    #[test]
    fn keeps_artwork_on_disk_within_its_bounds() {
        let dir = std::env::temp_dir().join(format!("lily-artwork-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = |url: &str| dir.join(disk_name(url));
        for url in ["a", "b", "c"] {
            save(&dir, url, &[0; 10]);
        }
        assert_eq!(std::fs::read(path("a")).unwrap(), [0; 10]);
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            3,
            "no temporary files left"
        );

        let month_ago = SystemTime::now() - DISK_MAX_AGE - Duration::from_secs(60);
        let set = |url, when| {
            let file = std::fs::File::options()
                .write(true)
                .open(path(url))
                .unwrap();
            file.set_modified(when).unwrap();
        };
        set("a", month_ago);
        set("b", SystemTime::now() - Duration::from_secs(60));
        prune(&dir, u64::MAX, DISK_MAX_AGE);
        assert!(!path("a").exists(), "too old");
        assert!(path("b").exists());
        prune(&dir, 15, DISK_MAX_AGE);
        assert!(!path("b").exists(), "the older of two past the budget");
        assert!(path("c").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
