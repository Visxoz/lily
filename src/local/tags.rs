//! Reads a file's tags, length and cover art through Symphonia.

use std::{fs::File, io, path::Path};

use symphonia::core::{
    errors::Error,
    formats::{FormatOptions, FormatReader, TrackType, probe::Hint},
    io::MediaSourceStream,
    meta::{MetadataRevision, StandardTag, StandardVisualKey, Tag, Visual},
};

use super::Song;
use crate::api::{ArtistRef, Track};

/// What the decoders built in play.
const EXTENSIONS: [&str; 9] = [
    "flac", "m4a", "mka", "mp3", "oga", "ogg", "opus", "wav", "webm",
];

pub(super) fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

/// The file's reader, or `None` should it not be audio. A file that can't be
/// read just now, as on a share that dropped, is an error.
fn probe(path: &Path) -> io::Result<Option<Box<dyn FormatReader>>> {
    let file = File::open(path)?;
    let mut hint = Hint::new();
    if let Some(extension) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(extension);
    }
    let stream = MediaSourceStream::new(Box::new(file), Default::default());
    let probe = symphonia::default::get_probe();
    match probe.probe(&hint, stream, FormatOptions::default(), Default::default()) {
        Ok(format) => Ok(Some(format)),
        // Running out is what a file that isn't audio does.
        Err(Error::IoError(e)) if e.kind() != io::ErrorKind::UnexpectedEof => Err(e),
        Err(e) => {
            log::debug!("unreadable audio file: {e}");
            Ok(None)
        }
    }
}

/// Every revision of the file's metadata, oldest first: tags found ahead of
/// the container, such as an MP3's ID3, then the container's own.
fn revisions(format: &mut dyn FormatReader, mut each: impl FnMut(&MetadataRevision)) {
    let mut metadata = format.metadata();
    loop {
        if let Some(revision) = metadata.current() {
            each(revision);
        }
        if metadata.pop().is_none() {
            break;
        }
    }
}

fn tags(revision: &MetadataRevision) -> impl Iterator<Item = &Tag> {
    let per_track = revision.per_track.iter().flat_map(|t| &t.metadata.tags);
    revision.media.tags.iter().chain(per_track)
}

fn visuals(revision: &MetadataRevision) -> impl Iterator<Item = &Visual> {
    let per_track = revision.per_track.iter().flat_map(|t| &t.metadata.visuals);
    revision.media.visuals.iter().chain(per_track)
}

#[derive(Default)]
pub(super) struct Tags {
    title: Option<String>,
    artists: Vec<String>,
    album_artist: Option<String>,
    album: Option<String>,
    track_nr: Option<u16>,
    disc: Option<u16>,
}

impl Tags {
    /// Keeps the first of each, as later revisions repeat earlier ones.
    fn add(&mut self, tag: &Tag) {
        match &tag.std {
            Some(StandardTag::TrackTitle(s)) => {
                self.title.get_or_insert_with(|| s.trim().to_owned());
            }
            Some(StandardTag::Artist(s)) => {
                let name = s.trim();
                if !name.is_empty() && !self.artists.iter().any(|a| a == name) {
                    self.artists.push(name.to_owned());
                }
            }
            Some(StandardTag::AlbumArtist(s)) => {
                self.album_artist.get_or_insert_with(|| s.trim().to_owned());
            }
            Some(StandardTag::Album(s)) => {
                self.album.get_or_insert_with(|| s.trim().to_owned());
            }
            Some(StandardTag::TrackNumber(n)) => {
                self.track_nr = self.track_nr.or(u16::try_from(*n).ok());
            }
            Some(StandardTag::DiscNumber(n)) => {
                self.disc = self.disc.or(u16::try_from(*n).ok());
            }
            _ => {}
        }
    }
}

/// The file as a song. Its cover is only noted here, and read as it shows.
pub(super) fn read_song(path: &Path) -> io::Result<Option<Song>> {
    let Some(mut format) = probe(path)? else {
        return Ok(None);
    };
    // Matroska gives the length for the whole file only.
    let track_time = format
        .default_track(TrackType::Audio)
        .and_then(|t| t.time_base?.calc_duration(t.duration?));
    let media = format.media_info();
    let time = track_time.or_else(|| media.time_base?.calc_duration(media.duration?));
    let mut found = Tags::default();
    let mut cover = false;
    revisions(format.as_mut(), |revision| {
        tags(revision).for_each(|t| found.add(t));
        cover |= visuals(revision).next().is_some();
    });
    Ok(song(path, found, cover, time.map(|t| t.as_secs_f64())))
}

/// The song `found` tells of: its path is its ID, and an untagged file goes
/// by its name.
pub(super) fn song(path: &Path, found: Tags, cover: bool, seconds: Option<f64>) -> Option<Song> {
    let id = path.to_str()?.to_owned();
    let present = |s: Option<String>| s.filter(|s| !s.is_empty());
    let title =
        present(found.title).or_else(|| Some(path.file_stem()?.to_string_lossy().into_owned()))?;
    let album_artist = present(found.album_artist);
    let mut artists = found.artists;
    if artists.is_empty() {
        artists.extend(album_artist.clone());
    }
    let track = Track {
        title,
        artists: artists
            .into_iter()
            .map(|name| ArtistRef { id: None, name })
            .collect(),
        // Named for itself, so tracks of one album follow on gaplessly.
        album: present(found.album).map(|name| (name.clone(), name)),
        duration: seconds.map(|s| s.round() as u32),
        thumb: None,
        track_nr: found.track_nr,
        id,
        video: false,
        set_video_id: None,
    };
    Some(Song {
        by: album_artist.unwrap_or_else(|| track.artist_names()),
        disc: found.disc.unwrap_or(0),
        cover,
        track,
    })
}

/// A picture, or the front cover inside an audio file. Blocking.
pub fn artwork(path: &Path) -> Option<Vec<u8>> {
    if !is_audio(path) {
        return std::fs::read(path).ok();
    }
    let mut format = probe(path).ok().flatten()?;
    let mut cover: Option<Vec<u8>> = None;
    let mut front = false;
    revisions(format.as_mut(), |revision| {
        for visual in visuals(revision) {
            let is_front = visual.usage == Some(StandardVisualKey::FrontCover);
            if cover.is_none() || (is_front && !front) {
                cover = Some(visual.data.to_vec());
                front = is_front;
            }
        }
    });
    cover
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knows_audio_by_its_extension() {
        assert!(is_audio(Path::new(r"C:\Music\a.FLAC")));
        assert!(is_audio(Path::new(r"C:\Music\b.mp3")));
        assert!(!is_audio(Path::new(r"C:\Music\cover.jpg")));
        assert!(!is_audio(Path::new(r"C:\Music\notes")));
    }
}
