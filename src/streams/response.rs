//! YouTube's player response, the parts of it read, and the choice of audio
//! format from it, which the Premium path shares.

use std::sync::Arc;

use serde::Deserialize;

use super::ResolvedStream;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PlayerResponse {
    playability_status: PlayabilityStatus,
    streaming_data: Option<StreamingData>,
    player_config: Option<PlayerConfig>,
    /// For the play's report to reuse, so the answer is parsed once.
    pub playback_tracking: Option<crate::api::Tracking>,
}

#[derive(Deserialize)]
struct PlayabilityStatus {
    status: String,
    reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StreamingData {
    #[serde(default)]
    adaptive_formats: Vec<Format>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlayerConfig {
    audio_config: Option<AudioConfig>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AudioConfig {
    loudness_db: Option<f32>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Format {
    pub itag: u32,
    pub url: Option<String>,
    /// `s`, `sp` and `url`, when the URL is scrambled.
    pub signature_cipher: Option<String>,
    mime_type: String,
    pub bitrate: u64,
    content_length: Option<String>,
    approx_duration_ms: Option<String>,
    loudness_db: Option<f32>,
    #[serde(default)]
    is_drc: bool,
    audio_track: Option<AudioTrack>,
    audio_channels: Option<u32>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct AudioTrack {
    #[serde(default)]
    audio_is_default: bool,
}

impl Format {
    fn is_opus(&self) -> bool {
        self.mime_type.starts_with("audio/webm") && self.mime_type.contains("opus")
    }

    /// AAC-LC only: Symphonia decodes no other AAC, so HE-AAC (`mp4a.40.5`,
    /// itag 139, the only AAC under the Low cap) fails or plays muffled. The
    /// quotes keep out `mp4a.40.29`.
    fn is_aac(&self) -> bool {
        self.mime_type.starts_with("audio/mp4") && self.mime_type.contains("\"mp4a.40.2\"")
    }

    fn has_url(&self) -> bool {
        self.url.is_some() || self.signature_cipher.is_some()
    }

    /// Not a dynamic-range compressed variant, dubbed track or surround
    /// mix, which the decoders can't play.
    fn plain(&self) -> bool {
        !self.is_drc
            && self.audio_channels.is_none_or(|c| c <= 2)
            && self.audio_track.as_ref().is_none_or(|t| t.audio_is_default)
    }
}

impl PlayerResponse {
    pub fn audio_formats(&self) -> &[Format] {
        self.streaming_data
            .as_ref()
            .map_or(&[], |s| s.adaptive_formats.as_slice())
    }

    fn loudness_db(&self) -> Option<f32> {
        self.player_config
            .as_ref()
            .and_then(|c| c.audio_config.as_ref())
            .and_then(|a| a.loudness_db)
    }

    pub fn stream(
        &self,
        format: &Format,
        url: String,
        user_agent: &str,
        premium: bool,
    ) -> ResolvedStream {
        let (mime, codec) = if format.is_opus() {
            ("audio/webm", "Opus")
        } else {
            ("audio/mp4", "AAC")
        };
        ResolvedStream {
            url,
            size: format
                .content_length
                .as_deref()
                .and_then(|n| n.parse().ok())
                .unwrap_or(0),
            mime: mime.into(),
            headers: vec![("User-Agent".into(), user_agent.into())],
            codec: codec.into(),
            bitrate_kbps: format.bitrate as f64 / 1000.0,
            duration: format
                .approx_duration_ms
                .as_deref()
                .and_then(|ms| ms.parse::<f64>().ok())
                .map(|ms| ms / 1000.0),
            loudness_db: format.loudness_db.or_else(|| self.loudness_db()),
            premium,
            itag: format.itag,
            file: None,
            client: None,
            head: Arc::default(),
            asked: None,
        }
    }
}

/// Parses a player response, failing with YouTube's reason if unplayable.
pub(crate) fn parse_response(json: &str) -> Result<PlayerResponse, String> {
    let response: PlayerResponse =
        serde_json::from_str(json).map_err(|e| format!("unexpected player response: {e}"))?;
    if response.playability_status.status != "OK" {
        return Err(response
            .playability_status
            .reason
            .clone()
            .unwrap_or_else(|| format!("not playable ({})", response.playability_status.status)));
    }
    Ok(response)
}

/// The best plain-URL stream of a player response, or with `itag`, that
/// one. A client that answers with scrambled URLs only counts as having none.
pub(super) fn parse(
    json: &str,
    user_agent: &str,
    max_kbps: Option<u64>,
    itag: Option<u32>,
) -> Result<ResolvedStream, String> {
    let response = parse_response(json)?;
    let formats = response.audio_formats();
    let format = match itag {
        Some(itag) => formats
            .iter()
            .find(|f| f.itag == itag && f.url.is_some() && f.plain()),
        None => best_audio(formats, max_kbps),
    }
    .ok_or("no playable audio stream")?;
    let url = format.url.clone().ok_or("no playable audio stream")?;
    Ok(response.stream(format, url, user_agent, false))
}

/// The highest-bitrate plain Opus stream, else the best AAC. With
/// `max_kbps`, the best within it, or the lowest there is when none fits.
pub(crate) fn best_audio(formats: &[Format], max_kbps: Option<u64>) -> Option<&Format> {
    let candidates = |codec: fn(&Format) -> bool| {
        formats
            .iter()
            .filter(move |f| codec(f) && f.has_url() && f.plain())
    };
    let pick = |codec: fn(&Format) -> bool| {
        candidates(codec)
            .filter(|f| max_kbps.is_none_or(|kbps| f.bitrate <= kbps * 1000))
            .max_by_key(|f| (f.bitrate, f.itag))
            .or_else(|| candidates(codec).min_by_key(|f| f.bitrate))
    };
    pick(Format::is_opus).or_else(|| pick(Format::is_aac))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(formats: &str) -> String {
        format!(
            r#"{{"playabilityStatus":{{"status":"OK"}},
                "streamingData":{{"adaptiveFormats":[{formats}]}},
                "playerConfig":{{"audioConfig":{{"loudnessDb":-2.5}}}}}}"#
        )
    }

    const AAC: &str = r#"{"itag":140,"url":"https://a/140","mimeType":"audio/mp4; codecs=\"mp4a.40.2\"",
        "bitrate":137064,"contentLength":"5983881","approxDurationMs":"369626"}"#;
    const OPUS: &str = r#"{"itag":251,"url":"https://a/251","mimeType":"audio/webm; codecs=\"opus\"",
        "bitrate":146460,"contentLength":"6214234","approxDurationMs":"369641","loudnessDb":4.5}"#;
    const OPUS_DRC: &str = r#"{"itag":251,"url":"https://a/drc","mimeType":"audio/webm; codecs=\"opus\"",
        "bitrate":150000,"isDrc":true}"#;
    const VIDEO: &str = r#"{"itag":248,"url":"https://a/248","mimeType":"video/webm; codecs=\"vp9\"",
        "bitrate":2000000}"#;
    const PREMIUM: &str = r#"{"itag":774,"signatureCipher":"s=abc&sp=sig&url=https%3A%2F%2Fa%2F774",
        "mimeType":"audio/webm; codecs=\"opus\"","bitrate":260000}"#;

    const UA: &str = "test";
    const OPUS_LOW: &str = r#"{"itag":249,"url":"https://a/249","mimeType":"audio/webm; codecs=\"opus\"",
        "bitrate":52000}"#;

    #[test]
    fn prefers_opus() {
        let stream = parse(
            &response(&[AAC, OPUS, OPUS_DRC, VIDEO].join(",")),
            UA,
            None,
            None,
        )
        .unwrap();
        assert_eq!(stream.url, "https://a/251");
        assert_eq!(stream.mime, "audio/webm");
        assert_eq!(stream.size, 6214234);
        assert_eq!(stream.loudness_db, Some(4.5));
        assert_eq!(stream.duration, Some(369.641));
        assert!(!stream.premium);
        assert_eq!(stream.headers[0].1, UA, "downloads as the same client");
    }

    #[test]
    fn caps_the_bitrate_for_lower_qualities() {
        let all = response(&[AAC, OPUS, OPUS_LOW].join(","));
        assert_eq!(
            parse(&all, UA, Some(64), None).unwrap().url,
            "https://a/249"
        );
        assert_eq!(
            parse(&all, UA, Some(192), None).unwrap().url,
            "https://a/251"
        );
        assert_eq!(
            parse(&response(&[AAC, OPUS].join(",")), UA, Some(64), None)
                .unwrap()
                .url,
            "https://a/251",
            "nothing within the cap, so the lowest Opus"
        );
    }

    #[test]
    fn skips_aac_and_surround_the_decoders_refuse() {
        const HE_AAC: &str = r#"{"itag":139,"url":"https://a/139","mimeType":"audio/mp4; codecs=\"mp4a.40.5\"",
            "bitrate":50000}"#;
        const SURROUND: &str = r#"{"itag":258,"url":"https://a/258","mimeType":"audio/mp4; codecs=\"mp4a.40.2\"",
            "bitrate":384000,"audioChannels":6}"#;
        let all = response(&[HE_AAC, AAC, SURROUND].join(","));
        assert_eq!(
            parse(&all, UA, Some(64), None).unwrap().url,
            "https://a/140"
        );
        assert_eq!(parse(&all, UA, None, None).unwrap().url, "https://a/140");
    }

    #[test]
    fn falls_back_to_aac() {
        let stream = parse(&response(&[AAC, VIDEO].join(",")), UA, None, None).unwrap();
        assert_eq!(stream.url, "https://a/140");
        assert_eq!(stream.codec, "AAC");
        assert_eq!(
            stream.loudness_db,
            Some(-2.5),
            "uses the player-wide loudness"
        );
    }

    #[test]
    fn scrambled_urls_count_as_none() {
        assert_eq!(
            parse(&response(PREMIUM), UA, None, None).unwrap_err(),
            "no playable audio stream"
        );
    }

    #[test]
    fn picks_scrambled_premium_formats() {
        let response = parse_response(&response(&[OPUS, PREMIUM].join(","))).unwrap();
        let best = best_audio(response.audio_formats(), None).unwrap();
        assert_eq!(best.itag, 774);
        assert!(best.signature_cipher.is_some());
    }

    #[test]
    fn carries_on_with_the_same_format_only() {
        let all = response(&[AAC, OPUS, OPUS_LOW].join(","));
        let stream = parse(&all, UA, None, Some(249)).unwrap();
        assert_eq!(stream.url, "https://a/249", "not the best");
        assert!(parse(&all, UA, None, Some(250)).is_err(), "not offered");
        assert!(
            parse(&response(PREMIUM), UA, None, Some(774)).is_err(),
            "scrambled"
        );
        let drc_first = response(&[OPUS_DRC, OPUS].join(","));
        assert_eq!(
            parse(&drc_first, UA, None, Some(251)).unwrap().url,
            "https://a/251",
            "not the compressed variant"
        );
    }

    #[test]
    fn reports_unplayable_reason() {
        let json = r#"{"playabilityStatus":{"status":"LOGIN_REQUIRED","reason":"Sign in to confirm your age"}}"#;
        assert_eq!(
            parse(json, UA, None, None).unwrap_err(),
            "Sign in to confirm your age"
        );
        assert_eq!(
            parse(&response(VIDEO), UA, None, None).unwrap_err(),
            "no playable audio stream"
        );
    }
}
