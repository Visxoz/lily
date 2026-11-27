//! Downloads a stream into memory in the background and serves it to the
//! demuxer as a seekable file, blocking reads until their bytes arrive. The
//! download follows the reader, so a seek ahead is fetched next and the gaps
//! after. A file on disk is served in place, and a stream to be kept is
//! written out as it arrives.

mod blocks;
mod download;

use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    os::windows::fs::FileExt,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, OnceLock, PoisonError},
    thread,
};

use blocks::State;
use download::Part;
use symphonia::core::io::MediaSource;

use super::Keep;

/// googlevideo throttles large single requests, so the file is fetched in
/// ranges no bigger than this, each within one aligned block.
const CHUNK: u64 = 1024 * 1024;
/// Streams longer than this, which only long mixes and uploads reach, keep
/// only the blocks around the read position; songs are held whole, for replays.
const WINDOW_FROM: u64 = 64 * CHUNK;
/// Blocks a window keeps behind the read position, and fetches ahead of it:
/// minutes of sound either way, far longer than `RIDE_OUT` waits on a drop.
const KEEP_BEHIND: usize = 4;
const KEEP_AHEAD: usize = 8;

pub struct Source {
    pub url: String,
    /// Length YouTube reports up front, or 0 if it did not.
    pub size: u64,
    pub headers: Vec<(String, String)>,
    /// Its first bytes, fetched ahead from the same URL, or none.
    pub head: Vec<u8>,
}

/// Asks YouTube again for the stream, as googlevideo sometimes serves a URL's
/// first megabyte and refuses the rest. Told whether the last fresh URL was
/// refused too. Runs on the download thread, so may block.
pub type Refresh = Box<dyn FnMut(bool) -> Result<Source, String> + Send>;

/// Each file a live download plays from or is being kept as, once per
/// download, as repeat-one opens one file for both the current and next.
static IN_USE: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Holds a path in `IN_USE` until dropped.
struct InUse(PathBuf);

impl InUse {
    fn new(path: &Path) -> Self {
        lock_in_use().push(path.to_owned());
        Self(path.to_owned())
    }
}

impl Drop for InUse {
    fn drop(&mut self) {
        let mut held = lock_in_use();
        if let Some(i) = held.iter().position(|p| *p == self.0) {
            held.swap_remove(i);
        }
    }
}

fn lock_in_use() -> std::sync::MutexGuard<'static, Vec<PathBuf>> {
    IN_USE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether a download is playing `path` or writing it out, so it must stay.
pub fn in_use(path: &Path) -> bool {
    lock_in_use().iter().any(|p| p == path)
}

#[derive(Default)]
pub struct Download {
    state: Mutex<State>,
    changed: Condvar,
    /// A file read in place, set before `done`.
    file: OnceLock<File>,
    /// The URL it started from, which a replay is matched on; empty for a file.
    url: String,
    /// The file it reads or is kept as, held in use while it lives.
    _in_use: Option<InUse>,
}

impl Download {
    fn sized(size: u64) -> Self {
        let download = Self::default();
        download.state.lock().unwrap().total = size;
        download
    }

    /// With `keep`, the stream is written out as it arrives and kept once
    /// whole, unless it is a window, which never is.
    pub fn start(mut source: Source, refresh: Refresh, keep: Option<Keep>) -> Arc<Self> {
        // Only here, as `fetch_all` must keep every byte.
        let window = source.size > WINDOW_FROM;
        let keep = keep.filter(|_| !window);
        let mut download = Self {
            url: source.url.clone(),
            _in_use: keep.as_ref().map(|keep| InUse::new(&keep.path)),
            ..Self::sized(source.size)
        };
        let state = download.state.get_mut().unwrap();
        state.window = window;
        // `run` carries on after the bytes held. Taken, as the source is kept
        // for refreshes all song long.
        let head = std::mem::take(&mut source.head);
        state.put(0, &head);
        let part = keep.map(|keep| Part::new(keep, head));
        let download = Arc::new(download);
        let worker = Arc::clone(&download);
        thread::Builder::new()
            .name("stream-download".into())
            .spawn(move || worker.run(source, refresh, part))
            .expect("download thread spawns");
        download
    }

