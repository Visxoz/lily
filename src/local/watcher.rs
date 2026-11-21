//! Watches the music folders for files added, removed, renamed or written
//! to, on a thread of its own, so Local files is scanned again as they change.

use std::{
    io,
    os::windows::ffi::OsStrExt,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT},
    Storage::FileSystem::{
        FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE,
        FindCloseChangeNotification, FindFirstChangeNotificationW, FindNextChangeNotification,
        GetDriveTypeW,
    },
    System::Threading::{CreateEventW, SetEvent, WaitForMultipleObjects, WaitForSingleObject},
};

use super::roots;

/// `DRIVE_REMOVABLE`, a USB stick or a card, which a folder being watched
/// would keep from being safely removed.
const REMOVABLE: u32 = 2;
/// A burst of changes, as an album copied in, is waited out this long...
const SETTLE: Duration = Duration::from_millis(500);
/// ...but no longer than this, so a folder written to all the time still shows.
const SETTLE_AT_MOST: Duration = Duration::from_secs(10);
/// How often a folder that couldn't be watched is tried again.
const RETRY_EVERY: Duration = Duration::from_secs(60);
/// `MAXIMUM_WAIT_OBJECTS`, less the stop event's place.
const MAX_WATCHED: usize = 63;

/// An event the watcher thread waits on, closed once neither side holds it.
struct Event(HANDLE);

// SAFETY: an event handle may be set and waited on from any thread.
unsafe impl Send for Event {}
// SAFETY: as above; nothing changes the handle itself.
unsafe impl Sync for Event {}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateEventW and is closed only here.
        unsafe { CloseHandle(self.0) };
    }
}

/// Watches the folders for files added, removed, renamed or written to, until dropped.
pub struct Watcher(Arc<Event>);

impl Drop for Watcher {
    /// Not joined: the thread may be waiting on a sleeping share, and ends
    /// by itself once that answers.
    fn drop(&mut self) {
        // SAFETY: the event stays open while this holds it.
        unsafe { SetEvent(self.0.0) };
    }
}

/// Runs `changed` on a thread of its own once files in `folders` change, a
/// burst of changes at a time. The folders are opened there, as a share
/// that's asleep can take a while to answer.
pub fn watch(folders: &[PathBuf], changed: impl Fn() + Send + 'static) -> Option<Watcher> {
    let roots = roots(folders);
    if roots.is_empty() {
        return None;
    }
    // SAFETY: a manual-reset event, not yet set, with no name or attributes.
    let stop = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    if stop.is_null() {
        return None;
    }
    let stop = Arc::new(Event(stop));
    let waiting = Arc::clone(&stop);
    let spawned = std::thread::Builder::new()
        .name("folder-watch".into())
        .spawn(move || wait_for_changes(waiting.0, &roots, changed));
    if let Err(e) = spawned {
        log::warn!("could not watch the music folders: {e}");
        return None;
    }
    Some(Watcher(stop))
}

