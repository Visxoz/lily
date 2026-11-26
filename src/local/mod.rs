//! The user's own music: the audio files in the folders they add, with the
//! title, artists, album and cover art their tags give.

use std::{
    collections::{HashMap, HashSet},
    os::windows::fs::MetadataExt,
    path::{Component, Path, PathBuf},
    sync::{PoisonError, RwLock},
    time::{Duration, SystemTime},
};

use serde::{Deserialize, Serialize};

use crate::api::Track;

mod tags;
mod watcher;

pub use tags::artwork;
use tags::{Tags, is_audio, read_song, song};
pub use watcher::{Watcher, watch};

/// `FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM`, both set on folders such
/// as `$RECYCLE.BIN` that hold no music of the user's. System alone is often
/// set on an album's folder to give it a picture.
const HIDDEN_AND_SYSTEM: u32 = 0x2 | 0x4;
/// `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`, on a file OneDrive keeps online
/// only, which reading would download.
const ONLINE_ONLY: u32 = 0x0040_0000;

static FOLDERS: RwLock<Vec<PathBuf>> = RwLock::new(Vec::new());

/// The folders `owns` allows, kept in step with the settings.
pub fn set_folders(folders: &[PathBuf]) {
    *FOLDERS.write().unwrap_or_else(PoisonError::into_inner) = folders.to_vec();
}

pub fn folders() -> Vec<PathBuf> {
    FOLDERS
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// Whether `path` is in a folder the user added. Paths anywhere else, as
/// YouTube's data might hold, are never read, so no share they didn't pick
/// is reached.
pub fn owns(path: &Path) -> bool {
    if !path.is_absolute() || path.components().any(|c| c == Component::ParentDir) {
        return false;
    }
    (FOLDERS.read().unwrap_or_else(PoisonError::into_inner))
        .iter()
        .any(|folder| path.starts_with(folder))
}

/// Which of the local tracks `paths` have lost their files: missing while
/// the folder they're in is there, so an unplugged drive or a sleeping share
/// loses nothing. Blocking.
pub fn gone(paths: HashSet<String>) -> HashSet<String> {
    if paths.is_empty() {
        return paths;
    }
    gone_from(paths, &folders())
}

fn gone_from(mut paths: HashSet<String>, folders: &[PathBuf]) -> HashSet<String> {
    let reachable: Vec<&PathBuf> = folders
        .iter()
        .filter(|f| matches!(f.try_exists(), Ok(true)))
        .collect();
    paths.retain(|path| {
        let path = Path::new(path);
        reachable.iter().any(|f| path.starts_with(f)) && matches!(path.try_exists(), Ok(false))
    });
    paths
}

/// The folders to list, without those inside another, which are listed with it.
fn roots(folders: &[PathBuf]) -> Vec<PathBuf> {
    let mut roots = folders.to_vec();
    roots.sort();
    roots.dedup_by(|inner, outer| inner.starts_with(outer));
    roots
}

fn library_path() -> PathBuf {
    crate::settings::data_dir().join("local.json")
}

/// A file as read, with what its track sorts and shares a cover by.
#[derive(Serialize, Deserialize)]
struct Song {
    track: Track,
    /// The album artist, or else the track's own.
    by: String,
    disc: u16,
    cover: bool,
}

/// Each file's song by path, with its modified time since 1970 when read, so
/// a scan reads only new or changed files. One that won't open is kept as
/// `None`, so it isn't tried again until it changes.
#[derive(Default, Serialize, Deserialize)]
pub struct Library {
    files: HashMap<PathBuf, (Duration, Option<Song>)>,
    /// The online-only files the last scan listed, which `files` leaves out.
    #[serde(skip)]
    online: HashSet<String>,
    /// The folders the last scan couldn't list, whose files it left out.
    #[serde(skip)]
    away: HashSet<PathBuf>,
    /// Counts the scans that found a change, so a list shown can tell it's old.
    #[serde(skip)]
    revision: u64,
}

impl Library {
    pub fn load() -> Self {
        crate::settings::read_json(&library_path()).unwrap_or_default()
    }

    pub fn save(&self) {
        crate::settings::save_json(&library_path(), self, "the local files");
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// The tracks in `folders` and below, by album artist, album, disc and
    /// track number, and whether any file was new, changed or gone. Blocking.
    pub fn scan(&mut self, folders: &[PathBuf]) -> (Vec<Track>, bool) {
        let mut found = HashMap::with_capacity(self.files.len());
        let mut kept = Vec::new();
        let mut away = HashSet::new();
        // Listed by name and not kept, so they're read once downloaded.
        let mut online = Vec::new();
        let mut changed = false;
        let roots = roots(folders);
        let mut dirs = roots.clone();
        while let Some(dir) = dirs.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(e) => {
                    log::debug!("could not list a music folder: {e}");
                    // An unplugged drive or a sleeping share keeps what was
                    // read of it, so it isn't all read again once it's back.
                    if roots.contains(&dir) {
                        let before = kept.len();
                        kept.extend(self.files.extract_if(|path, _| path.starts_with(&dir)));
                        if kept.len() > before {
                            away.insert(dir.clone());
                        }
                    }
                    continue;
                }
            };
            for entry in entries.flatten() {
                // Links aren't followed, so a loop of them can't trap the scan.
                let (Ok(kind), Ok(metadata)) = (entry.file_type(), entry.metadata()) else {
                    continue;
                };
                let path = entry.path();
                if kind.is_dir() {
                    if metadata.file_attributes() & HIDDEN_AND_SYSTEM != HIDDEN_AND_SYSTEM {
                        dirs.push(path);
                    }
                    continue;
                }
                // Paths that aren't Unicode can't be a track's ID.
                if !kind.is_file() || !is_audio(&path) || path.to_str().is_none() {
                    continue;
                }
                let Ok(modified) = metadata.modified() else {
                    continue;
                };
                // A file dated before 1970 reads as 1970.
                let modified =
                    (modified.duration_since(SystemTime::UNIX_EPOCH)).unwrap_or_default();
                let song = match self.files.remove(&path) {
                    Some((at, song)) if at == modified => song,
                    _ if metadata.file_attributes() & ONLINE_ONLY != 0 => {
                        online.extend(song(&path, Tags::default(), false, None));
                        continue;
                    }
                    _ => match read_song(&path) {
                        Ok(song) => {
                            changed = true;
                            song
                        }
                        // Not kept, so it's tried again next time.
                        Err(e) => {
                            log::debug!("could not read an audio file: {e}");
                            continue;
                        }
                    },
                };
                found.insert(path, (modified, song));
            }
        }
        changed |= !self.files.is_empty();
        let online_now: HashSet<String> = online.iter().map(|s| s.track.id.clone()).collect();
        // Their songs aren't listed while they're away, nor kept in `files`.
        changed |= online_now != self.online || away != self.away;
        self.online = online_now;
        self.away = away;
        self.revision += u64::from(changed);
        self.files = found;
        let songs = self.files.values().filter_map(|(_, s)| s.as_ref());
        let tracks = tracks(songs.chain(&online));
        self.files.extend(kept);
        (tracks, changed)
    }
}

