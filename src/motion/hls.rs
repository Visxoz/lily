//! The little of Apple's HLS playlists a motion cover needs: a master's
//! variants, and the one file each variant's segments are byte ranges of,
//! which is the whole clip.

use std::cmp::Reverse;

use url::Url;

/// Sharp enough for the full player, small enough for a few MB a clip.
const BANDWIDTH_CAP: u32 = 3_000_000;

struct Variant<'a> {
    uri: &'a str,
    bandwidth: u32,
    pixels: u32,
}

/// The variant to download, resolved against the master's URL: the H.264 one
/// with the most pixels under the cap, then the smaller file, or failing any,
/// the smallest. Windows decodes HEVC only with a Store extension most
/// machines lack, so it is never taken.
pub fn variant(master: &str, base: &Url) -> Option<Url> {
    let all = variants(master);
    let pick = all
        .iter()
        .filter(|v| v.bandwidth <= BANDWIDTH_CAP)
        .max_by_key(|v| (v.pixels, Reverse(v.bandwidth)))
        .or_else(|| all.iter().min_by_key(|v| v.bandwidth))?;
    base.join(pick.uri).ok()
}

/// The H.264 variants; I-frame playlists are other tags, so never among them.
fn variants(master: &str) -> Vec<Variant<'_>> {
    let mut variants = Vec::new();
    let mut pending = None;
    for line in lines(master) {
        if let Some(list) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            pending = Some(list);
        } else if !line.starts_with('#')
            && let Some(list) = pending.take()
            && attribute(list, "CODECS")
                .is_some_and(|c| c.starts_with("avc1") || c.starts_with("avc3"))
            && let Some(bandwidth) = attribute(list, "BANDWIDTH").and_then(|b| b.parse().ok())
        {
            // Narrow types, so a hostile playlist's numbers can't overflow the product.
            let pixels = attribute(list, "RESOLUTION")
                .and_then(|r| r.split_once('x'))
                .and_then(|(w, h)| {
                    Some(u32::from(w.parse::<u16>().ok()?) * u32::from(h.parse::<u16>().ok()?))
                })
                .unwrap_or(0);
            variants.push(Variant {
                uri: line,
                bandwidth,
                pixels,
            });
        }
    }
    variants
}

/// The file the map and every segment name, resolved against the variant's
/// URL; None when they name several, as only one file is downloaded.
pub fn media_file(variant: &str, base: &Url) -> Option<Url> {
    let mut file = None;
    for line in lines(variant) {
        let uri = match line.strip_prefix("#EXT-X-MAP:") {
            Some(list) => attribute(list, "URI")?,
            None if line.starts_with('#') => continue,
            None => line,
        };
        if file.is_some_and(|file| file != uri) {
            return None;
        }
        file = Some(uri);
    }
    base.join(file?).ok()
}

/// Blank lines dropped, as they name nothing.
fn lines(text: &str) -> impl Iterator<Item = &str> {
    text.split(['\n', '\r']).filter(|line| !line.is_empty())
}

/// From `KEY=value,KEY="quoted, value"`, split only at commas outside quotes,
/// the quotes dropped.
fn attribute<'a>(list: &'a str, key: &str) -> Option<&'a str> {
    let mut quoted = false;
    let split = move |c: char| {
        quoted ^= c == '"';
        c == ',' && !quoted
    };
    list.split(split).find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        let unquoted = value.strip_prefix('"').and_then(|v| v.strip_suffix('"'));
        (name == key).then(|| unquoted.unwrap_or(value))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    const MASTER_URL: &str =
        "https://mvod.itunes.apple.com/itunes-assets/HLSMusic112/v4/14/P524581047_default.m3u8";

    /// Shaped as Apple's: I-frame lines, an average bandwidth before the peak
    /// one, and pairs at one resolution.
    const MASTER: &str = r#"#EXTM3U
#EXT-X-VERSION:7
#EXT-X-INDEPENDENT-SEGMENTS


