use std::{io, path::PathBuf};

use serde::{Deserialize, Serialize};

mod account;
mod files;
mod history;
mod searches;
mod session;

pub use account::{
    load_home, load_library, load_visitor, remove_account_data, save_home, save_library,
    save_visitor,
};
pub use files::{
    HISTORY_SAVES, SESSION_SAVES, data_dir, move_data_dir, remove_old_cookie, write_atomic,
    write_atomic_parts,
};
pub(crate) use files::{read_json, save_json};
pub use history::History;
pub use searches::Searches;
pub use session::Session;

fn settings_path() -> PathBuf {
    data_dir().join("settings.json")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WindowGeometry {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub maximized: bool,
}

/// How much of a stream to ask for. Low and Normal cap the bitrate, for
/// metered connections; High takes the best YouTube offers, which is 256
/// kbps when signed in with Premium.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Quality {
    Low,
    Normal,
    #[default]
    High,
}

impl Quality {
    pub fn index(self) -> i32 {
        self as i32
    }

    pub fn from_index(i: i32) -> Self {
        match i {
            0 => Quality::Low,
            1 => Quality::Normal,
            _ => Quality::High,
        }
    }

    /// The highest bitrate wanted, in kbps, or `None` for the best there
    /// is. YouTube's Opus streams come at about 50, 70 and 130 kbps.
    pub fn max_kbps(self) -> Option<u64> {
        match self {
            Quality::Low => Some(64),
            Quality::Normal => Some(192),
            Quality::High => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Look {
    Dark,
    Light,
    #[default]
    Black,
}

impl Look {
    pub fn index(self) -> i32 {
        self as i32
    }

    pub fn from_index(i: i32) -> Self {
        match i {
            1 => Look::Light,
            2 => Look::Black,
            _ => Look::Dark,
        }
    }
}

/// The orchid of Lily's icon.
pub const LILY: u32 = 0xd65db1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Accent {
    #[default]
    Lily,
    Red,
    Orange,
    Green,
    Teal,
    Blue,
    Purple,
    Pink,
    Artwork,
}

impl Accent {
    /// In the order the Settings dialog offers them, which is their index.
    pub const ALL: [Self; 9] = [
        Accent::Lily,
        Accent::Red,
        Accent::Orange,
        Accent::Green,
        Accent::Teal,
        Accent::Blue,
        Accent::Purple,
        Accent::Pink,
        Accent::Artwork,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Accent::Lily => "Lily",
            Accent::Red => "Red",
            Accent::Orange => "Orange",
            Accent::Green => "Green",
            Accent::Teal => "Teal",
            Accent::Blue => "Blue",
            Accent::Purple => "Purple",
            Accent::Pink => "Pink",
            Accent::Artwork => "From the artwork",
        }
    }

    /// The colour as `0xRRGGBB`, or `None` for the artwork's, which changes
    /// with the song.
    pub fn rgb(self) -> Option<u32> {
        match self {
            Accent::Lily => Some(LILY),
            Accent::Red => Some(0xff0033),
            Accent::Orange => Some(0xff6a13),
            Accent::Green => Some(0x1db954),
            Accent::Teal => Some(0x00a89d),
            Accent::Blue => Some(0x3d7bff),
            Accent::Purple => Some(0x9b5cff),
            Accent::Pink => Some(0xf0458f),
            Accent::Artwork => None,
        }
    }

    pub fn index(self) -> i32 {
        self as i32
    }

    pub fn from_index(i: i32) -> Self {
        usize::try_from(i)
            .ok()
            .and_then(|i| Self::ALL.get(i).copied())
            .unwrap_or_default()
    }
}

/// The crossfade lengths Settings offers, in seconds.
pub const CROSSFADE_SECS: [f32; 9] = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 8.0, 10.0, 12.0];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub volume: f32,
    pub muted: bool,
    #[serde(deserialize_with = "normalise_level")]
    pub normalise: crate::audio::Normalise,
    pub discord: bool,
    pub autoplay: bool,
    /// Report songs played to the signed-in account's YouTube history.
    pub youtube_history: bool,
    pub rating_buttons: bool,
    pub shuffle: bool,
    /// Shuffle with recommended tracks mixed in; a kind of shuffle, so it
    /// goes off with it.
    pub smart_shuffle: bool,
    pub repeat: i32,
    pub sidebar_collapsed: bool,
    pub quality: Quality,
    /// Older versions' form of `quality`'s High, kept in step for them; only
    /// `load` reads it here.
    pub premium_audio: bool,
    /// The signed-in account has been served Premium formats, so songs
    /// without them, such as music videos, never mark it as not Premium.
    pub premium_account: bool,
    /// The signed-in account was taken as without Premium, so songs start
    /// on the free stream at once while a background check confirms it.
    pub no_premium: bool,
    pub crossfade: bool,
    pub crossfade_secs: f32,
    /// Blends tracks by their sound, in place of the crossfade.
    pub automix: bool,
    /// Pauses as the device playing is unplugged, rather than carrying on
    /// through the default.
    pub pause_on_unplug: bool,
    /// Playback speed, the pitch moving with it.
    pub speed: f32,
    /// Closing the window leaves the app playing in the tray.
    pub close_to_tray: bool,
    pub check_updates: bool,
    pub equaliser: bool,
    /// Each band's gain in dB, kept while the equaliser is off.
    #[serde(deserialize_with = "equaliser_gains")]
    pub equaliser_gains: crate::audio::eq::Gains,
    pub crossfeed: bool,
    pub mono: bool,
    /// From -1, the left side alone, to 1, the right.
    pub balance: f32,
    pub look: Look,
    pub accent: Accent,
    /// The full player plays the song's Spotify Canvas or its album's Apple
    /// Music motion cover, where there is one.
    pub animated_art: bool,
    pub window: Option<WindowGeometry>,
    /// Where the mini player was last put, in physical pixels.
    pub mini_position: Option<(i32, i32)>,
    /// Where the user's own music is, shown under Local files.
    pub local_folders: Vec<PathBuf>,
    /// The output device chosen, or `None` to follow Windows' default.
    pub output_device: Option<OutputDevice>,
    /// The signed-in account's picture, shown at once on the next start.
    pub account_photo: Option<String>,
    /// Playlist IDs pinned to the top of the sidebar, first pinned first.
    pub pinned: Vec<String>,
}

