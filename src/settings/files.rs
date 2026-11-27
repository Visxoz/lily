//! The data folder, and the writes that keep its files whole: each goes to
//! a temporary file renamed into place, and saves made off the UI thread are
//! numbered so a late one can't overwrite a newer.

use std::{
    fs::File,
    io,
    path::{Path, PathBuf},
    sync::{
        Mutex, OnceLock, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime},
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
    write_atomic_parts(path, |file| io::Write::write_all(file, bytes))
}

/// `write_atomic` for bytes `write` puts in the file in parts, as a stream's
/// blocks, which are then never joined in memory.
pub fn write_atomic_parts(
    path: &Path,
    write: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let partial = path.with_extension(format!("{}.tmp", NEXT.fetch_add(1, Ordering::Relaxed)));
    // The file is closed by then, as Windows renames no open file.
    let result = File::create(&partial)
        .and_then(|mut file| write(&mut file))
        .and_then(|()| std::fs::rename(&partial, path));
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

/// Deletes through `remove` the files in `dir` that `counts` takes, first any
/// last changed over `max_age` ago, then the oldest until the rest fit
/// `budget`; one `remove` keeps, returning false, still counts toward it.
pub fn prune(
    dir: &Path,
    budget: u64,
    max_age: Option<Duration>,
    remove: impl Fn(&Path) -> bool,
    counts: impl Fn(&Path) -> bool,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(SystemTime, u64, PathBuf)> = entries
        .filter_map(|e| {
            let e = e.ok()?;
            let path = e.path();
            if !counts(&path) {
                return None;
            }
            let meta = e.metadata().ok()?;
            Some((meta.modified().ok()?, meta.len(), path))
        })
        .collect();
    files.sort_unstable_by_key(|(modified, ..)| *modified);
    let mut total: u64 = files.iter().map(|(_, size, _)| size).sum();
    let now = SystemTime::now();
    for (modified, size, path) in files {
        let old =
            max_age.is_some_and(|max| now.duration_since(modified).is_ok_and(|age| age > max));
        if !old && total <= budget {
            // Sorted oldest first, so none after is old either.
            break;
        }
        if remove(&path) {
            total -= size;
        }
    }
}

/// Marks a kept file as just used, for the prune's order; false when there
/// is none.
pub fn touch(path: &Path) -> bool {
    use std::os::windows::fs::OpenOptionsExt;
    // Attributes only, which a decoder's open of the same file shares.
    std::fs::File::options()
        .access_mode(windows_sys::Win32::Storage::FileSystem::FILE_WRITE_ATTRIBUTES)
        .open(path)
        .and_then(|file| file.set_modified(SystemTime::now()))
        .is_ok()
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
