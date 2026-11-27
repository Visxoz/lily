//! Songs played lately, each stream's file as YouTube served it, in
//! `audio\` beside a sidecar naming it, so they play again without the
//! network; those played longest ago go first past the size Settings sets.

use std::{
    path::{Path, PathBuf},
    sync::{
        Mutex, MutexGuard, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
};

use serde::{Deserialize, Serialize};

use crate::{
    api,
    audio::{self, Keep},
    settings::{self, Quality},
    streams::ResolvedStream,
};

/// What a kept file is, and what it was kept under.
#[derive(Serialize, Deserialize)]
struct Entry {
    /// Its song, as file names ignore case and video IDs don't.
    id: String,
    stream: ResolvedStream,
    quality: Quality,
    /// Kept from the Premium path's answer; one kept without it is asked
    /// for again once that is wanted.
    premium_path: bool,
}

/// Held while pruning, clearing or measuring, so two prunes never count the
/// same files and a size is never read halfway through a clear.
static TIDYING: Mutex<()> = Mutex::new(());

fn tidying() -> MutexGuard<'static, ()> {
    TIDYING.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The size Settings sets, read as each prune runs rather than as its
/// download started, so a change mid-song counts at once.
static BUDGET: AtomicU64 = AtomicU64::new(settings::mb_bytes(settings::DEFAULT_AUDIO_CACHE_MB));

pub fn set_budget(bytes: u64) {
    BUDGET.store(bytes, Ordering::Relaxed);
}

fn budget() -> u64 {
    BUDGET.load(Ordering::Relaxed)
}

/// Copies that failed to play this run, refused until a copy kept after is
/// kept over them.
struct Failed {
    /// Oldest first, each with its failure's number.
    copies: Vec<(PathBuf, u64)>,
    /// Failures so far, so the failing stream's own keep, should it finish
    /// after, lifts no refusal.
    count: u64,
}

static FAILED: Mutex<Failed> = Mutex::new(Failed {
    copies: Vec::new(),
    count: 0,
});
/// Past this many the oldest refusal goes, costing one more failed try of it.
const FAILED_LIMIT: usize = 16;

fn failed() -> MutexGuard<'static, Failed> {
    FAILED.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone)]
pub struct AudioCache {
    dir: PathBuf,
    /// Whether a song is downloaded to keep, which is then never kept here.
    downloaded: fn(&str) -> bool,
}

/// In the data folder, kept through signing in or out.
impl Default for AudioCache {
    fn default() -> Self {
        Self {
            dir: settings::data_dir().join("audio"),
            downloaded: crate::downloads::holds,
        }
    }
}

impl AudioCache {
    /// A song's file and sidecar. IDs are checked to be YouTube's before
    /// any path is made of one.
    fn paths(&self, id: &str) -> Option<(PathBuf, PathBuf)> {
        let path = |extension| self.dir.join(format!("{id}.{extension}"));
        api::is_video_id(id).then(|| (path("audio"), path("json")))
    }

    /// `id`'s sidecar and file, should the file be as long as it says and not
    /// have failed to play. One gone or cut short is deleted unless in use.
    fn whole(&self, id: &str) -> Option<(Entry, PathBuf)> {
        let (audio, sidecar) = self.paths(id)?;
        if failed().copies.iter().any(|(p, _)| *p == audio) {
            return None;
        }
        let entry: Entry = settings::read_json(&sidecar)?;
        if entry.id != id {
            return None;
        }
        let size = entry.stream.size;
        match std::fs::metadata(&audio) {
            Ok(meta) if size > 0 && meta.len() == size => Some((entry, audio)),
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => None,
            // One in use may be a new copy whose sidecar is still to come.
            _ if audio::in_use(&audio) => None,
            _ => {
                self.remove(id);
                None
            }
        }
    }

    /// `id`'s kept stream, should it be whole and kept at `quality` or better,
    /// through the Premium path should that be wanted. Blocking.
    pub fn find(&self, id: &str, quality: Quality, premium_path: bool) -> Option<ResolvedStream> {
        let (entry, audio) = self.whole(id)?;
        if entry.quality < quality || (premium_path && !entry.premium_path) {
            return None;
        }
        settings::touch(&audio);
        Some(ResolvedStream {
            file: Some(audio),
            ..entry.stream
        })
    }

    /// `id`'s kept file, should it be `stream`'s very encode, for Downloads to
    /// move rather than fetch again. Blocking.
    pub fn holding(&self, id: &str, stream: &ResolvedStream) -> Option<PathBuf> {
        let (entry, audio) = self.whole(id)?;
        ((entry.stream.itag, entry.stream.size) == (stream.itag, stream.size)).then_some(audio)
    }