    /// Serves `path`; a file that won't open fails the first read. Opened on
    /// a thread, as a sleeping disk or share can take seconds to answer.
    pub fn open(path: PathBuf) -> Arc<Self> {
        let download = Arc::new(Self {
            _in_use: Some(InUse::new(&path)),
            ..Self::default()
        });
        let worker = Arc::clone(&download);
        thread::Builder::new()
            .name("file-open".into())
            .spawn(move || {
                let opened = File::open(&path).and_then(|f| Ok((f.metadata()?.len(), f)));
                let mut state = worker.state.lock().unwrap();
                match opened {
                    Ok((len, file)) => {
                        let _ = worker.file.set(file);
                        state.total = len;
                        state.done = true;
                    }
                    Err(e) => state.error = Some(format!("couldn't open the file: {e}")),
                }
                drop(state);
                worker.changed.notify_all();
            })
            .expect("file thread spawns");
        download
    }

    /// The whole stream, fetched on this thread as the player would, for keeping.
    /// Its blocks, which are written out in turn rather than joined, as
    /// joining would hold the stream twice.
    pub fn fetch_all(source: Source, refresh: Refresh) -> Result<Vec<Vec<u8>>, String> {
        let download = Self::sized(source.size);
        download.run(source, refresh, None);
        let state = download.state.into_inner().unwrap();
        if let Some(e) = state.error {
            return Err(e);
        }
        Ok(state.blocks)
    }

    /// Stops the download and fails any blocked read.
    pub fn cancel(&self) {
        let mut state = self.state.lock().unwrap();
        state.cancelled = true;
        // Nothing reads a cancelled download, though a slot or a thread
        // waiting on the network may hold it a while yet.
        state.blocks = Vec::new();
        drop(state);
        self.changed.notify_all();
    }

    /// Where the bytes held from the read position run to, as a fraction of
    /// the stream.
    pub fn progress(&self) -> f32 {
        let state = self.state.lock().unwrap();
        if state.done {
            1.0
        } else if state.total == 0 {
            0.0
        } else {
            state.run_end(state.pos) as f32 / state.total as f32
        }
    }

    /// Whether all of `url` is held in memory, so it can play again from
    /// there. A cancelled download is emptied, even one that was done.
    pub fn whole(&self, url: &str) -> bool {
        let state = self.state.lock().unwrap();
        !self.url.is_empty() && self.url == url && state.done && !state.cancelled
    }

    /// Whether it gave up on a connection that stopped answering, rather
    /// than on a refusal or a bad file.
    pub fn dropped(&self) -> bool {
        self.state.lock().unwrap().dropped
    }

    pub fn reader(self: &Arc<Self>) -> StreamReader {
        self.state.lock().unwrap().readers += 1;
        StreamReader {
            download: Arc::clone(self),
            pos: 0,
        }
    }
}

pub struct StreamReader {
    download: Arc<Download>,
    pos: u64,
}

impl Read for StreamReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut state = self.download.state.lock().unwrap();
        // The download follows the reader, and a window waits on it moving on.
        if state.pos / CHUNK != self.pos / CHUNK {
            self.download.changed.notify_all();
        }
        state.pos = self.pos;
        loop {
            if state.cancelled {
                // Not `Interrupted`, which Symphonia's exact reads retry
                // for ever, hanging the engine on a skip.
                return Err(io::Error::other("download cancelled"));
            }
            if let Some(file) = self.download.file.get() {
                drop(state);
                // Positional, as seeking moves only `pos`.
                let n = file.seek_read(buf, self.pos)?;
                self.pos += n as u64;
                return Ok(n);
            }
            let block = state.blocks.get((self.pos / CHUNK) as usize);
            let held = block.and_then(|b| b.get((self.pos % CHUNK) as usize..));
            if let Some(held) = held.filter(|h| !h.is_empty()) {
                let n = buf.len().min(held.len());
                buf[..n].copy_from_slice(&held[..n]);
                self.pos += n as u64;
                return Ok(n);
            }
            if state.total > 0 && self.pos >= state.total {
                return Ok(0);
            }
            if let Some(e) = &state.error {
                return Err(io::Error::other(e.clone()));
            }
            state = self.download.changed.wait(state).unwrap();
        }
    }
}

impl Drop for StreamReader {
    fn drop(&mut self) {
        // A download whose track is dropped is not always cancelled, so a
        // window waiting on its reader would otherwise wait for ever.
        if let Ok(mut state) = self.download.state.lock() {
            state.readers -= 1;
            state.abandoned = state.readers == 0;
        }
        self.download.changed.notify_all();
    }
}