/// Older versions kept `normalise` as on or off, which is Normal or Off.
fn normalise_level<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<crate::audio::Normalise, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Stored {
        Old(bool),
        Level(crate::audio::Normalise),
    }
    Ok(match Stored::deserialize(d)? {
        Stored::Old(true) => crate::audio::Normalise::Normal,
        Stored::Old(false) => crate::audio::Normalise::Off,
        Stored::Level(level) => level,
    })
}

/// Older versions had six bands, which are carried over to the ten by
/// interpolating in log-frequency between their centres, to the whole dB
/// the sliders step in, so the old presets land on the new ones.
fn equaliser_gains<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<crate::audio::eq::Gains, D::Error> {
    use crate::audio::eq::BANDS;
    const OLD: [f64; 6] = [60.0, 150.0, 400.0, 1_000.0, 2_400.0, 15_000.0];
    let gains = Vec::<f32>::deserialize(d)?;
    if let Ok(gains) = gains.as_slice().try_into() {
        return Ok(gains);
    }
    let old: [f32; 6] = gains.as_slice().try_into().map_err(|_| {
        serde::de::Error::invalid_length(gains.len(), &"ten gains, or six from older versions")
    })?;
    Ok(BANDS.map(|hz| {
        let db = match OLD.partition_point(|&c| c <= hz) {
            0 => old[0],
            6 => old[5],
            i => {
                let t = ((hz / OLD[i - 1]).ln() / (OLD[i] / OLD[i - 1]).ln()) as f32;
                old[i - 1] + (old[i] - old[i - 1]) * t
            }
        };
        // Adding 0 turns a rounded -0 into 0.
        db.round() + 0.0
    }))
}