#EXT-X-I-FRAME-STREAM-INF:AVERAGE-BANDWIDTH=692796,_AVG-BANDWIDTH=692796,BANDWIDTH=710556,VIDEO-RANGE=SDR,CODECS="avc1.64001f",RESOLUTION=2048x2732,URI="https://mvod.itunes.apple.com/a/trickPlay.iframe.m3u8"
#EXT-X-STREAM-INF:AVERAGE-BANDWIDTH=2140560,_AVG-BANDWIDTH=2140560,BANDWIDTH=2279733,VIDEO-RANGE=SDR,CLOSED-CAPTIONS=NONE,CODECS="avc1.64001f",FRAME-RATE=24.000,RESOLUTION=664x886,STABLE-VARIANT-ID="4368a1c1"
https://mvod.itunes.apple.com/a/avc_664x886.m3u8
#EXT-X-STREAM-INF:AVERAGE-BANDWIDTH=2800000,_AVG-BANDWIDTH=2800000,BANDWIDTH=2999000,VIDEO-RANGE=SDR,CLOSED-CAPTIONS=NONE,CODECS="avc1.640028,mp4a.40.2",FRAME-RATE=24.000,RESOLUTION=1078x1438,STABLE-VARIANT-ID="a1"
https://mvod.itunes.apple.com/a/avc_1078x1438.m3u8
#EXT-X-STREAM-INF:AVERAGE-BANDWIDTH=2700000,_AVG-BANDWIDTH=2700000,BANDWIDTH=2844954,VIDEO-RANGE=SDR,CLOSED-CAPTIONS=NONE,CODECS="hvc1.2.20000000.L123.B0",FRAME-RATE=24.000,RESOLUTION=1078x1438,STABLE-VARIANT-ID="h2"
https://mvod.itunes.apple.com/a/hvc_1078x1438_more.m3u8
#EXT-X-STREAM-INF:AVERAGE-BANDWIDTH=2600000,_AVG-BANDWIDTH=2600000,BANDWIDTH=2829518,VIDEO-RANGE=SDR,CLOSED-CAPTIONS=NONE,CODECS="hvc1.2.20000000.L123.B0",FRAME-RATE=24.000,RESOLUTION=1078x1438,STABLE-VARIANT-ID="h1"
https://mvod.itunes.apple.com/a/hvc_1078x1438.m3u8
#EXT-X-STREAM-INF:AVERAGE-BANDWIDTH=2900000,_AVG-BANDWIDTH=2900000,BANDWIDTH=12061404,VIDEO-RANGE=SDR,CLOSED-CAPTIONS=NONE,CODECS="hvc1.2.20000000.H150.B0",FRAME-RATE=24.000,RESOLUTION=2048x2732,STABLE-VARIANT-ID="big"
https://mvod.itunes.apple.com/a/hvc_2048x2732.m3u8
"#;

    fn chosen(text: &str) -> Option<String> {
        variant(text, &url(MASTER_URL)).map(String::from)
    }

    #[test]
    fn chooses_the_most_pixels_under_the_cap_in_h264_only() {
        assert_eq!(
            chosen(MASTER).as_deref(),
            Some("https://mvod.itunes.apple.com/a/avc_1078x1438.m3u8")
        );
    }

    #[test]
    fn reads_the_bandwidth_itself_not_the_average() {
        let all = variants(MASTER);
        assert_eq!(all.len(), 2, "I-frame playlists and HEVC are not variants");
        let last = all.last().unwrap();
        assert_eq!((last.bandwidth, last.pixels), (2_999_000, 1078 * 1438));
    }

    #[test]
    fn takes_the_smallest_h264_with_none_under_the_cap() {
        let over = "#EXTM3U\n\
            #EXT-X-STREAM-INF:BANDWIDTH=3500000,CODECS=\"hvc1.2.20000000.H150.B0\",RESOLUTION=1078x1438\n\
            hvc_1078x1438.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=7184476,CODECS=\"avc1.640033\",RESOLUTION=2048x2732\n\
            avc_2048x2732.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=4001233,CODECS=\"avc3.640028\",RESOLUTION=1078x1438\n\
            avc_1078x1438.m3u8\n";
        assert_eq!(
            chosen(over).as_deref(),
            Some(
                "https://mvod.itunes.apple.com/itunes-assets/HLSMusic112/v4/14/avc_1078x1438.m3u8"
            ),
            "relative URIs resolve"
        );
        assert_eq!(chosen("#EXTM3U\n"), None);
    }

    #[test]
    fn takes_nothing_without_h264() {
        let hevc = "#EXTM3U\r\n\
            #EXT-X-STREAM-INF:BANDWIDTH=2000000,CODECS=\"hvc1.2.20000000.L123.B0\",RESOLUTION=1078x1438\r\n\
            hvc.m3u8\r\n\
            #EXT-X-STREAM-INF:BANDWIDTH=2000000,RESOLUTION=1078x1438\r\n\
            unknown.m3u8\r\n";
        assert_eq!(chosen(hevc), None);
    }

    #[test]
    fn survives_hostile_numbers() {
        let hostile = "#EXT-X-STREAM-INF:BANDWIDTH=99999999999,CODECS=\"avc1\",RESOLUTION=65535x65535\n\
            huge.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=4294967295,CODECS=\"avc1\",RESOLUTION=65535x65535\n\
            max.m3u8\n";
        assert_eq!(
            chosen(hostile).as_deref(),
            Some("https://mvod.itunes.apple.com/itunes-assets/HLSMusic112/v4/14/max.m3u8")
        );
    }

    #[test]
    fn splits_attributes_only_outside_quotes() {
        let list = r#"CODECS="avc1.640028,mp4a.40.2",RESOLUTION=1078x1438,URI="a.mp4""#;
        assert_eq!(attribute(list, "CODECS"), Some("avc1.640028,mp4a.40.2"));
        assert_eq!(attribute(list, "RESOLUTION"), Some("1078x1438"));
        assert_eq!(attribute(list, "URI"), Some("a.mp4"));
        assert_eq!(attribute(list, "BANDWIDTH"), None);
    }

    const VARIANT_URL: &str = "https://mvod.itunes.apple.com/itunes-assets/HLSMusic112/v4/d3/P524581047_Anull_video_gr565_sdr_1078x1438_-.m3u8";
    const CLIP: &str = "https://mvod.itunes.apple.com/itunes-assets/HLSMusic112/v4/d3/P524581047_Anull_video_gr565_sdr_1078x1438_-.mp4";

    fn file(text: &str) -> Option<String> {
        media_file(text, &url(VARIANT_URL)).map(String::from)
    }

    #[test]
    fn finds_the_one_file_every_segment_is_a_range_of() {
        let variant = "#EXTM3U\n\
            #EXT-X-TARGETDURATION:3\n\
            #EXT-X-VERSION:7\n\
            #EXT-X-PLAYLIST-TYPE:VOD\n\
            #EXT-X-MAP:URI=\"P524581047_Anull_video_gr565_sdr_1078x1438_-.mp4\",BYTERANGE=\"985@0\"\n\
            #EXTINF:2.83333,\t\n\
            #EXT-X-BYTERANGE:947221@985\n\
            P524581047_Anull_video_gr565_sdr_1078x1438_-.mp4\n\
            \n\
            #EXTINF:2.87500,\t\n\
            #EXT-X-BYTERANGE:958168@948206\n\
            P524581047_Anull_video_gr565_sdr_1078x1438_-.mp4\n\
            #EXT-X-ENDLIST\n";
        assert_eq!(
            file(variant).as_deref(),
            Some(CLIP),
            "blank lines name nothing"
        );
        assert_eq!(
            file("#EXTM3U\n#EXTINF:2.8,\nP524581047_Anull_video_gr565_sdr_1078x1438_-.mp4\n")
                .as_deref(),
            Some(CLIP),
            "without a map"
        );
        assert_eq!(
            file("#EXTM3U\n#EXTINF:2.8,\nseg1.mp4\n#EXTINF:2.8,\nseg2.mp4\n"),
            None,
            "several files"
        );
        assert_eq!(
            file("#EXTM3U\n#EXT-X-MAP:BYTERANGE=\"985@0\"\nseg.mp4\n"),
            None,
            "a map without a file"
        );
        assert_eq!(file("#EXTM3U\n"), None);
    }
}
