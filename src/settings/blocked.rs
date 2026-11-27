//! Artists whose songs play only when chosen, kept for every account on this
//! PC and never sent to YouTube.

use std::{
    collections::HashSet,
    hash::{DefaultHasher, Hasher},
};

use serde::{Deserialize, Serialize};

use crate::api::{ArtistRef, Track};

/// Saved as the list alone, oldest first; the sets are made from it, so
/// checking a song is a lookup per artist.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(from = "Vec<serde_json::Value>")]
pub struct Blocked {
    artists: Vec<ArtistRef>,
    ids: HashSet<String>,
    /// Names by `name_key`, as some rows carry no ID and others an alias's.
    names: HashSet<u64>,
}

fn folded(name: &str) -> impl Iterator<Item = char> + '_ {
    name.chars().flat_map(char::to_lowercase)
}

/// Hashed as it folds to lower case, so checking a name allocates nothing.
fn name_key(name: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    folded(name).for_each(|c| hasher.write_u32(c.into()));
    hasher.finish()
}

impl FromIterator<ArtistRef> for Blocked {
    fn from_iter<I: IntoIterator<Item = ArtistRef>>(artists: I) -> Self {
        let artists: Vec<ArtistRef> = artists.into_iter().collect();
        Self {
            ids: artists.iter().filter_map(|a| a.id.clone()).collect(),
            names: artists.iter().map(|a| name_key(&a.name)).collect(),
            artists,
        }
    }
}

impl Serialize for Blocked {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.artists.serialize(serializer)
    }
}

impl From<Vec<serde_json::Value>> for Blocked {
    fn from(stored: Vec<serde_json::Value>) -> Self {
        super::each_read(stored).collect()
    }
}

impl Blocked {
    pub fn artists(&self) -> &[ArtistRef] {
        &self.artists
    }

    /// The same artist by ID or by name.
    pub fn matches(&self, artist: &ArtistRef) -> bool {
        artist.id.as_ref().is_some_and(|id| self.ids.contains(id))
            || (self.names.contains(&name_key(&artist.name))
                // A hash alone could collide.
                && (self.artists.iter()).any(|b| folded(&b.name).eq(folded(&artist.name))))
    }

    /// Whether any of the song's artists is blocked.
    pub fn has(&self, track: &Track) -> bool {
        !self.artists.is_empty() && track.artists.iter().any(|a| self.matches(a))
    }

    /// Blocks `artist`, or unblocks it, returning whether it is now blocked.
    pub fn toggle(&mut self, artist: ArtistRef) -> bool {
        let blocking = !self.matches(&artist);
        let mut artists = std::mem::take(&mut self.artists);
        if blocking {
            artists.push(artist);
        } else {
            let unblocked: Self = [artist].into_iter().collect();
            artists.retain(|a| !unblocked.matches(a));
        }
        *self = artists.into_iter().collect();
        blocking
    }

    /// Leaves out blocked artists' songs but the one at `chosen`, returning
    /// its new index.
    pub fn leave_out(&self, tracks: &mut Vec<Track>, chosen: Option<usize>) -> Option<usize> {
        if self.artists.is_empty() {
            return chosen;
        }
        let (mut at, mut kept, mut index) = (0, 0, None);
        tracks.retain(|track| {
            let spared = Some(at) == chosen;
            let keep = spared || !self.has(track);
            if spared {
                index = Some(kept);
            }
            kept += usize::from(keep);
            at += 1;
            keep
        });
        index
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn by(id: &str, artist: Option<&str>, name: &str) -> Track {
        Track {
            id: id.into(),
            artists: vec![ArtistRef {
                id: artist.map(Into::into),
                name: name.into(),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn blocks_an_artist_by_id_or_name() {
        let mut blocked = Blocked::default();
        let tame = ArtistRef {
            id: Some("UC1".into()),
            name: "Tame Impala".into(),
        };
        assert!(blocked.toggle(tame.clone()));
        let by_name = by("a", None, "tame impala");
        let alias = by("b", Some("UC2"), "Tame Impala");
        let by_id = by("c", Some("UC1"), "Tame Impala & Friends");
        let other = by("d", Some("UC3"), "Pond");
        assert!(blocked.has(&by_name), "a row without an ID");
        assert!(blocked.has(&alias), "another ID for the same name");
        assert!(blocked.has(&by_id));
        assert!(!blocked.has(&other));
        let mut featuring = other.clone();
        featuring.artists.push(tame.clone());
        assert!(blocked.has(&featuring), "any of a song's artists");

        let mut kept = vec![by_name.clone(), other.clone(), by_name.clone()];
        let index = blocked.leave_out(&mut kept, Some(2));
        assert_eq!(
            kept.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            ["d", "a"]
        );
        assert_eq!(index, Some(1), "bar the one chosen");
        let mut kept = vec![by_name, other];
        assert_eq!(blocked.leave_out(&mut kept, None), None);
        assert_eq!(kept.len(), 1);

        assert!(
            !blocked.toggle(ArtistRef {
                id: Some("UC1".into()),
                name: "Renamed".into(),
            }),
            "unblocked by ID"
        );
        assert!(blocked.artists().is_empty() && !blocked.has(&by_id));
    }

    #[test]
    fn blocked_artists_round_trip_and_older_settings_have_none() {
        use crate::settings::Settings;
        let mut settings = Settings::default();
        settings.blocked_artists.toggle(ArtistRef {
            id: Some("UC1".into()),
            name: "Tame Impala".into(),
        });
        let json = serde_json::to_vec(&settings).unwrap();
        assert_eq!(Settings::parse(&json), settings);
        assert!(Settings::parse(b"{}").blocked_artists.artists().is_empty());
        let file = br#"{"blocked_artists": [{"id": "UC1", "name": "Tame Impala"}, {"id": 3}]}"#;
        assert_eq!(
            Settings::parse(file).blocked_artists,
            settings.blocked_artists,
            "one that won't read loses only itself"
        );
    }
}