/// `songs` in order as tracks.
fn tracks<'a>(songs: impl Iterator<Item = &'a Song>) -> Vec<Track> {
    let mut songs: Vec<&Song> = songs.collect();
    songs.sort_by_cached_key(|&s| {
        (
            s.by.to_lowercase(),
            s.track.album_name().to_lowercase(),
            s.disc,
            s.track.track_nr,
            s.track.title.to_lowercase(),
            // So files alike keep their places from one scan to the next.
            s.track.id.as_str(),
        )
    });
    let mut tracks = Vec::with_capacity(songs.len());
    let same_album = |a: &&Song, b: &&Song| {
        a.track.album.is_some() && a.by == b.by && a.track.album == b.track.album
    };
    for album in songs.chunk_by(same_album) {
        // One file's cover for the whole album, so it is read and decoded once.
        let cover = album.iter().find(|s| s.cover).map(|s| s.track.id.clone());
        tracks.extend(album.iter().map(|s| Track {
            thumb: cover.clone(),
            ..s.track.clone()
        }));
    }
    tracks
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A 16-bit WAV of interleaved `samples`.
    pub(crate) fn write_wav(path: &Path, samples: &[f32], rate: u32, channels: u16) {
        let data: Vec<u8> = samples
            .iter()
            .flat_map(|s| ((s * 32_767.0) as i16).to_le_bytes())
            .collect();
        let mut wav = b"RIFF".to_vec();
        wav.extend((36 + data.len() as u32).to_le_bytes());
        wav.extend(b"WAVEfmt ");
        wav.extend(16u32.to_le_bytes());
        wav.extend(1u16.to_le_bytes()); // PCM
        wav.extend(channels.to_le_bytes());
        wav.extend(rate.to_le_bytes());
        wav.extend((rate * 2 * u32::from(channels)).to_le_bytes());
        wav.extend((2 * channels).to_le_bytes());
        wav.extend(16u16.to_le_bytes());
        wav.extend(b"data");
        wav.extend((data.len() as u32).to_le_bytes());
        wav.extend(data);
        std::fs::write(path, wav).unwrap();
    }

    #[test]
    fn reads_an_untagged_file_by_its_name() {
        let dir = std::env::temp_dir().join(format!("lily-untagged-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Some Song.wav");
        write_wav(&path, &[0.0; 16_000], 8_000, 1);
        let (tracks, _) = Library::default().scan(std::slice::from_ref(&dir));
        std::fs::remove_dir_all(&dir).unwrap();
        let [track] = tracks.as_slice() else {
            panic!("one track, found {}", tracks.len());
        };
        assert_eq!(track.id, path.to_str().unwrap());
        assert_eq!(track.title, "Some Song");
        assert_eq!(track.duration, Some(2));
        assert!(track.artists.is_empty() && track.album.is_none());
        assert_eq!(track.thumb, None, "no cover to read");
    }

    fn tagged(id: &str, by: &str, album: Option<&str>, disc: u16, nr: u16, cover: bool) -> Song {
        Song {
            track: Track {
                id: id.into(),
                album: album.map(|a| (a.into(), a.into())),
                track_nr: Some(nr),
                ..Default::default()
            },
            by: by.into(),
            disc,
            cover,
        }
    }

    #[test]
    fn orders_albums_by_disc_and_shares_their_cover() {
        let songs = [
            tagged("2-1", "Band", Some("Album"), 2, 1, true),
            tagged("1-2", "Band", Some("Album"), 1, 2, false),
            tagged("1-1", "band", Some("Album"), 1, 1, false),
            tagged("loose", "Band", None, 0, 1, true),
            tagged("plain", "Band", None, 0, 2, false),
        ];
        let tracks = tracks(songs.iter());
        let order: Vec<_> = tracks
            .iter()
            .map(|t| (t.id.as_str(), t.thumb.as_deref()))
            .collect();
        assert_eq!(
            order,
            [
                ("loose", Some("loose")),
                ("plain", None),
                // Named differently, so not the same album's cover.
                ("1-1", None),
                ("1-2", Some("2-1")),
                ("2-1", Some("2-1")),
            ]
        );
    }

    #[test]
    fn owns_only_the_folders_added() {
        set_folders(&[PathBuf::from(r"C:\Music")]);
        assert!(owns(Path::new(r"C:\Music\Band\a.mp3")));
        assert!(!owns(Path::new(r"C:\Music\..\Windows\win.ini")));
        assert!(!owns(Path::new(r"C:\Musicals\a.mp3")));
        assert!(!owns(Path::new(r"\\host\share\a.jpg")));
        assert!(!owns(Path::new("//yt3.ggpht.com/a")));
    }

    #[test]
    fn a_file_is_gone_only_from_a_folder_still_there() {
        let dir = std::env::temp_dir().join(format!("lily-gone-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("here.wav"), b"").unwrap();
        let unplugged = dir.with_extension("unplugged");
        let path = |p: &Path| p.to_str().unwrap().to_owned();
        let paths = HashSet::from([
            path(&dir.join("here.wav")),
            path(&dir.join("deleted.wav")),
            path(&unplugged.join("away.wav")),
        ]);
        let gone = gone_from(paths, &[dir.clone(), unplugged]);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(gone, HashSet::from([path(&dir.join("deleted.wav"))]));
    }

    #[test]
    fn a_local_track_never_passes_for_a_youtube_one() {
        let track = Track {
            id: r"C:\Music\ab.mp3".into(),
            album: Some(("Album".into(), "Album".into())),
            ..Default::default()
        };
        assert!(track.is_local());
        assert!(!crate::api::is_video_id(&track.id));
        assert_eq!(track.album_id(), None, "no YouTube album to open");
        let youtube = Track {
            id: "dQw4w9WgXcQ".into(),
            ..Default::default()
        };
        assert!(!youtube.is_local());
        assert!(crate::api::is_video_id(&youtube.id));
    }

    #[test]
    fn rescans_only_what_changed() {
        let dir = std::env::temp_dir().join(format!("lily-local-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("inner")).unwrap();
        // Not audio, so it reads as nothing and is kept as such.
        std::fs::write(dir.join("inner").join("broken.mp3"), b"not audio").unwrap();
        std::fs::write(dir.join("cover.jpg"), b"jpeg").unwrap();
        let mut library = Library::default();
        let folders = [dir.clone(), dir.join("inner")];
        assert_eq!(library.scan(&folders), (Vec::new(), true));
        assert_eq!(library.files.len(), 1, "found once, though in two folders");
        assert_eq!(library.scan(&folders), (Vec::new(), false), "nothing new");
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(
            library.scan(&folders),
            (Vec::new(), true),
            "its songs aren't listed while it's away"
        );
        assert_eq!(
            library.files.len(),
            1,
            "a folder that is away keeps its files"
        );
        std::fs::create_dir_all(dir.join("inner")).unwrap();
        assert_eq!(library.scan(&folders), (Vec::new(), true));
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(library.files.is_empty(), "a file gone is forgotten");
    }
}
