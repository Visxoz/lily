//! Animated artwork for the full player: a song's Spotify Canvas once signed
//! in to Spotify, else its album's Apple Music motion cover, kept on disk as
//! MP4 clips that `video` decodes.

mod apple;
mod hls;
mod names;
mod spotify;
pub mod video;

use std::{
    collections::HashMap,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Mutex, MutexGuard, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
};

use url::Url;

use crate::{api::Track, settings::touch};

/// Answers kept for the run, at a few hundred bytes each.
const LOOKUP_LIMIT: usize = 500;
/// About 25 Apple covers at 8 MB, or 50 Canvases at 4 MB.
const DISK_BUDGET: u64 = 200 << 20;
/// Far past any clip, so a hostile answer can't fill the disk.
const CLIP_LIMIT: usize = 64 << 20;

pub struct Motion {
    dir: PathBuf,
    apple: apple::Apple,
    spotify: spotify::Spotify,
    /// The `sp_dc` cookie, while signed in to Spotify.
    cookie: Mutex<Option<String>>,
    /// By song (a Canvas) or album (Apple), the clip's address or none; one
    /// that couldn't be asked isn't kept, so the next play asks again.
    lookups: Mutex<HashMap<String, Option<Url>>>,
}

fn cookie_path() -> PathBuf {
    crate::settings::data_dir().join("spotify.bin")
}

/// Nothing done under these locks can leave their data invalid, so a panic
/// elsewhere needn't lose it.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Motion {
    pub fn new() -> Self {
        let dir = crate::settings::data_dir().join("motion");
        Self {
            apple: apple::Apple::new(apple::storefront(), dir.join("apple-token.txt")),
            spotify: spotify::Spotify::new(dir.join("spotify.json")),
            cookie: Mutex::new(crate::platform::secret::read(&cookie_path())),
            lookups: Mutex::default(),
            dir,
        }
    }

    pub fn spotify_signed_in(&self) -> bool {
        lock(&self.cookie).is_some()
    }

    /// Checks the cookie with Spotify, then keeps it, encrypted.
    pub async fn sign_in_to_spotify(&self, text: &str) -> Result<(), String> {
        let cookie = spotify::cookie_value(text).ok_or("that isn't Spotify's session")?;
        // Before the check, so the token it mints serves the first Canvas.
        self.spotify.forget();
        self.spotify
            .check(crate::http::motion_client(), &cookie)
            .await
            .map_err(|e| match e {
                spotify::CanvasError::SignedOut => "Spotify didn't sign you in".to_owned(),
                spotify::CanvasError::Failed(e) => e,
            })?;
        crate::platform::secret::write(&cookie_path(), &cookie)
            .map_err(|_| "couldn't save the sign-in")?;
        self.set_cookie(Some(cookie));
        Ok(())
    }

    pub fn sign_out_of_spotify(&self) {
        if let Err(e) = std::fs::remove_file(cookie_path())
            && e.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!("could not delete the Spotify sign-in: {e}");
        }
        self.set_cookie(None);
    }

    /// Answers had under another session aren't this one's.
    fn set_cookie(&self, cookie: Option<String>) {
        if cookie.is_none() {
            self.spotify.forget();
        }
        *lock(&self.cookie) = cookie;
        lock(&self.lookups).retain(|key, _| !key.starts_with(CANVAS));
    }

    /// The track's clip on disk, downloaded first should it need to be:
    /// `Ok(None)` when it has none.
    pub async fn clip(&self, track: &Track) -> Result<Option<PathBuf>, String> {
        if track.is_local() {
            return Ok(None);
        }
        let cookie = lock(&self.cookie).clone();
        if let Some(cookie) = cookie
            && let Some(name) = file_name(&track.id, "canvas")
        {
            let path = self.dir.join(name);
            if touch(&path) {
                return Ok(Some(path));
            }
            let key = format!("{CANVAS}{}", track.id);
            // Spotify failing still leaves Apple's cover.
            match self.lookup(key, self.canvas(&cookie, track)).await {
                Ok(Some(url)) => match self.download(&url, &path).await {
                    Ok(()) => return Ok(Some(path)),
                    Err(e) => log::warn!("{e}"),
                },
                Ok(None) => {}
                Err(e) => log::warn!("Spotify's Canvas couldn't be had: {e}"),
            }
        }
        let (Some(album), Some(artist)) = (track.album_id(), track.artists.first()) else {
            return Ok(None);
        };
        let Some(name) = file_name(album, "square") else {
            return Ok(None);
        };
        let path = self.dir.join(name);
        if touch(&path) {
            return Ok(Some(path));
        }
        let client = crate::http::motion_client();
        let search = self
            .apple
            .square_cover(client, track.album_name(), &artist.name);
        let Some(master) = self.lookup(format!("{APPLE}{album}"), search).await? else {
            return Ok(None);
        };
        let url = apple::clip_url(client, &master).await?;
        self.download(&url, &path).await?;
        Ok(Some(path))
    }

    async fn canvas(&self, cookie: &str, track: &Track) -> Result<Option<Url>, String> {
        let artists: Vec<String> = track.artists.iter().map(|a| a.name.clone()).collect();
        let album = Some(track.album_name()).filter(|a| !a.is_empty());
        let found = self
            .spotify
            .canvas(
                crate::http::motion_client(),
                cookie,
                &track.title,
                &artists,
                album,
                track.duration,
            )
            .await;
        match found {
            Ok(url) => Ok(url),
            Err(spotify::CanvasError::SignedOut) => {
                // Unless signed in again meanwhile.
                if lock(&self.cookie).as_deref() == Some(cookie) {
                    log::warn!("Spotify's sign-in no longer works, so it was forgotten");
                    self.sign_out_of_spotify();
                }
                // An error, so the memo never keeps this session's answer for the next.
                Err("Spotify took its sign-in as signed out".to_owned())
            }
            Err(spotify::CanvasError::Failed(e)) => Err(e),
        }
    }

    /// `ask`'s answer, kept for the run once it has one.
    async fn lookup(
        &self,
        key: String,
        ask: impl Future<Output = Result<Option<Url>, String>>,
    ) -> Result<Option<Url>, String> {
        if let Some(found) = lock(&self.lookups).get(&key) {
            return Ok(found.clone());
        }
        let found = ask.await?;
        let mut lookups = lock(&self.lookups);
        // All at once, as asking again costs only a search.
        if lookups.len() >= LOOKUP_LIMIT {
            lookups.clear();
        }
        lookups.insert(key, found.clone());
        Ok(found)
    }

    /// Streamed to a file of its own and renamed in, so the clip is never
    /// held in memory and one cut short is never kept.
    async fn download(&self, url: &Url, path: &Path) -> Result<(), String> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        if !crate::http::motion_allowed(url) {
            return Err("refused a clip from an unexpected host".to_owned());
        }
        let response = crate::http::motion_client()
            .get(url.clone())
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| format!("could not download a clip: {}", e.without_url()))?;
        let keep_failed = |e: std::io::Error| format!("could not keep a clip: {e}");
        std::fs::create_dir_all(&self.dir).map_err(keep_failed)?;
        let partial = path.with_extension(format!("{}.part", NEXT.fetch_add(1, Ordering::Relaxed)));
        let mut file = std::fs::File::create(&partial).map_err(keep_failed)?;
        let written = crate::http::each_chunk(response, CLIP_LIMIT, |chunk| {
            file.write_all(chunk).map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| format!("could not download a clip: {e}"));
        // Closed first, as Windows renames no open file.
        drop(file);
        let kept = written.and_then(|()| std::fs::rename(&partial, path).map_err(keep_failed));
        if kept.is_err() {
            let _ = std::fs::remove_file(&partial);
        }
        kept?;
        prune(&self.dir, path, DISK_BUDGET);
        Ok(())
    }
}

