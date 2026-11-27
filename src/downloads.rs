//! Songs kept for playing offline: each stream's file as YouTube served it
//! and its cover, in `downloads\`, indexed by `downloads.json`.

use std::{
    cell::RefCell,
    collections::HashMap,
    path::PathBuf,
    time::{Duration, SystemTime},
};

use serde::{Deserialize, Serialize};

use crate::{
    api::Track,
    audio::{self, Refresh, Source},
    audio_cache::AudioCache,
    settings,
    streams::ResolvedStream,
};

thread_local! {
    /// Each saved cover by the thumbnail it was saved from, so artwork shows
    /// from the file wherever the song does, online or not.
    static COVERS: RefCell<HashMap<String, PathBuf>> = RefCell::default();
}

/// The saved cover for the thumbnail `url`. On the UI thread.
pub fn cover(url: &str) -> Option<PathBuf> {
    COVERS.with_borrow(|covers| covers.get(url).cloned())
}

fn dir() -> PathBuf {
    settings::data_dir().join("downloads")
}

fn index_path() -> PathBuf {
    settings::data_dir().join("downloads.json")
}

/// IDs are checked to be YouTube's before anything is saved, so they make
/// safe file names.
fn audio_path(id: &str, mime: &str) -> PathBuf {
    let extension = if mime.contains("webm") { "webm" } else { "m4a" };
    dir().join(format!("{id}.{extension}"))
}

/// Whether `id`'s audio is downloaded, in either format. Blocking.
pub fn holds(id: &str) -> bool {
    ["audio/webm", "audio/mp4"]
        .into_iter()
        .any(|mime| audio_path(id, mime).exists())
}

fn cover_path(id: &str) -> PathBuf {
    dir().join(format!("{id}.jpg"))
}

#[derive(Serialize, Deserialize)]
pub struct Saved {
    pub track: Track,
    pub stream: ResolvedStream,
    /// When it was saved, for showing the newest first.
    pub added: SystemTime,
    /// Whether its cover was saved too.
    #[serde(default)]
    cover: bool,
}

impl Saved {
    /// Moves `track`'s stream from the audio cache should it hold that very
    /// file, else downloads it, then its cover, and leaves the cache. Blocking.
    pub fn fetch(
        track: Track,
        stream: ResolvedStream,
        source: Source,
        refresh: Refresh,
    ) -> Result<Self, String> {
        if !crate::api::is_video_id(&track.id) {
            return Err("not a YouTube song".into());
        }
        let saving = |e: std::io::Error| format!("couldn't save it: {e}");
        std::fs::create_dir_all(dir()).map_err(saving)?;
        let cache = AudioCache::default();
        let audio = audio_path(&track.id, &stream.mime);
        let moved = cache.holding(&track.id, &stream).is_some_and(|from| {
            std::fs::rename(from, &audio)
                .map_err(|e| log::debug!("could not move a song played to the downloads: {e}"))
                .is_ok()
        });
        if !moved {
            let blocks = audio::fetch(source, refresh)?;
            let write = |file: &mut std::fs::File| {
                (blocks.iter()).try_for_each(|b| std::io::Write::write_all(file, b))
            };
            settings::write_atomic_parts(&audio, write).map_err(saving)?;
        }
        // Without its cover it still plays, with the cover online.
        let cover = track
            .large_thumb()
            .and_then(|url| fetch_cover(&url))
            .is_some_and(|cover| {
                settings::write_atomic(&cover_path(&track.id), cover.as_ref())
                    .map_err(|e| log::warn!("could not save a cover: {e}"))
                    .is_ok()
            });
        // Not on disk twice.
        cache.remove(&track.id);
        Ok(Self {
            track,
            stream,
            added: SystemTime::now(),
            cover,
        })
    }

    /// Lets its cover show from the file, or no longer.
    fn show_cover(&self, saved: bool) {
        let Some(url) = self.track.thumb.clone().filter(|_| self.cover) else {
            return;
        };
        COVERS.with_borrow_mut(|covers| {
            if saved {
                covers.insert(url, cover_path(&self.track.id));
            } else {
                covers.remove(&url);
            }
        });
    }

    fn remove_files(&self) {
        let audio = audio_path(&self.track.id, &self.stream.mime);
        for path in [audio, cover_path(&self.track.id)] {
            if let Err(e) = std::fs::remove_file(&path)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                log::warn!("could not delete a download: {e}");
            }
        }
    }
}

fn fetch_cover(url: &str) -> Option<impl AsRef<[u8]> + use<>> {
    if !crate::http::allowed(url) {
        return None;
    }
    let response = crate::http::blocking_client()
        .get(url)
        .timeout(Duration::from_secs(20))
        .send()
        .and_then(|r| r.error_for_status())
        .map_err(|e| log::debug!("could not fetch a cover: {}", e.without_url()))
        .ok()?;
    response.bytes().ok()
}

#[derive(Default, Serialize, Deserialize)]
pub struct Downloads {
    songs: HashMap<String, Saved>,
}

impl Downloads {
    pub fn load() -> Self {
        let downloads: Self = settings::read_json(&index_path()).unwrap_or_default();
        downloads.songs.values().for_each(|s| s.show_cover(true));
        downloads
    }

    /// On the UI thread, as it changes only as a song finishes or is removed.
    pub fn save(&self) {
        settings::save_json(&index_path(), self, "the downloads");
    }

    pub fn contains(&self, id: &str) -> bool {
        self.songs.contains_key(id)
    }

    /// The stream to play `id` from, or `None` should it not be downloaded,
    /// or its file have gone.
    pub fn stream(&self, id: &str) -> Option<ResolvedStream> {
        let saved = self.songs.get(id)?;
        let path = audio_path(id, &saved.stream.mime);
        path.is_file().then(|| ResolvedStream {
            file: Some(path),
            ..saved.stream.clone()
        })
    }

    /// Newest first.
    pub fn tracks(&self) -> Vec<Track> {
        let mut saved: Vec<&Saved> = self.songs.values().collect();
        saved.sort_unstable_by_key(|s| std::cmp::Reverse(s.added));
        saved.into_iter().map(|s| s.track.clone()).collect()
    }

    pub fn insert(&mut self, saved: Saved) {
        saved.show_cover(true);
        self.songs.insert(saved.track.id.clone(), saved);
    }

    /// Deletes `id`'s files; returns whether it was downloaded.
    pub fn remove(&mut self, id: &str) -> bool {
        let removed = self.songs.remove(id);
        if let Some(saved) = &removed {
            saved.show_cover(false);
            saved.remove_files();
        }
        removed.is_some()
    }
}
