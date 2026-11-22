//! Loads artwork off the UI thread and caches decoded images on it.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

use slint::{Color, Image};
use tokio::{
    runtime::Handle,
    sync::{OwnedSemaphorePermit, Semaphore, oneshot},
};

mod colour;
mod disk;

use colour::{Decoded, decode};
pub use colour::{readable, vivid};
pub use disk::prune_disk;
use disk::{disk_dir, disk_name, save};

// Logical sizes artwork is drawn at.
pub const ROW: u32 = 48;
pub const PLAYER: u32 = 64;
pub const TOP: u32 = 96;
pub const CARD: u32 = 176;
pub const HEADER: u32 = 232;
pub const BANNER: u32 = 360;
pub const COVER: u32 = 560;
pub const AVATAR: u32 = 26;

/// Decoded artwork kept for reuse, in bytes. Images still on screen stay
/// alive regardless, as Slint holds its own reference.
const CACHE_BUDGET: usize = 32 * 1024 * 1024;
/// The budget while the window is hidden, when nothing draws artwork: the
/// latest few songs' player art, which plays on in the tray brings.
const HIDDEN_BUDGET: usize = 2 * 1024 * 1024;
/// Covers, very wide banners, and headers from 225% scale (cards from 300%):
/// dropped once unused for `LARGE_UNUSED_FOR`, even under the budget.
const LARGE: usize = 1024 * 1024;
const LARGE_UNUSED_FOR: Duration = Duration::from_secs(5 * 60);
/// Thumbnails are a few KB each and share HTTP/2 connections, so a page's
/// worth is fetched widely in parallel.
const PARALLEL_FETCHES: usize = 16;

thread_local! {
    static CACHE: RefCell<Cache> = RefCell::new(Cache::default());
    /// Images being fetched, with whoever else is waiting for each.
    static PENDING: RefCell<HashMap<String, Vec<oneshot::Sender<Option<Image>>>>> =
        RefCell::new(HashMap::new());
    /// Window scale factor, so artwork decodes at the size it is drawn.
    static SCALE: Cell<f32> = const { Cell::new(1.0) };
}

pub fn set_scale(scale: f32) {
    SCALE.with(|s| s.set(scale.clamp(1.0, 4.0)));
}

/// Shrinks the cache to `HIDDEN_BUDGET` while the window is hidden, and lets
/// it grow again once shown. Images on the shown page and in the queue stay,
/// as their models hold them, so only what Back or a rebuild would reuse goes,
/// to be read from disk again.
pub fn set_hidden(hidden: bool) {
    CACHE.with(|c| {
        let mut cache = c.borrow_mut();
        cache.budget = if hidden { HIDDEN_BUDGET } else { CACHE_BUDGET };
        cache.shrink();
    });
}

struct Cache {
    images: HashMap<String, Cached>,
    accents: HashMap<String, Color>,
    backdrops: HashMap<String, Image>,
    bytes: usize,
    budget: usize,
}

impl Default for Cache {
    fn default() -> Self {
        Self {
            images: HashMap::new(),
            accents: HashMap::new(),
            backdrops: HashMap::new(),
            bytes: 0,
            budget: CACHE_BUDGET,
        }
    }
}

struct Cached {
    image: Image,
    bytes: usize,
    used: Instant,
}

impl Cache {
    /// Marks `key` as just used, so eviction keeps it longest.
    fn used(&mut self, key: &str, now: Instant) -> Option<&Image> {
        let entry = self.images.get_mut(key)?;
        entry.used = now;
        Some(&entry.image)
    }

    fn insert(&mut self, key: String, image: Image, now: Instant) {
        let size = image.size();
        let bytes = size.width as usize * size.height as usize * 4;
        self.bytes += bytes;
        let entry = Cached {
            image,
            bytes,
            used: now,
        };
        if let Some(old) = self.images.insert(key, entry) {
            self.bytes -= old.bytes;
        }
        // On insert, which each new song's art brings, so a cover opened once
        // doesn't pin megabytes for days.
        self.evict(|e| e.bytes >= LARGE && now.duration_since(e.used) > LARGE_UNUSED_FOR);
        self.shrink();
    }

    /// Drops the least recently used images once over budget, down to 7/8 of
    /// it, so a full cache sorts once per few hundred thumbnails rather than on
    /// every one. Sorted only when over, so a hit (per row on scroll) stays one write.
    fn shrink(&mut self) {
        if self.bytes <= self.budget {
            return;
        }
        let mut ages: Vec<_> = self.images.values().map(|e| (e.used, e.bytes)).collect();
        ages.sort_unstable_by_key(|&(used, _)| used);
        let mut excess = self.bytes - self.budget / 8 * 7;
        let cutoff = ages.into_iter().find_map(|(used, bytes)| {
            excess = excess.saturating_sub(bytes);
            (excess == 0).then_some(used)
        });
        if let Some(cutoff) = cutoff {
            self.evict(|e| e.used <= cutoff);
        }
    }