    /// Refuses `id`'s copy, which failed to play, until another is kept over
    /// it, so it neither plays again nor goes to Downloads. Touches no disk.
    pub fn failed_to_play(&self, id: &str) {
        let Some((audio, _)) = self.paths(id) else {
            return;
        };
        let failed = &mut *failed();
        failed.copies.retain(|(p, _)| *p != audio);
        if failed.copies.len() >= FAILED_LIMIT {
            failed.copies.remove(0);
        }
        failed.copies.push((audio, failed.count));
        failed.count += 1;
    }

    /// Where `id`'s stream is kept once played, unless of no known length,
    /// past the budget or not resolved to play. Touches no disk (UI thread).
    pub fn keep(&self, id: &str, stream: &ResolvedStream) -> Option<Keep> {
        let (quality, premium_path) = stream.asked?;
        let (path, sidecar) = self.paths(id)?;
        if stream.size == 0 || stream.size > budget() {
            return None;
        }
        let entry = Entry {
            id: id.to_owned(),
            // Without its first bytes, which the closure would hold all song.
            stream: ResolvedStream {
                head: Default::default(),
                ..stream.clone()
            },
            quality,
            premium_path,
        };
        let (audio, since, cache) = (path.clone(), failed().count, self.clone());
        let kept = move |write: audio::WriteOut| {
            // A better copy kept stays, and a song downloaded needs none.
            let better = (cache.whole(&entry.id)).is_some_and(|(held, _)| {
                (held.quality, held.premium_path) > (quality, premium_path)
            });
            if better || (cache.downloaded)(&entry.id) {
                return Ok(());
            }
            std::fs::create_dir_all(&cache.dir)?;
            settings::write_atomic_parts(&audio, write)?;
            // Checked again once in place, so a download finishing meanwhile
            // either is seen here or removes it after.
            if (cache.downloaded)(&entry.id)
                || !settings::save_json(&sidecar, &entry, "a song played")
            {
                cache.remove(&entry.id);
            } else {
                (failed().copies).retain(|(p, failure)| *p != audio || *failure >= since);
            }
            cache.prune();
            Ok(())
        };
        Some(Keep {
            path,
            kept: Box::new(kept),
        })
    }

    /// Deletes the songs played longest ago until the rest fit the budget,
    /// never one playing or queued. Blocking.
    pub fn prune(&self) {
        let _tidying = tidying();
        self.prune_to(budget());
    }

    fn prune_to(&self, budget: u64) {
        let remove = |song: &Path| {
            let id = song.file_stem().and_then(|id| id.to_str());
            !audio::in_use(song) && id.is_some_and(|id| self.remove(id))
        };
        let songs = |path: &Path| path.extension().is_some_and(|x| x == "audio");
        settings::prune(&self.dir, budget, None, remove, songs);
    }

    /// Every song but those playing or queued, and any still being written.
    /// Blocking.
    pub fn clear(&self) {
        let _tidying = tidying();
        self.prune_to(0);
    }