/// An output device by cpal's ID, with its name for while it is unplugged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputDevice {
    pub id: String,
    pub name: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            volume: 0.8,
            muted: false,
            normalise: crate::audio::Normalise::Normal,
            discord: true,
            autoplay: true,
            youtube_history: true,
            rating_buttons: true,
            shuffle: false,
            smart_shuffle: false,
            repeat: 0,
            sidebar_collapsed: false,
            quality: Quality::High,
            premium_audio: true,
            premium_account: false,
            no_premium: false,
            crossfade: false,
            // Long enough to blend two songs, short enough to keep most of an ending.
            crossfade_secs: 6.0,
            automix: false,
            pause_on_unplug: true,
            speed: 1.0,
            close_to_tray: true,
            check_updates: true,
            equaliser: false,
            equaliser_gains: Default::default(),
            crossfeed: false,
            mono: false,
            balance: 0.0,
            look: Look::Black,
            accent: Accent::Lily,
            animated_art: false,
            window: None,
            mini_position: None,
            local_folders: Vec::new(),
            output_device: None,
            account_photo: None,
            pinned: Vec::new(),
        }
    }
}

impl Settings {
    pub fn load() -> Self {
        let mut settings = std::fs::read(settings_path())
            .map(|json| Self::parse(&json))
            .unwrap_or_default();
        // Premium audio turned off by an older version means Normal.
        if !settings.premium_audio && settings.quality == Quality::High {
            settings.quality = Quality::Normal;
        }
        // The app divides by it, so it must agree with the player's.
        settings.speed = settings
            .speed
            .clamp(crate::audio::MIN_SPEED, crate::audio::MAX_SPEED);
        settings.balance = settings.balance.clamp(-1.0, 1.0);
        settings.crossfade_secs = settings
            .crossfade_secs
            .clamp(CROSSFADE_SECS[0], CROSSFADE_SECS[CROSSFADE_SECS.len() - 1]);
        settings
    }

    /// Reads each setting on its own, so one this version can't read (a hand
    /// edit, or a choice added by a newer version) goes back to its default
    /// alone rather than taking an opt-out such as `youtube_history` with it.
    fn parse(json: &[u8]) -> Self {
        let file = match serde_json::from_slice(json) {
            Ok(serde_json::Value::Object(file)) => file,
            _ => {
                log::warn!("settings.json could not be read; using the defaults");
                return Self::default();
            }
        };
        let mut kept = serde_json::Map::new();
        for (key, value) in file {
            kept.insert(key.clone(), value);
            if serde_json::from_value::<Self>(kept.clone().into()).is_err() {
                log::warn!("setting {key} could not be read; using its default");
                kept.remove(&key);
            }
        }
        serde_json::from_value(kept.into()).unwrap_or_default()
    }

    pub fn set_quality(&mut self, quality: Quality) {
        self.quality = quality;
        self.premium_audio = quality == Quality::High;
    }

    pub fn equaliser(&self) -> Option<crate::audio::eq::Gains> {
        self.equaliser.then_some(self.equaliser_gains)
    }

    /// How a track moves into the next, as set; the caller keeps an album in
    /// order gapless.
    pub fn transition(&self) -> crate::audio::Transition {
        use crate::audio::Transition;
        if self.automix {
            Transition::Automix
        } else if self.crossfade {
            Transition::Crossfade(f64::from(self.crossfade_secs))
        } else {
            Transition::Gapless
        }
    }

    pub fn set_shuffle(&mut self, on: bool) {
        self.shuffle = on;
        if !on {
            self.smart_shuffle = false;
        }
    }