    /// Drops every image `stale` picks, with its accent and backdrop.
    fn evict(&mut self, stale: impl Fn(&Cached) -> bool) {
        let Self {
            images,
            accents,
            backdrops,
            bytes,
            ..
        } = self;
        images.retain(|key, e| {
            if !stale(e) {
                return true;
            }
            *bytes -= e.bytes;
            accents.remove(key);
            backdrops.remove(key);
            false
        });
    }
}

/// Song rows ask once on screen, after a page's cards are queued, so they
/// get their own downloads rather than wait behind every card.
fn permits(edge: u32) -> &'static Arc<Semaphore> {
    static ROWS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    static OTHERS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    let pool = if edge == ROW { &ROWS } else { &OTHERS };
    pool.get_or_init(|| Arc::new(Semaphore::new(PARALLEL_FETCHES)))
}

/// Returns a cached image without fetching. `edge` is in logical pixels.
pub fn cached(url: &str, edge: u32) -> Option<Image> {
    cached_key(&key(url, edge))
}

fn cached_key(key: &str) -> Option<Image> {
    CACHE.with(|c| c.borrow_mut().used(key, Instant::now()).cloned())
}

/// The cached artwork for `url`, or an empty image, which the UI shows as a
/// placeholder until it loads.
pub fn cached_or_empty(url: Option<&str>, edge: u32) -> Image {
    url.and_then(|url| cached(url, edge)).unwrap_or_default()
}

/// A colour from the artwork for backgrounds behind it, once loaded.
pub fn accent(url: &str, edge: u32) -> Option<Color> {
    CACHE.with(|c| c.borrow().accents.get(&key(url, edge)).copied())
}

/// What is drawn behind artwork at `edge`, once loaded: the player's colours
/// where they sit, or a page header's blurred copy.
pub fn backdrop(url: &str, edge: u32) -> Option<Image> {
    CACHE.with(|c| c.borrow().backdrops.get(&key(url, edge)).cloned())
}

fn key(url: &str, edge: u32) -> String {
    format!("{}:{url}", physical(edge))
}

fn physical(edge: u32) -> u32 {
    (edge as f32 * SCALE.with(Cell::get)).round() as u32
}

/// Fetches `url` at `edge` logical pixels, on the UI thread. Requests for an
/// image already on its way share it; one no longer `wanted` by anyone is
/// skipped when its turn comes, as a page's art is once the page is left.
pub async fn load(
    rt: &Handle,
    url: &str,
    edge: u32,
    wanted: impl FnOnce() -> bool,
) -> Option<Image> {
    let key = key(url, edge);
    if let Some(image) = cached_key(&key) {
        return Some(image);
    }
    let waiting = PENDING.with(|p| {
        let mut pending = p.borrow_mut();
        match pending.get_mut(&key) {
            Some(waiters) => {
                let (tx, rx) = oneshot::channel();
                waiters.push(tx);
                Some(rx)
            }
            None => {
                pending.insert(key.clone(), Vec::new());
                None
            }
        }
    });
    if let Some(rx) = waiting {
        return rx.await.ok().flatten();
    }

    // Large single images skip the queue rather than wait behind thumbnails.
    // The permit is taken here so `wanted` is checked before downloading.
    let permit = if matches!(edge, PLAYER | HEADER | BANNER | COVER) {
        None
    } else {
        Arc::clone(permits(edge)).acquire_owned().await.ok()
    };
    let alone = PENDING.with(|p| p.borrow().get(&key).is_some_and(Vec::is_empty));
    let fetched = if alone && !wanted() {
        None
    } else {
        fetch(rt, url.to_owned(), edge, permit).await
    };
    let image = fetched.map(|(buffer, accent, backdrop)| {
        let image = Image::from_rgba8(buffer);
        CACHE.with(|c| {
            let mut cache = c.borrow_mut();
            if let Some(accent) = accent {
                cache.accents.insert(key.clone(), accent);
            }
            if let Some(backdrop) = backdrop {
                cache
                    .backdrops
                    .insert(key.clone(), Image::from_rgba8(backdrop));
            }
            cache.insert(key.clone(), image.clone(), Instant::now());
        });
        image
    });
    let waiters = PENDING.with(|p| p.borrow_mut().remove(&key));
    for tx in waiters.into_iter().flatten() {
        let _ = tx.send(image.clone());
    }
    image
}

