//! The data folder, and the writes that keep its files whole: each goes to
//! a temporary file renamed into place, and saves made off the UI thread are
//! numbered so a late one can't overwrite a newer.

use std::{
    io,
    path::{Path, PathBuf},
    sync::{
        Mutex, OnceLock, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
};

use serde::{Serialize, de::DeserializeOwned};

static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

fn app_data() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

/// `%APPDATA%\Lily`, where settings and caches live.
pub fn data_dir() -> PathBuf {
    DATA_DIR
        .get()
        .cloned()
        .unwrap_or_else(|| app_data().join("Lily"))
}

/// Moves the folder of the versions named ytm-native to `%APPDATA%\Lily`.
/// Run once from `main` while holding the single-instance lock, so no other
/// copy has files open; should the move fail, the old folder stays in use.
pub fn move_data_dir() {
    let (old, new) = (app_data().join("ytm-native"), app_data().join("Lily"));
    let dir = if !new.exists() && old.exists() {
        match std::fs::rename(&old, &new) {
            Ok(()) => new,
            Err(e) => {
                log::warn!("could not move {} to {}: {e}", old.display(), new.display());
                old
            }
        }
    } else {
        new
    };
    let _ = DATA_DIR.set(dir);
}

/// Renames a temporary file over `path`, so a failed write leaves the old file
/// whole. Not flushed, as most saves run on the UI thread. Each write has its
/// own temporary file, as rustypipe saves its cache from any worker.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_atomic_parts(path, [bytes])
}

/// `write_atomic` for bytes in parts, as a stream's blocks, which are then
/// never joined in memory.
pub fn write_atomic_parts<'a>(
    path: &Path,
    parts: impl IntoIterator<Item = &'a [u8]>,
) -> io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let partial = path.with_extension(format!("{}.tmp", NEXT.fetch_add(1, Ordering::Relaxed)));
    let write = || {
        let mut file = std::fs::File::create(&partial)?;
        for part in parts {
            io::Write::write_all(&mut file, part)?;
        }
        Ok(())
    };
    // The file is closed by then, as Windows renames no open file.
    let result = write().and_then(|()| std::fs::rename(&partial, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    result
}

pub(crate) fn read_json<T: DeserializeOwned>(path: &Path) -> Option<T> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Returns whether it was saved; `what` names it in the warning.
pub(crate) fn save_json(path: &Path, value: &impl Serialize, what: &str) -> bool {
    let written = serde_json::to_vec(value)
        .map_err(io::Error::other)
        .and_then(|json| write_atomic(path, &json));
    if let Err(e) = &written {
        log::warn!("could not save {what}: {e}");
    }
    written.is_ok()
}

/// Numbers one file's saves, which run off the UI thread and can finish
/// out of order, so an older one finishing late can't overwrite a newer.
pub struct Saves {
    issued: AtomicU64,
    written: Mutex<u64>,
}

impl Saves {
    const fn new() -> Self {
        Self {
            issued: AtomicU64::new(0),
            written: Mutex::new(0),
        }
    }

    /// Taken on the UI thread, in the order the saves are asked for.
    pub fn ticket(&self) -> u64 {
        self.issued.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Saves `value` unless a later ticket's save has been written, which
    /// counts as saved. Returns whether it was saved.
    pub(super) fn save(
        &self,
        ticket: u64,
        path: &Path,
        value: &impl Serialize,
        what: &str,
    ) -> bool {
        let mut written = self.written.lock().unwrap_or_else(PoisonError::into_inner);
        if ticket < *written {
            return true;
        }
        let saved = save_json(path, value, what);
        if saved {
            *written = ticket;
        }
        saved
    }
}

pub static SESSION_SAVES: Saves = Saves::new();
pub static HISTORY_SAVES: Saves = Saves::new();

/// Older versions kept a second copy of the session cookie for Premium,
/// which would go stale now that the cookie is renewed.
pub fn remove_old_cookie() {
    let dir = data_dir();
    let _ = std::fs::remove_file(dir.join("cookie.bin"));
    let _ = std::fs::remove_file(dir.join("cookie"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_late_older_save_does_not_overwrite_a_newer_one() {
        let saves = Saves::new();
        let path = std::env::temp_dir().join(format!("ytm-saves-{}.json", std::process::id()));
        let (older, newer) = (saves.ticket(), saves.ticket());
        assert!(saves.save(newer, &path, &"newer", "a test"));
        assert!(saves.save(older, &path, &"older", "a test"));
        assert_eq!(read_json::<String>(&path).as_deref(), Some("newer"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn replaces_files_whole() {
        let dir = std::env::temp_dir().join(format!("ytm-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.json");
        write_atomic(&path, b"[1]").unwrap();
        write_atomic(&path, b"[1,2]").unwrap();
        assert_eq!(read_json::<Vec<u8>>(&path), Some(vec![1, 2]));
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            1,
            "the partial files are renamed into place"
        );
        assert_eq!(read_json::<Vec<u8>>(&dir.join("missing.json")), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