    /// Run at startup: deletes what a crash left, temporary files and songs or
    /// sidecars without the other, unless their song is in use. Blocking.
    pub fn sweep(&self) {
        let _tidying = tidying();
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        for path in entries.filter_map(|e| Some(e.ok()?.path())) {
            let left = match path.extension().and_then(|x| x.to_str()) {
                Some("json") => !path.with_extension("audio").exists(),
                Some("tmp") => true,
                Some("audio") => !path.with_extension("json").exists(),
                _ => false,
            };
            // A song started since launch writes its temporary files under
            // its song's name, then its sidecar after the rename.
            let song = path
                .file_prefix()
                .map(|id| self.dir.join(id).with_extension("audio"));
            if left && !song.is_some_and(|song| audio::in_use(&song)) {
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    /// The bytes its songs take. Blocking.
    pub fn size(&self) -> u64 {
        let _tidying = tidying();
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return 0;
        };
        entries
            .filter_map(|e| {
                let e = e.ok()?;
                let audio = e.path().extension().is_some_and(|x| x == "audio");
                audio.then(|| e.metadata().ok().map(|m| m.len())).flatten()
            })
            .sum()
    }

    /// Whether `path` is one of its songs.
    pub fn owns(&self, path: &Path) -> bool {
        path.parent() == Some(self.dir.as_path())
    }

    /// Deletes `id`'s song, its sidecar first so it is never found without
    /// its file; returns whether the song went. Blocking.
    pub fn remove(&self, id: &str) -> bool {
        let Some((audio, sidecar)) = self.paths(id) else {
            return false;
        };
        let _ = std::fs::remove_file(sidecar);
        std::fs::remove_file(audio).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "zzLilyTest0";

    fn cache(name: &str) -> AudioCache {
        let dir = std::env::temp_dir().join(format!("lily-audio-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        AudioCache {
            dir,
            downloaded: |_| false,
        }
    }

    fn stream(size: u64, asked: (Quality, bool)) -> ResolvedStream {
        ResolvedStream {
            url: "https://rr1.googlevideo.com/videoplayback?itag=251".into(),
            size,
            mime: "audio/webm".into(),
            itag: 251,
            asked: Some(asked),
            ..Default::default()
        }
    }

    const FREE: (Quality, bool) = (Quality::High, false);
    const PREMIUM: (Quality, bool) = (Quality::High, true);

    /// Hands `keep` `bytes` as a download would once whole.
    fn hand(keep: Keep, bytes: &[u8]) {
        let mut write = |file: &mut std::fs::File| std::io::Write::write_all(file, bytes);
        (keep.kept)(&mut write).unwrap();
    }

    fn kept_as(cache: &AudioCache, id: &str, bytes: &[u8], asked: (Quality, bool)) {
        hand(
            cache.keep(id, &stream(bytes.len() as u64, asked)).unwrap(),
            bytes,
        );
    }

    fn kept(cache: &AudioCache, bytes: &[u8], asked: (Quality, bool)) {
        kept_as(cache, ID, bytes, asked);
    }

    fn has(cache: &AudioCache, id: &str, (quality, premium): (Quality, bool)) -> bool {
        cache.find(id, quality, premium).is_some()
    }

    fn files(cache: &AudioCache) -> Vec<String> {
        let entries = std::fs::read_dir(&cache.dir).unwrap();
        let mut names: Vec<_> = (entries.map(|e| e.unwrap().file_name()))
            .map(|name| name.into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn plays_a_kept_song_only_as_it_was_kept() {
        let cache = cache("find");
        kept(&cache, &[1; 10], FREE);
        let found = cache.find(ID, Quality::High, false).unwrap();
        assert_eq!(found.file, Some(cache.dir.join(format!("{ID}.audio"))));
        assert_eq!((found.size, found.itag), (10, 251));
        assert_eq!(found.mime, "audio/webm");
        assert!(cache.owns(found.file.as_deref().unwrap()));
        assert!(has(&cache, ID, (Quality::Normal, false)), "a better copy");
        assert!(!has(&cache, ID, PREMIUM), "the Premium path now wanted");
        assert!(has(&cache, ID, FREE), "still kept");
        assert!(!has(&cache, &ID.to_lowercase(), FREE), "another song");
        assert!(!has(&cache, "../../x.json", FREE));

        kept(&cache, &[1; 10], PREMIUM);
        assert!(has(&cache, ID, FREE), "kept through it, either way");
        assert!(has(&cache, ID, PREMIUM));
        let _ = std::fs::remove_dir_all(&cache.dir);
    }

    #[test]
    fn a_worse_copy_neither_plays_for_nor_replaces_a_better() {
        let cache = cache("better");
        const NORMAL: (Quality, bool) = (Quality::Normal, false);
        kept(&cache, &[1; 5], NORMAL);
        assert!(!has(&cache, ID, FREE), "worse than asked");
        kept(&cache, &[1; 10], PREMIUM);
        kept(&cache, &[1; 6], NORMAL);
        kept(&cache, &[1; 7], FREE);
        let found = cache.find(ID, Quality::Low, false).unwrap();
        assert_eq!(found.size, 10, "the Premium copy stays");
        assert_eq!(files(&cache).len(), 2, "one copy");
        let _ = std::fs::remove_dir_all(&cache.dir);
    }

    #[test]
    fn a_copy_that_failed_to_play_is_refused_until_replaced() {
        let cache = cache("failed");
        // The failing stream's own keep, finishing after its failure.
        let keep = cache.keep(ID, &stream(10, PREMIUM)).unwrap();
        cache.failed_to_play(ID);
        hand(keep, &[1; 10]);
        assert!(!has(&cache, ID, FREE));
        let held = cache.holding(ID, &stream(10, FREE));
        assert_eq!(held, None, "not to Downloads");
        assert_eq!(files(&cache).len(), 2, "left for a new copy to replace");
        kept(&cache, &[1; 8], FREE);
        assert_eq!(cache.find(ID, Quality::High, false).unwrap().size, 8);
        let _ = std::fs::remove_dir_all(&cache.dir);
    }

    #[test]
    fn a_kept_song_of_the_wrong_length_is_dropped() {
        let cache = cache("length");
        kept(&cache, &[1; 10], FREE);
        let audio = cache.dir.join(format!("{ID}.audio"));
        std::fs::write(&audio, [1; 9]).unwrap();
        let writing = audio::InUse::new(&audio);
        assert!(!has(&cache, ID, FREE), "cut short");
        assert_eq!(files(&cache).len(), 2, "or a new copy, its sidecar to come");
        drop(writing);
        assert!(!has(&cache, ID, FREE));
        assert!(files(&cache).is_empty(), "both files gone");
        let _ = std::fs::remove_dir_all(&cache.dir);
    }

    #[test]
    fn keeps_only_what_can_be_trusted_and_fits() {
        let cache = cache("keep");
        let keeps = |id: &str, stream: &ResolvedStream| cache.keep(id, stream).is_some();
        assert!(keeps(ID, &stream(10, FREE)));
        assert!(!keeps(ID, &stream(0, FREE)), "no length");
        assert!(!keeps(ID, &stream(budget() + 1, FREE)), "past the budget");
        let mut other = stream(10, FREE);
        other.asked = None;
        assert!(!keeps(ID, &other), "not resolved to play");
        assert!(!keeps("C:\\music\\a.flac", &stream(10, FREE)));
        assert!(
            !cache.dir.exists(),
            "nothing written until handed the stream"
        );
    }

    #[test]
    fn never_keeps_a_song_downloaded() {
        let cache = AudioCache {
            downloaded: |_| true,
            ..cache("downloaded")
        };
        kept(&cache, &[1; 10], FREE);
        assert!(!cache.dir.exists(), "nothing written");
    }

    #[test]
    fn hands_over_only_the_same_encode_to_download() {
        let cache = cache("hand-over");
        kept(&cache, &[1; 10], FREE);
        let other = ResolvedStream {
            itag: 140,
            ..stream(10, FREE)
        };
        assert_eq!(cache.holding(ID, &other), None, "another format");
        assert_eq!(cache.holding(ID, &stream(11, FREE)), None, "another length");
        assert_eq!(cache.holding("aaaaaaaaaaa", &stream(10, FREE)), None);
        let held = cache.holding(ID, &stream(10, FREE));
        assert_eq!(held, Some(cache.dir.join(format!("{ID}.audio"))));
        std::fs::write(held.unwrap(), [1; 9]).unwrap();
        assert_eq!(cache.holding(ID, &stream(10, FREE)), None, "cut short");
        let _ = std::fs::remove_dir_all(&cache.dir);
    }

    #[test]
    fn prunes_a_song_with_its_sidecar() {
        let cache = cache("prune");
        kept(&cache, &[1; 10], FREE);
        kept_as(&cache, "aaaaaaaaaaa", &[1; 10], FREE);
        let _playing = audio::InUse::new(&cache.dir.join("aaaaaaaaaaa.audio"));
        cache.prune_to(15);
        assert!(!has(&cache, ID, FREE), "one went");
        assert_eq!(files(&cache).len(), 2, "with its sidecar");
        cache.prune_to(0);
        assert!(has(&cache, "aaaaaaaaaaa", FREE), "never one in use");
        let _ = std::fs::remove_dir_all(&cache.dir);
    }

    #[test]
    fn sweeps_away_what_a_crash_left() {
        let cache = cache("sweep");
        kept(&cache, &[1; 10], FREE);
        let write = |name: &str| std::fs::write(cache.dir.join(name), [0; 4]).unwrap();
        let left = [
            "bbbbbbbbbbb.json",
            "ccccccccccc.audio",
            "ddddddddddd.4.tmp",
            "notes.txt",
        ];
        left.into_iter().for_each(write);
        // A song started since launch, its temporary file and then its rename.
        let writing = ["fffffffffff.6.tmp", "ggggggggggg.audio"];
        writing.into_iter().for_each(write);
        let _held = ["fffffffffff", "ggggggggggg"]
            .map(|id| audio::InUse::new(&cache.dir.join(format!("{id}.audio"))));
        cache.sweep();
        let (audio, sidecar) = (format!("{ID}.audio"), format!("{ID}.json"));
        let rest = [writing[0], writing[1], "notes.txt"].map(String::from);
        assert_eq!(files(&cache), [&rest[..], &[audio, sidecar]].concat());
        for name in writing {
            let _ = std::fs::remove_file(cache.dir.join(name));
        }
        assert_eq!(cache.size(), 10, "songs only");
        write("eeeeeeeeeee.5.tmp");
        cache.clear();
        assert_eq!(cache.size(), 0);
        assert!(!has(&cache, ID, FREE));
        let still = ["eeeeeeeeeee.5.tmp", "notes.txt"];
        assert_eq!(files(&cache), still, "one still being written stays");
        let _ = std::fs::remove_dir_all(&cache.dir);
    }
}