async fn fetch(
    rt: &Handle,
    url: String,
    edge: u32,
    permit: Option<OwnedSemaphorePermit>,
) -> Option<Decoded> {
    let pixels = physical(edge);
    // A download's cover, or a local file's own. Any other path fails the
    // host check below.
    let file = crate::downloads::cover(&url)
        .or_else(|| crate::local::owns(url.as_ref()).then(|| url.clone().into()));
    if let Some(file) = file {
        // A file's own picture decodes at full size before scaling, tens of
        // MB for a large one, so only a couple at once.
        static FILES: OnceLock<Arc<Semaphore>> = OnceLock::new();
        let files = FILES.get_or_init(|| Arc::new(Semaphore::new(2)));
        let decoding = Arc::clone(files).acquire_owned().await.ok();
        return rt
            .spawn_blocking(move || {
                let _permits = (permit, decoding);
                decode(&crate::local::artwork(&file)?, pixels, edge)
            })
            .await
            .ok()?;
    }
    if !crate::http::allowed(&url) {
        log::debug!("skipped artwork from a host YouTube does not use");
        return None;
    }
    // googleusercontent renders any size, so ask for the pixels drawn rather
    // than download a larger rendition and scale it down. Banners keep the
    // wide rendition they came with, as a square size would crop them.
    let url = if edge == BANNER {
        url
    } else {
        crate::api::resize_thumb(&url, pixels)
    };
    rt.spawn(async move {
        let _permit = permit;
        let kept = disk_dir().join(disk_name(&url));
        let kept =
            tokio::task::spawn_blocking(move || decode(&std::fs::read(kept).ok()?, pixels, edge));
        if let Ok(Some(decoded)) = kept.await {
            return Some(decoded);
        }
        let bytes = crate::http::client()
            .get(&url)
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .ok()?
            .bytes()
            .await
            .ok()?;
        let decoded = tokio::task::spawn_blocking({
            let bytes = bytes.clone();
            move || decode(&bytes, pixels, edge)
        })
        .await
        .ok()??;
        // Only bytes that decoded are kept, and after the image is on its way.
        tokio::task::spawn_blocking(move || save(disk_dir(), &url, &bytes));
        Some(decoded)
    })
    .await
    .ok()?
}

#[cfg(test)]
mod tests {
    use super::*;
    use slint::SharedPixelBuffer;

    fn blank(edge: u32) -> Image {
        Image::from_rgba8(SharedPixelBuffer::new(edge, edge))
    }

    #[test]
    fn evicts_the_least_recently_used() {
        let mut cache = Cache::default();
        let start = Instant::now();
        let at = |s| start + Duration::from_secs(s);
        // Eight 4 MB images fill the budget exactly.
        for i in 0..8 {
            cache.insert(i.to_string(), blank(1024), at(i));
        }
        cache.insert("7".into(), blank(1024), at(7));
        assert_eq!(cache.bytes, CACHE_BUDGET, "a key put in again counts once");
        assert!(cache.used("0", at(8)).is_some());
        cache.accents.insert("1".into(), Color::default());
        cache.backdrops.insert("1".into(), blank(4));
        cache.insert("8".into(), blank(1024), at(9));
        assert!(cache.images.contains_key("0"), "used most recently but one");
        assert!(!cache.images.contains_key("1"), "the least recently used");
        assert!(
            !cache.images.contains_key("2"),
            "down to the low-water mark"
        );
        assert!(cache.images.contains_key("3"));
        assert!(!cache.accents.contains_key("1") && !cache.backdrops.contains_key("1"));
        assert_eq!(cache.bytes, CACHE_BUDGET / 8 * 7);
    }

    #[test]
    fn keeps_little_while_hidden() {
        let mut cache = Cache::default();
        let start = Instant::now();
        // 1 MB each, at 512 by 512.
        for i in 0..8 {
            cache.insert(i.to_string(), blank(512), start + Duration::from_secs(i));
        }
        cache.budget = HIDDEN_BUDGET;
        cache.shrink();
        let kept: Vec<_> = (0..8)
            .filter(|i| cache.images.contains_key(&i.to_string()))
            .collect();
        assert_eq!(kept, [7], "the latest, within 7/8 of 2 MB");
        cache.budget = CACHE_BUDGET;
        cache.insert("8".into(), blank(512), start + Duration::from_secs(8));
        assert_eq!(cache.images.len(), 2, "grows again once shown");
    }

    #[test]
    fn drops_large_images_left_unused() {
        let mut cache = Cache::default();
        let start = Instant::now();
        cache.insert("cover".into(), blank(512), start);
        cache.insert("kept cover".into(), blank(512), start);
        cache.insert("row".into(), blank(48), start);
        let later = start + LARGE_UNUSED_FOR + Duration::from_secs(1);
        cache.used("kept cover", later);
        cache.insert("next".into(), blank(64), later);
        assert!(!cache.images.contains_key("cover"));
        assert!(cache.images.contains_key("kept cover"));
        assert!(cache.images.contains_key("row"), "small images stay");
        assert_eq!(cache.bytes, (512 * 512 + 48 * 48 + 64 * 64) * 4);
    }
}