const CANVAS: &str = "canvas ";
const APPLE: &str = "apple ";

/// A Canvas's by song, Apple's by album, so songs sharing their album's cover
/// play it on; only the ID's safe characters, as YouTube supplied it.
fn file_name(id: &str, kind: &str) -> Option<String> {
    let id: String = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
        .collect();
    (!id.is_empty()).then(|| format!("{id}-{kind}.mp4"))
}

/// Deletes the clips used longest ago until the rest fit `budget`, never the
/// one just kept. A `.part` counts too, so one a crash left behind goes in
/// turn; one being written is the newest, so it never does.
fn prune(dir: &Path, kept: &Path, budget: u64) {
    crate::settings::prune(
        dir,
        budget,
        None,
        |path| path != kept && std::fs::remove_file(path).is_ok(),
        |path| path.extension().is_some_and(|x| x == "mp4" || x == "part"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_clips_by_the_ids_safe_characters() {
        assert_eq!(
            file_name("MPREb_a-1", "square").as_deref(),
            Some("MPREb_a-1-square.mp4")
        );
        assert_eq!(
            file_name("..\\x/y", "canvas").as_deref(),
            Some("xy-canvas.mp4")
        );
        assert_eq!(file_name("../", "canvas"), None);
    }

    #[test]
    fn prunes_the_oldest_clips_past_the_budget() {
        let dir = std::env::temp_dir().join(format!("lily-motion-prune-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        for (name, age) in [("a.mp4", 4), ("z.0.part", 3), ("b.mp4", 2), ("c.mp4", 1)] {
            let path = dir.join(name);
            std::fs::write(&path, [0; 10]).unwrap();
            let file = std::fs::File::options().write(true).open(&path).unwrap();
            file.set_modified(old - std::time::Duration::from_secs(age))
                .unwrap();
        }
        std::fs::write(dir.join("token.txt"), [0; 100]).unwrap();
        // The oldest go, but never the one just kept.
        prune(&dir, &dir.join("a.mp4"), 20);
        assert!(dir.join("a.mp4").exists());
        assert!(!dir.join("z.0.part").exists(), "a crash's leftover counts");
        assert!(!dir.join("b.mp4").exists());
        assert!(dir.join("c.mp4").exists());
        assert!(dir.join("token.txt").exists(), "only clips count");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