fn open_watch(root: &Path) -> Option<HANDLE> {
    let path: Vec<u16> = root.as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: `path` is NUL-terminated.
    let handle = unsafe {
        FindFirstChangeNotificationW(
            path.as_ptr(),
            1,
            FILE_NOTIFY_CHANGE_FILE_NAME
                | FILE_NOTIFY_CHANGE_DIR_NAME
                | FILE_NOTIFY_CHANGE_LAST_WRITE,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        log::debug!(
            "could not watch a music folder: {}",
            io::Error::last_os_error()
        );
        return None;
    }
    Some(handle)
}

/// The watcher's caller keeps `stop` open while it runs.
fn stopped(stop: HANDLE) -> bool {
    // SAFETY: as above.
    unsafe { WaitForSingleObject(stop, 0) == WAIT_OBJECT_0 }
}

fn close_watches(watches: &[HANDLE]) {
    for &handle in watches {
        // SAFETY: each came from FindFirstChangeNotificationW and is closed once.
        unsafe { FindCloseChangeNotification(handle) };
    }
}

fn wait_for_changes(stop: HANDLE, roots: &[PathBuf], changed: impl Fn()) {
    // shortcut: folders on removable drives aren't watched, so a USB stick can be
    // safely removed; a USB hard disk counts as fixed. And as Windows won't rename
    // a folder with an open handle inside it, a watched folder's parents can't be
    // renamed or moved while Lily runs. Use device notifications or polling should
    // either matter.
    let mut roots: Vec<&Path> = (roots.iter().map(PathBuf::as_path))
        .filter(|root| !removable(root))
        .collect();
    if roots.len() > MAX_WATCHED {
        log::warn!("only the first {MAX_WATCHED} music folders are watched");
        roots.truncate(MAX_WATCHED);
    }
    // The stop event, then a watch for each of `watched`, in order.
    let mut handles = vec![stop];
    let mut watched = Vec::new();
    let mut unwatched = Vec::new();
    for root in roots {
        if stopped(stop) {
            break;
        }
        match open_watch(root) {
            Some(handle) => {
                handles.push(handle);
                watched.push(root);
            }
            None => unwatched.push(root),
        }
    }
    // When the first change not yet reported came.
    let mut since: Option<Instant> = None;
    loop {
        let timeout = match since {
            Some(at) => {
                let left = SETTLE_AT_MOST.saturating_sub(at.elapsed());
                if left.is_zero() {
                    since = None;
                    changed();
                    continue;
                }
                left.min(SETTLE).as_millis() as u32
            }
            None if watched.is_empty() && unwatched.is_empty() => break,
            None => RETRY_EVERY.as_millis() as u32,
        };
        // SAFETY: every handle is open, and there are at most `MAX_WATCHED` + 1.
        let result =
            unsafe { WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, timeout) };
        if result == WAIT_TIMEOUT {
            if since.take().is_some() {
                changed();
                continue;
            }
            // A folder renamed or sent to the Recycle Bin is watched where it
            // went, so it is let go of, to be watched again should it come back.
            let mut moved = false;
            for i in (0..watched.len()).rev() {
                if matches!(watched[i].try_exists(), Ok(false)) {
                    close_watches(&[handles.swap_remove(i + 1)]);
                    unwatched.push(watched.swap_remove(i));
                    moved = true;
                }
            }
            // A share back from sleep, or a drive plugged in again; whatever
            // changed while it was away is reported.
            let before = unwatched.len();
            unwatched.retain(|root| {
                if stopped(stop) {
                    return true;
                }
                match open_watch(root) {
                    Some(handle) => {
                        handles.push(handle);
                        watched.push(root);
                        false
                    }
                    None => true,
                }
            });
            if moved || unwatched.len() < before {
                changed();
            }
            continue;
        }
        let index = result.wrapping_sub(WAIT_OBJECT_0) as usize;
        if index == 0 {
            break;
        }
        if index >= handles.len() {
            log::warn!(
                "stopped watching the music folders: {}",
                io::Error::last_os_error()
            );
            break;
        }
        since.get_or_insert_with(Instant::now);
        // SAFETY: the handle came from FindFirstChangeNotificationW.
        if unsafe { FindNextChangeNotification(handles[index]) } == 0 {
            // Its folder went, or its drive, or a share's connection: it's
            // reported, and watched again once it can be.
            close_watches(&[handles.swap_remove(index)]);
            unwatched.push(watched.swap_remove(index - 1));
        }
    }
    close_watches(&handles[1..]);
}

fn removable(path: &Path) -> bool {
    let mut components = path.components();
    let (Some(Component::Prefix(prefix)), Some(Component::RootDir)) =
        (components.next(), components.next())
    else {
        return false;
    };
    let root: Vec<u16> = prefix
        .as_os_str()
        .encode_wide()
        .chain([u16::from(b'\\'), 0])
        .collect();
    // SAFETY: `root` is NUL-terminated.
    unsafe { GetDriveTypeW(root.as_ptr()) == REMOVABLE }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watches_a_folder_for_files_going() {
        let dir = std::env::temp_dir().join(format!("lily-watch-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("album")).unwrap();
        let file = dir.join("album").join("song.wav");
        std::fs::write(&file, b"").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let watcher = watch(std::slice::from_ref(&dir), move || {
            let _ = tx.send(());
        })
        .unwrap();
        // The folder is opened on the watcher's thread, so a change made
        // before it is watched goes unseen; one is made until it shows.
        let reported = (0..30).any(|_| {
            std::fs::write(&file, b"").unwrap();
            std::fs::remove_file(&file).unwrap();
            rx.recv_timeout(Duration::from_millis(500)).is_ok()
        });
        drop(watcher);
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(reported, "a file deleted below the folder is reported");
    }
}