    pub fn save(&self) {
        // Pretty, as people open this file to look at it.
        let result = std::fs::create_dir_all(data_dir()).and_then(|()| {
            let json = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
            write_atomic(&settings_path(), &json)
        });
        if let Err(e) = result {
            log::warn!("could not save settings: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unreadable_setting_resets_only_itself() {
        let settings = Settings::parse(
            br#"{"youtube_history": false, "accent": "Blue", "equaliser_gains": [1], "volume": 0.5}"#,
        );
        assert!(!settings.youtube_history, "the opt-out is kept");
        assert_eq!(settings.volume, 0.5);
        assert_eq!(settings.accent, Accent::Lily);
        assert_eq!(settings.equaliser_gains, [0.0; 10]);
        assert_eq!(Settings::parse(b"not json"), Settings::default());
    }

    #[test]
    fn six_old_equaliser_bands_carry_over_to_ten() {
        let old = Settings::parse(br#"{"equaliser_gains": [1, 2, 3, 4, 5, 6]}"#);
        let gains = old.equaliser_gains;
        // 31 Hz holds the old lowest, at 60 Hz; 62 Hz is just past it.
        // 250 Hz is about halfway, in octaves, from 150 to 400 Hz.
        assert_eq!(gains, [1.0, 1.0, 2.0, 3.0, 3.0, 4.0, 5.0, 5.0, 6.0, 6.0]);
        let bass = Settings::parse(br#"{"equaliser_gains": [6, 4, 1, 0, 0, 0]}"#);
        assert_eq!(bass.equaliser_gains, crate::audio::eq::PRESETS[1].1);
        let ten = Settings::parse(br#"{"equaliser_gains": [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]}"#);
        assert_eq!(ten.equaliser_gains[9], 10.0);
    }

    #[test]
    fn fills_missing_fields_with_defaults() {
        let settings: Settings = serde_json::from_str(r#"{"volume":0.5}"#).unwrap();
        assert_eq!(settings.volume, 0.5);
        assert_eq!(settings.normalise, crate::audio::Normalise::Normal);
        assert!(!settings.crossfade, "off until chosen");
        assert!(!settings.automix, "off until chosen");
        assert!(
            CROSSFADE_SECS.contains(&settings.crossfade_secs),
            "a length Settings offers"
        );
        assert!(settings.pause_on_unplug);
        assert_eq!(settings.quality, Quality::High);
    }

    #[test]
    fn reads_normalise_from_older_versions() {
        use crate::audio::Normalise;
        let level = |json: &str| Settings::parse(json.as_bytes()).normalise;
        assert_eq!(level(r#"{"normalise":false}"#), Normalise::Off);
        assert_eq!(level(r#"{"normalise":true}"#), Normalise::Normal);
        let quiet = Settings {
            normalise: Normalise::Quiet,
            ..Default::default()
        };
        let json = serde_json::to_string(&quiet).unwrap();
        assert_eq!(level(&json), Normalise::Quiet, "and its own");
    }

    #[test]
    fn quality_and_premium_audio_agree() {
        let mut settings = Settings::default();
        settings.set_quality(Quality::Low);
        assert!(!settings.premium_audio);
        settings.set_quality(Quality::High);
        assert!(settings.premium_audio);
        assert_eq!(
            Quality::from_index(Quality::Normal.index()),
            Quality::Normal
        );
        assert_eq!(Quality::High.max_kbps(), None);
    }

    #[test]
    fn looks_and_accents_round_trip_their_index() {
        for (i, accent) in Accent::ALL.into_iter().enumerate() {
            assert_eq!(accent.index(), i as i32, "ALL is in declaration order");
            assert_eq!(Accent::from_index(accent.index()), accent);
        }
        assert_eq!(Accent::from_index(-1), Accent::Lily);
        assert_eq!(Accent::from_index(99), Accent::Lily);
        assert_eq!(Accent::Artwork.rgb(), None);
        for look in [Look::Dark, Look::Light, Look::Black] {
            assert_eq!(Look::from_index(look.index()), look);
        }
        let settings: Settings =
            serde_json::from_str(r#"{"look":"black","accent":"artwork"}"#).unwrap();
        assert_eq!(
            (settings.look, settings.accent),
            (Look::Black, Accent::Artwork)
        );
    }
}