impl Seek for StreamReader {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let total = self.download.state.lock().unwrap().total;
        let target = match from {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::Current(n) => self.pos.checked_add_signed(n),
            SeekFrom::End(n) => total.checked_add_signed(n),
        };
        self.pos = target
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek out of range"))?;
        Ok(self.pos)
    }
}

impl MediaSource for StreamReader {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        let total = self.download.state.lock().unwrap().total;
        (total > 0).then_some(total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) const URL: &str = "https://rr1.googlevideo.com/videoplayback?expire=1&itag=251&clen=4171552&lmt=1700000000000000&n=a";

    #[test]
    fn a_missing_file_fails_to_read() {
        let path = std::env::temp_dir()
            .join("lily-no-such-folder")
            .join("a.flac");
        let mut reader = Download::open(path).reader();
        let e = reader.read(&mut [0; 4]).unwrap_err();
        assert!(e.to_string().contains("couldn't open"), "{e}");
    }

    #[test]
    fn holds_a_file_in_use_while_any_download_has_it() {
        let path = std::env::temp_dir().join("lily-in-use.audio");
        let (current, next) = (InUse::new(&path), InUse::new(&path));
        drop(current);
        assert!(in_use(&path), "the next track still plays it");
        drop(next);
        assert!(!in_use(&path));
    }

    #[test]
    fn a_cancelled_read_fails_for_good() {
        let download = Arc::new(Download::default());
        download.state.lock().unwrap().put(0, &[1; 8]);
        download.cancel();
        let kind = download.reader().read(&mut [0; 4]).unwrap_err().kind();
        assert_ne!(kind, io::ErrorKind::Interrupted, "Symphonia would retry");
        assert_ne!(kind, io::ErrorKind::UnexpectedEof, "not the stream's end");
        assert_eq!(download.state.lock().unwrap().blocks.capacity(), 0);
    }

    #[test]
    fn plays_again_only_a_whole_download_of_the_same_url() {
        let download = Arc::new(Download {
            url: URL.into(),
            ..Download::default()
        });
        download.state.lock().unwrap().put(0, &[1, 2, 3]);
        assert!(!download.whole(URL), "still downloading");
        download.state.lock().unwrap().done = true;
        assert!(download.whole(URL));
        assert!(
            !download.whole(&URL.replace("n=a", "n=b")),
            "another stream"
        );
        download.cancel();
        assert!(!download.whole(URL), "cancelled, so emptied");
    }

    pub(super) const MB: u64 = CHUNK;

    pub(super) fn fill(state: &mut State, block: u64) {
        let len = (state.total - block * MB).min(MB) as usize;
        let at = block * MB + state.held(block as usize) as u64;
        let rest = len - state.held(block as usize);
        state.put(at, &vec![block as u8; rest]);
    }

    #[test]
    fn reads_a_block_held_past_a_gap() {
        let download = Arc::new(Download::sized(4 * MB));
        fill(&mut download.state.lock().unwrap(), 2);
        let next = || download.state.lock().unwrap().next_range();
        let mut reader = download.reader();
        reader.seek(SeekFrom::Start(2 * MB + 1)).unwrap();
        assert_eq!(next(), Some(0), "a seek alone moves nothing");
        let mut buf = [0; 4];
        assert_eq!(reader.read(&mut buf).unwrap(), 4);
        assert_eq!(buf, [2; 4]);
        assert_eq!(next(), Some(3 * MB), "on from where it read");
        reader.seek(SeekFrom::Start(4 * MB)).unwrap();
        assert_eq!(reader.read(&mut buf).unwrap(), 0, "the end, gaps or not");
    }

    #[test]
    fn seeks_within_range_only() {
        let mut reader = Arc::new(Download::default()).reader();
        assert_eq!(reader.seek(SeekFrom::Current(1)).unwrap(), 1);
        assert!(reader.seek(SeekFrom::Current(-2)).is_err(), "before start");
        assert!(reader.seek(SeekFrom::End(-1)).is_err(), "no length yet");
        assert_eq!(reader.seek(SeekFrom::Current(i64::MAX)).unwrap(), 1 << 63);
        assert!(reader.seek(SeekFrom::Current(i64::MAX)).is_ok());
        assert!(
            reader.seek(SeekFrom::Current(i64::MAX)).is_err(),
            "overflow"
        );
    }
}
