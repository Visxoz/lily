//! The web client gets lyrics as one block of text, while the Android app gets
//! lines with their start times when synced, so timed lyrics are asked for as it.

use serde::Deserialize;
use serde_json::{Value, json, value::RawValue};

use super::Lyrics;

const BROWSE_URL: &str = "https://music.youtube.com/youtubei/v1/browse?prettyPrint=false";
const VERSION: &str = "7.21.50";
const USER_AGENT: &str =
    "com.google.android.apps.youtube.music/7.21.50 (Linux; U; Android 14) gzip";
const LYRICS_DATA: &str =
    "/elementRenderer/newElement/type/componentType/model/timedLyricsModel/lyricsData";

#[derive(Deserialize)]
struct Response<'a> {
    /// Kept raw, so the large `frameworkUpdates` beside it is only scanned.
    #[serde(borrow)]
    contents: &'a RawValue,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LyricsData {
    timed_lyrics_data: Vec<Line>,
    #[serde(default)]
    source_message: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Line {
    lyric_line: String,
    cue_range: Option<CueRange>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CueRange {
    start_time_milliseconds: String,
    end_time_milliseconds: Option<String>,
}

/// The lyrics `browse_id` names as the Android app is given them, or
/// `None` when the answer is not in the shape expected.
pub(in crate::api) async fn timed(browse_id: &str) -> Result<Option<Lyrics>, String> {
    let body = json!({
        "context": { "client": {
            "clientName": "ANDROID_MUSIC",
            "clientVersion": VERSION,
            "androidSdkVersion": 34,
            "hl": "en",
        }},
        "browseId": browse_id,
    });
    let request = crate::http::client()
        .post(BROWSE_URL)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .header("Content-Type", "application/json")
        .body(body.to_string());
    let text = crate::http::text(request, "the Android app's request").await?;
    Ok(parse(&text))
}

fn parse(json: &str) -> Option<Lyrics> {
    let response: Response = serde_json::from_str(json).ok()?;
    let contents: Value = serde_json::from_str(response.contents.get()).ok()?;
    let data = LyricsData::deserialize(contents.pointer(LYRICS_DATA)?).ok()?;
    if data.timed_lyrics_data.is_empty() {
        return None;
    }
    // Kept only if every line has its time.
    let times = |time: fn(&CueRange) -> Option<&str>| -> Option<Vec<f64>> {
        let seconds = |line: &Line| {
            let ms: u64 = time(line.cue_range.as_ref()?)?.parse().ok()?;
            Some(ms as f64 / 1000.0)
        };
        data.timed_lyrics_data.iter().map(seconds).collect()
    };
    let starts = times(|cue| Some(cue.start_time_milliseconds.as_str()));
    let ends = starts
        .is_some()
        .then(|| times(|cue| cue.end_time_milliseconds.as_deref()))
        .flatten();
    Some(Lyrics {
        lines: data
            .timed_lyrics_data
            .into_iter()
            .map(|line| line.lyric_line)
            .collect(),
        starts: starts.unwrap_or_default(),
        ends: ends.unwrap_or_default(),
        source: data.source_message,
        ..Lyrics::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(lines: &str) -> String {
        format!(
            r#"{{"contents":{{"elementRenderer":{{"newElement":{{"type":{{"componentType":{{"model":{{"timedLyricsModel":{{"lyricsData":{{"timedLyricsData":[{lines}],"sourceMessage":"Source: Musixmatch"}}}}}}}}}}}}}}}},"frameworkUpdates":{{"big":[1,2,3]}}}}"#
        )
    }

    #[test]
    fn reads_synced_lines_with_their_starts() {
        let json = response(
            r#"{"lyricLine":"One","cueRange":{"startTimeMilliseconds":"1500","endTimeMilliseconds":"3000","metadata":{"id":"1"}}},
               {"lyricLine":"Two","cueRange":{"startTimeMilliseconds":"3000","endTimeMilliseconds":"4000"}}"#,
        );
        assert_eq!(
            parse(&json),
            Some(Lyrics {
                lines: vec!["One".into(), "Two".into()],
                starts: vec![1.5, 3.0],
                ends: vec![3.0, 4.0],
                source: "Source: Musixmatch".into(),
                ..Lyrics::default()
            })
        );
    }

    #[test]
    fn untimed_lines_have_no_starts() {
        let json = response(
            r#"{"lyricLine":"One"},{"lyricLine":"Two","cueRange":{"startTimeMilliseconds":"3000"}}"#,
        );
        let lyrics = parse(&json).unwrap();
        assert_eq!(lyrics.lines, ["One", "Two"]);
        assert!(lyrics.starts.is_empty(), "synced only when every line is");
        assert!(lyrics.ends.is_empty());
    }

    #[test]
    fn ends_are_kept_only_when_every_line_has_one() {
        let json = response(
            r#"{"lyricLine":"One","cueRange":{"startTimeMilliseconds":"1500","endTimeMilliseconds":"3000"}},
               {"lyricLine":"Two","cueRange":{"startTimeMilliseconds":"3000"}}"#,
        );
        let lyrics = parse(&json).unwrap();
        assert_eq!(lyrics.starts, [1.5, 3.0]);
        assert!(lyrics.ends.is_empty());
    }

    #[test]
    fn other_shapes_are_none() {
        assert_eq!(parse(&response("")), None, "no lines");
        assert_eq!(parse(r#"{"contents":{"sectionListRenderer":{}}}"#), None);
        assert_eq!(parse("not json"), None);
    }
}
