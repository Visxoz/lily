//! Whether another site's song and lyrics are the ones YouTube is playing.

use std::{borrow::Cow, collections::HashMap, ptr};

use icu_normalizer::{
    ComposingNormalizerBorrowed, DecomposingNormalizerBorrowed,
    properties::CanonicalCombiningClassMapBorrowed,
};
use windows_sys::Win32::Globalization::{
    LCMAP_SIMPLIFIED_CHINESE, LCMapStringEx, LOCALE_NAME_INVARIANT,
};

use super::Lyrics;
use crate::api::Track;

const BARE_CREDITS: [&str; 5] = ["feat", "feat.", "ft", "ft.", "featuring"];
const PACKAGING: [&str; 14] = [
    "",
    "video",
    "audio",
    "lyric",
    "lyrics",
    "lyricvideo",
    "lyricsvideo",
    "visualiser",
    "visualizer",
    "mv",
    "hd",
    "hq",
    "4k",
    "fullsong",
];
const VERSION_WORDS: [&str; 19] = [
    "live",
    "remix",
    "mix",
    "acoustic",
    "sped",
    "slowed",
    "nightcore",
    "instrumental",
    "karaoke",
    "demo",
    "edit",
    "extended",
    "drumless",
    "reprise",
    "unplugged",
    "version",
    "cover",
    "orchestral",
    "rework",
];
const CREDIT_JOINS: [&str; 8] = [
    " and ",
    " x ",
    " with ",
    " feat. ",
    " feat ",
    " ft. ",
    " ft ",
    " featuring ",
];

/// The title as a lyrics site files it: without credits or upload labels, but
/// keeping a version such as "(Live)", lest another recording's lyrics be taken.
pub(super) fn title(title: &str) -> String {
    let mut kept = String::new();
    let mut credited = false;
    scan(title, |part, bracketed| {
        if bracketed {
            if !is_credit(part) && !is_packaging(part) {
                kept.push_str(part);
            }
        } else if !credited {
            // A bare credit runs to the next bracket. The title's first word is
            // a name, as in "Ft. Lauderdale".
            let skip = usize::from(kept.trim().is_empty());
            match part
                .split_whitespace()
                .skip(skip)
                .find(|word| is_bare_credit(word))
            {
                Some(credit) => {
                    kept.push_str(&part[..credit.as_ptr().addr() - part.as_ptr().addr()]);
                    credited = true;
                }
                None => kept.push_str(part),
            }
        }
    });
    let mut name = String::with_capacity(kept.len());
    for word in kept.split_whitespace() {
        if !name.is_empty() {
            name.push(' ');
        }
        name.push_str(word);
    }
    let end = name
        .trim_end_matches(|c: char| c.is_whitespace() || ",-–—".contains(c))
        .len();
    name.truncate(end);
    if name.is_empty() {
        title.to_owned()
    } else {
        name
    }
}

/// The first artist's name, without YouTube's " - Topic".
pub(super) fn artist(track: &Track) -> Option<String> {
    let name = without_topic(&track.artists.first()?.name);
    (!name.is_empty()).then(|| name.to_owned())
}

/// The same name, the same version, a shared artist and within 3 s. Remasters
/// count as the same, as their words and timing are.
pub(super) fn same_song(track: &Track, title: &str, artist: &str, duration: Option<f64>) -> bool {
    let close = matches!((track.duration, duration),
        (Some(length), Some(duration)) if (f64::from(length) - duration).abs() <= 3.0);
    if !close || parts(&track.title) != parts(title) {
        return false;
    }
    let theirs = credits(artist);
    track
        .artists
        .iter()
        .any(|a| credits(&a.name).iter().any(|name| theirs.contains(name)))
}

/// YouTube's lyrics tokenised once, to hold each site's against.
pub(super) struct Reference {
    tokens: Tokens,
    /// Each word's id, counting from 0, so `align`'s masks stay small.
    interned: HashMap<String, usize>,
}

impl Reference {
    pub(super) fn new(lyrics: &Lyrics) -> Self {
        let mut interned = HashMap::new();
        let tokens = tokens(lyrics, usize::MAX, |text| {
            if let Some(&id) = interned.get(text) {
                return id;
            }
            let id = interned.len();
            interned.insert(text.to_owned(), id);
            id
        });
        Self { tokens, interned }
    }

    /// Whether the candidate has mostly these words in their order and, when
    /// both are timed, starts its lines with these: a live take has the words
    /// but not the timing.
    pub(super) fn agrees(&self, candidate: &Lyrics) -> bool {
        let ours = &self.tokens;
        let count = ours.ids.len();
        if count == 0 {
            return false;
        }
        // A far longer answer is another text, and would only cost memory to
        // align. Its words ours lack share an id that matches nothing.
        let most = 4 * count + 64;
        let theirs = tokens(candidate, most, |text| {
            self.interned.get(text).copied().unwrap_or(usize::MAX)
        });
        if theirs.ids.len() > most {
            return false;
        }
        let needed = 0.6 * count as f64;
        // The words in common bound the match, sparing aligning another song.
        let mut left = vec![0usize; self.interned.len()];
        for &id in &theirs.ids {
            if let Some(n) = left.get_mut(id) {
                *n += 1;
            }
        }
        let mut shared = 0;
        for &id in &ours.ids {
            if left[id] > 0 {
                left[id] -= 1;
                shared += 1;
            }
        }
        if (shared as f64) < needed {
            return false;
        }
        // How much later the candidate starts each line both start with the same word.
        let mut gaps = Vec::new();
        let kept = align(&ours.ids, &theirs.ids, |i, j| {
            if let (Some(ours), Some(theirs)) = (ours.start(i), theirs.start(j)) {
                gaps.push(theirs - ours);
            }
        });
        if (kept as f64) < needed {
            return false;
        }
        if gaps.is_empty() {
            return true;
        }
        gaps.sort_unstable_by(f64::total_cmp);
        let middle = gaps.len() / 2;
        let median = if gaps.len().is_multiple_of(2) {
            (gaps[middle - 1] + gaps[middle]) / 2.0
        } else {
            gaps[middle]
        };
        median.abs() <= 1.5
    }
}

/// Traditional Chinese characters in their Simplified forms, which the Chinese
/// sites file names under.
pub(super) fn simplified(text: &str) -> Cow<'_, str> {
    if !text
        .chars()
        .any(|c| matches!(u32::from(c), 0x3400..0xA000 | 0x20000..))
    {
        return Cow::Borrowed(text);
    }
    let wide: Vec<u16> = text.encode_utf16().collect();
    let Ok(len) = i32::try_from(wide.len()) else {
        return Cow::Borrowed(text);
    };
    let map = |out: &mut [u16]| {
        let Ok(capacity) = i32::try_from(out.len()) else {
            return 0;
        };
        // SAFETY: `wide` holds `len` units and `out` `capacity`, both borrowed
        // through the call, which writes at most `capacity` units and none when
        // it is 0; the locale is a static null-terminated string.
        unsafe {
            LCMapStringEx(
                LOCALE_NAME_INVARIANT,
                LCMAP_SIMPLIFIED_CHINESE,
                wide.as_ptr(),
                len,
                out.as_mut_ptr(),
                capacity,
                ptr::null(),
                ptr::null(),
                0,
            )
        }
    };
    let mut out = vec![0; usize::try_from(map(&mut [])).unwrap_or(0)];
    let written = usize::try_from(map(&mut out)).unwrap_or(0);
    if written == 0 || written > out.len() {
        return Cow::Borrowed(text);
    }
    out.truncate(written);
    // Windows leaves these common ones be, where ICU, which the iOS app uses,
    // simplifies them.
    for unit in &mut out {
        *unit = match *unit {
            0x5F8C => 0x540E, // 後 to 后
            0x65BC => 0x4E8E, // 於 to 于
            0x9EBD => 0x4E48, // 麽, as Windows makes 麼, to 么
            unit => unit,
        };
    }
    String::from_utf16(&out).map_or(Cow::Borrowed(text), Cow::Owned)
}

/// Calls `body` with each stretch outside brackets and each bracketed group,
/// brackets included, in order.
fn scan<'a>(text: &'a str, mut body: impl FnMut(&'a str, bool)) {
    let mut rest = text;
    while let Some(open) = rest.find(['(', '['])
        && let Some(close) = rest[open + 1..].find([')', ']'])
    {
        let close = open + 1 + close;
        body(&rest[..open], false);
        body(&rest[open..=close], true);
        rest = &rest[close + 1..];
    }
    body(rest, false);
}

fn is_bare_credit(word: &str) -> bool {
    BARE_CREDITS
        .iter()
        .any(|credit| word.eq_ignore_ascii_case(credit))
}

/// "(feat. X)", "[ft. X]" or "(with X)".
fn is_credit(group: &str) -> bool {
    let inside = group[1..].trim_start();
    let first = &inside[..inside
        .find(|c: char| !c.is_alphabetic())
        .unwrap_or(inside.len())];
    first.eq_ignore_ascii_case("with") || is_bare_credit(first)
}

/// "(Official Music Video)", "[Lyrics]" and the like.
fn is_packaging(group: &str) -> bool {
    let key = key(group);
    let mut label = key.as_str();
    for prefix in ["official", "music"] {
        label = label.strip_prefix(prefix).unwrap_or(label);
    }
    PACKAGING.contains(&label)
}

/// The name outside brackets and before any " - " suffix, and the version words
/// in the rest, as bits of `VERSION_WORDS`.
fn parts(title: &str) -> (String, u32) {
    let kept = self::title(title);
    let (mut outside, mut rest) = (String::new(), String::new());
    scan(&kept, |part, bracketed| {
        if bracketed {
            outside.push(' ');
            rest.push_str(part);
        } else {
            outside.push_str(part);
        }
    });
    if let Some(dash) = outside.find(" - ") {
        rest.push_str(&outside[dash + 3..]);
        outside.truncate(dash);
    }
    let mut name = key(&outside);
    // A title of symbols keeps them, lest "✨" and "🪐" both have no name and
    // pass as one.
    if name.is_empty() {
        name = folded(&outside);
        name.retain(|c| !c.is_whitespace());
    }
    if name.is_empty() {
        name = key(&kept);
    }
    let versions = words(&folded(&rest))
        .filter_map(|word| VERSION_WORDS.iter().position(|&version| version == word))
        .fold(0, |set, i| set | 1 << i);
    (name, versions)
}

fn without_topic(name: &str) -> &str {
    let name = name.trim();
    name.strip_suffix(" - Topic").unwrap_or(name)
}

/// Each name a credit joins, as "A feat. B & C" is A, B and C, and
/// "BTS (防弹少年团)、Halsey" is all three.
fn credits(credit: &str) -> Vec<String> {
    let mut credit = folded(without_topic(credit));
    for join in CREDIT_JOINS {
        if credit.contains(join) {
            credit = credit.replace(join, ",");
        }
    }
    credit
        .split([',', '&', ';', '/', '、', '(', ')'])
        .map(|name| {
            name.chars()
                .filter(|c| c.is_alphanumeric())
                .collect::<String>()
        })
        .filter(|name| !name.is_empty())
        .collect()
}

/// Each token's id, one per word or per character of scripts written without
/// spaces, and when timed, each line's start on its first token.
#[derive(Default)]
struct Tokens {
    ids: Vec<usize>,
    /// Empty when untimed.
    starts: Vec<Option<f64>>,
}

impl Tokens {
    fn start(&self, i: usize) -> Option<f64> {
        self.starts.get(i).copied().flatten()
    }
}

/// The lyrics' tokens, each named by `id`, stopping after the line that takes
/// them past `most`.
fn tokens(lyrics: &Lyrics, most: usize, mut id: impl FnMut(&str) -> usize) -> Tokens {
    let mut tokens = Tokens::default();
    let timed = lyrics.is_synced();
    for (line, text) in lyrics.lines.iter().enumerate() {
        let first = tokens.ids.len();
        let text = folded(text);
        for word in words(&text) {
            let mut from = 0;
            for (at, c) in word.char_indices().filter(|&(_, c)| stands_alone(c)) {
                if at > from {
                    tokens.ids.push(id(&word[from..at]));
                }
                from = at + c.len_utf8();
                tokens.ids.push(id(&word[at..from]));
            }
            if from < word.len() {
                tokens.ids.push(id(&word[from..]));
            }
        }
        if timed && tokens.ids.len() > first {
            tokens.starts.resize(tokens.ids.len(), None);
            tokens.starts[first] = lyrics.starts.get(line).copied();
        }
        if tokens.ids.len() > most {
            break;
        }
    }
    tokens
}

/// The length of the longest common subsequence, calling `pair` with each pair
/// of indices it matches, in order. Bit-parallel (Hyyrö 2004) over both
/// reversed, so that tracing back pairs a repeat's first occurrence, as a diff
/// does: bit j of row i is clear where `b`'s (j + 1)th from last lengthens the
/// LCS of `a`'s last i and `b`'s last j. `a`'s ids index the masks, so must be
/// small, as interned ones are.
fn align(a: &[usize], b: &[usize], mut pair: impl FnMut(usize, usize)) -> usize {
    let (n, m) = (a.len(), b.len());
    let words = m.div_ceil(64);
    // Where in reversed `b` each of `a`'s ids is, as bits.
    let ids = a.iter().max().map_or(0, |&id| id + 1);
    let mut masks = vec![0u64; ids * words];
    for (j, &id) in b.iter().rev().enumerate() {
        if id < ids {
            masks[id * words + j / 64] |= 1 << (j % 64);
        }
    }
    let mut rows = vec![!0u64; (n + 1) * words];
    for (i, &id) in a.iter().rev().enumerate() {
        let (done, next) = rows.split_at_mut((i + 1) * words);
        let mask = &masks[id * words..][..words];
        let mut carry = false;
        for ((row, &prev), &mask) in next[..words].iter_mut().zip(&done[i * words..]).zip(mask) {
            let (sum, over) = prev.carrying_add(prev & mask, carry);
            carry = over;
            *row = sum | (prev & !mask);
        }
    }
    let (mut i, mut j, mut kept) = (n, m, 0);
    while i > 0 && j > 0 {
        if a[n - i] == b[m - j] {
            pair(n - i, m - j);
            i -= 1;
            j -= 1;
            kept += 1;
        } else if (rows[i * words + (j - 1) / 64] >> ((j - 1) % 64)) & 1 == 1 {
            j -= 1;
        } else {
            i -= 1;
        }
    }
    kept
}

/// Han, kana and hangul.
fn stands_alone(c: char) -> bool {
    matches!(u32::from(c), 0x1100..0x1200 | 0x2E80..0xA000 | 0xAC00..0xD7B0 | 0xF900..0xFB00 | 0x20000..)
}

/// Lower case, without accents, with full-width forms made plain and Chinese
/// in Simplified characters, as "ROSÉ" is "rose" and 周杰倫 is 周杰伦.
fn folded(text: &str) -> String {
    // Most lines are ASCII, which none of the rest would change.
    if text.is_ascii() {
        return text.to_ascii_lowercase();
    }
    let marks = CanonicalCombiningClassMapBorrowed::new();
    let mut plain: String = DecomposingNormalizerBorrowed::new_nfkd()
        .normalize_iter(text.chars())
        .filter(|&c| marks.get_u8(c) == 0)
        .flat_map(char::to_lowercase)
        .collect();
    // As case folding has them, so "Straße" is "STRASSE" and "σοφος" "ΣΟΦΟΣ".
    if plain.contains(['ß', 'ς']) {
        plain = plain.replace('ß', "ss").replace('ς', "σ");
    }
    // Decomposing split hangul syllables into letters, which would each be a word.
    if let Cow::Owned(composed) = ComposingNormalizerBorrowed::new_nfc().normalize(&plain) {
        plain = composed;
    }
    if let Cow::Owned(simple) = simplified(&plain) {
        plain = simple;
    }
    plain
}

/// The folded text's runs of letters and digits.
fn words(folded: &str) -> impl Iterator<Item = &str> {
    folded
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
}

fn key(text: &str) -> String {
    let mut key = folded(text);
    key.retain(char::is_alphanumeric);
    key
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ArtistRef;

    fn agrees(reference: &Lyrics, candidate: &Lyrics) -> bool {
        Reference::new(reference).agrees(candidate)
    }

    fn track(title: &str, artists: &[&str], duration: u32) -> Track {
        Track {
            id: "v".into(),
            title: title.into(),
            artists: artists
                .iter()
                .map(|&name| ArtistRef {
                    id: None,
                    name: name.into(),
                })
                .collect(),
            duration: Some(duration),
            ..Track::default()
        }
    }

    fn lucky() -> Track {
        track(
            "Get Lucky (feat. Pharrell Williams and Nile Rodgers)",
            &["Daft Punk"],
            369,
        )
    }

    #[test]
    fn titles_lose_credits_and_labels() {
        assert_eq!(
            title("Get Lucky (feat. Pharrell Williams and Nile Rodgers)"),
            "Get Lucky"
        );
        assert_eq!(title("Song [ft. Someone]"), "Song");
        assert_eq!(title("Song (with Someone)"), "Song");
        assert_eq!(title("Song feat. Someone & Another"), "Song");
        assert_eq!(title("Song (Official Music Video)"), "Song");
        assert_eq!(title("Song [Lyrics] (Audio)"), "Song");
        assert_eq!(title("Song (Official Visualiser) -"), "Song");
    }

    #[test]
    fn titles_keep_their_version() {
        assert_eq!(title("Song (Live) [feat. Someone]"), "Song (Live)");
        assert_eq!(title("Song feat. Someone (Live)"), "Song (Live)");
        assert_eq!(
            title("Song (Remix feat. Someone)"),
            "Song (Remix feat. Someone)"
        );
        assert_eq!(title("Ft. Lauderdale"), "Ft. Lauderdale");
        assert_eq!(title("Song (Sped Up)"), "Song (Sped Up)");
        assert_eq!(title("Song (Without You)"), "Song (Without You)");
        assert_eq!(
            title("(Official Video)"),
            "(Official Video)",
            "nothing would be left"
        );
    }

    #[test]
    fn artist_is_the_first_without_topic() {
        assert_eq!(
            artist(&track("Song", &["Daft Punk - Topic", "Someone"], 200)).as_deref(),
            Some("Daft Punk")
        );
        assert_eq!(artist(&track("Song", &[], 200)), None);
    }

    #[test]
    fn same_song_despite_credits_and_script() {
        assert!(same_song(&lucky(), "Get Lucky", "Daft Punk", Some(369.4)));
        assert!(same_song(
            &track("アイドル", &["YOASOBI"], 213),
            "アイドル",
            "YOASOBI",
            Some(213.2)
        ));
        assert!(same_song(
            &track("Song - 2011 Remaster", &["Band"], 200),
            "Song",
            "Band",
            Some(200.0)
        ));
        assert!(same_song(
            &track("Song (Remix)", &["Band"], 200),
            "Song [Remix]",
            "band",
            Some(200.0)
        ));
        assert!(same_song(
            &track("Rosé", &["ROSÉ"], 200),
            "ROSE",
            "Rose",
            Some(200.0)
        ));
        assert!(
            same_song(
                &track("晴天", &["周杰倫"], 200),
                "晴天",
                "周杰伦",
                Some(200.0)
            ),
            "Traditional and Simplified"
        );
    }

    #[test]
    fn another_version_is_another_song() {
        assert!(!same_song(
            &lucky(),
            "Get Lucky (Drumless Edition)",
            "Daft Punk",
            Some(369.0)
        ));
        assert!(!same_song(
            &track("Dynamite", &["BTS"], 200),
            "Dynamite (Live)",
            "BTS",
            Some(200.0)
        ));
        assert!(!same_song(
            &track("Song (Remix)", &["Band"], 200),
            "Song",
            "Band",
            Some(200.0)
        ));
        assert!(!same_song(
            &track("アイドル", &["YOASOBI"], 213),
            "Idol",
            "YOASOBI",
            Some(213.0)
        ));
        assert!(
            !same_song(&track("✨", &["Band"], 200), "🪐", "Band", Some(200.0)),
            "symbols alone are still the name"
        );
        assert!(same_song(
            &track("✨ ", &["Band"], 200),
            "✨",
            "Band",
            Some(200.0)
        ));
        assert!(!same_song(
            &track("✨ (Live)", &["Band"], 200),
            "🪐 (Live)",
            "Band",
            Some(200.0)
        ));
        assert!(same_song(
            &track("✨ (feat. IU)", &["Band"], 200),
            "✨",
            "Band",
            Some(200.0)
        ));
    }

    #[test]
    fn artists_must_share_one() {
        assert!(same_song(
            &lucky(),
            "Get Lucky",
            "Pharrell Williams & Daft Punk",
            Some(369.0)
        ));
        assert!(same_song(
            &lucky(),
            "Get Lucky",
            "Someone x Daft Punk feat. Another",
            Some(369.0)
        ));
        assert!(same_song(
            &track("Song", &["Band - Topic"], 200),
            "Song",
            "Band",
            Some(200.0)
        ));
        assert!(same_song(
            &track("Song", &["Band"], 200),
            "Song",
            "Someone、Band",
            Some(200.0)
        ));
        assert!(same_song(
            &track("Dynamite", &["BTS"], 200),
            "Dynamite",
            "BTS (防弹少年团)",
            Some(200.0)
        ));
        assert!(!same_song(
            &lucky(),
            "Get Lucky",
            "Someone Else",
            Some(369.0)
        ));
    }

    #[test]
    fn lengths_must_be_known_and_close() {
        assert!(same_song(&lucky(), "Get Lucky", "Daft Punk", Some(372.0)));
        assert!(!same_song(&lucky(), "Get Lucky", "Daft Punk", Some(373.0)));
        assert!(!same_song(&lucky(), "Get Lucky", "Daft Punk", None));
        assert!(!same_song(
            &lucky(),
            "Get Lucky",
            "Daft Punk",
            Some(f64::NAN)
        ));
        let unknown = Track {
            duration: None,
            ..track("Song", &["Band"], 200)
        };
        assert!(!same_song(&unknown, "Song", "Band", Some(200.0)));
    }

    const LINES: [&str; 4] = [
        "Alpha beta gamma delta",
        "epsilon zeta eta theta",
        "iota kappa lambda mu",
        "nu xi omicron pi",
    ];
    const STARTS: [f64; 4] = [10.0, 14.0, 18.0, 22.0];

    fn lyrics(lines: &[&str], starts: &[f64]) -> Lyrics {
        Lyrics {
            lines: lines.iter().map(|&line| line.into()).collect(),
            starts: starts.to_vec(),
            ..Lyrics::default()
        }
    }

    #[test]
    fn same_words_in_time_agree() {
        let reference = lyrics(&LINES, &STARTS);
        assert!(agrees(&reference, &reference));
        assert!(agrees(
            &reference,
            &lyrics(&LINES, &STARTS.map(|start| start + 0.5))
        ));
        assert!(
            agrees(&lyrics(&LINES, &[]), &reference),
            "untimed words are compared alone"
        );
    }

    #[test]
    fn other_words_or_timing_disagree() {
        let reference = lyrics(&LINES, &STARTS);
        let other = [
            "one two three four",
            "five six seven eight",
            "nine ten eleven twelve",
            "thirteen fourteen fifteen sixteen",
        ];
        assert!(!agrees(&reference, &lyrics(&other, &STARTS)));
        assert!(!agrees(
            &reference,
            &lyrics(&LINES, &STARTS.map(|start| start + 3.0))
        ));
        assert!(
            !agrees(&lyrics(&["", "..."], &[]), &reference),
            "a reference without words agrees with nothing"
        );
    }

    #[test]
    fn lines_split_differently_still_agree() {
        let candidate = lyrics(
            &[
                "ALPHA, beta",
                "gamma delta!",
                "Epsilon zeta eta theta",
                "(iota) kappa lambda mu",
                "nu xi omicron pi",
            ],
            &[10.2, 12.0, 14.2, 18.2, 22.2],
        );
        assert!(agrees(&lyrics(&LINES, &STARTS), &candidate));
    }

    #[test]
    fn each_cjk_character_is_a_word() {
        let reference = lyrics(&["あいうえお かきくけこ", "さしすせそ"], &[5.0, 9.0]);
        assert!(agrees(
            &reference,
            &lyrics(&["あいう", "えおかきくけこさしすせそ"], &[5.1, 7.0])
        ));
        assert!(!agrees(
            &reference,
            &lyrics(&["たちつてと なにぬねの", "はひふへほ"], &[5.0, 9.0])
        ));
        // Only 我 and 你 are written alike in both scripts, so these agree only through the fold.
        assert!(agrees(
            &lyrics(&["我們說愛你", "聽見風聲"], &[]),
            &lyrics(&["我们说爱你", "听见风声"], &[])
        ));
        assert_eq!(folded("사랑해"), "사랑해", "hangul stays in syllables");
    }

    #[test]
    fn case_folds_as_swift_does() {
        assert_eq!(folded("STRASSE"), folded("Straße"));
        assert_eq!(folded("ΣΟΦΟΣ"), folded("σοφος"));
        assert_eq!(folded("ＲＯＳÉ"), "rose");
        assert_eq!(folded("It's 4AM, Baby"), "it's 4am, baby");
        assert_eq!(folded("4AM Café"), "4am cafe", "ASCII beside an accent");
    }

    #[test]
    fn a_reference_holds_against_many() {
        let reference = lyrics(&LINES, &STARTS);
        let other = lyrics(&["one two three four", "five six"], &[10.0, 14.0]);
        let late = lyrics(&LINES, &STARTS.map(|start| start + 3.0));
        let untimed = lyrics(&LINES, &[]);
        let held = Reference::new(&reference);
        let answers = [&reference, &other, &untimed, &late, &reference].map(|c| held.agrees(c));
        assert_eq!(answers, [true, false, true, false, true]);
    }

    #[test]
    fn repeats_pair_with_their_first() {
        let reference = lyrics(
            &["verse line here", "chorus line here", "chorus line here"],
            &[10.0, 18.0, 26.0],
        );
        let mut longer = reference.clone();
        longer.lines.push("chorus line here".into());
        longer.starts.push(34.0);
        assert!(agrees(&reference, &longer), "a site's extra chorus");
        assert!(agrees(&longer, &reference), "a chorus the site lacks");
    }

    #[test]
    fn simplifies_traditional_chinese() {
        assert_eq!(simplified("周杰倫"), "周杰伦");
        assert_eq!(simplified("我們說愛你"), "我们说爱你");
        assert_eq!(simplified("最後說什麼"), "最后说什么");
        assert_eq!(simplified("於是"), "于是");
        assert!(matches!(simplified("Song"), Cow::Borrowed("Song")));
    }

    #[test]
    fn odd_answers_disagree() {
        let reference = lyrics(&LINES, &STARTS);
        assert!(!agrees(&reference, &lyrics(&LINES, &[f64::NAN; 4])));
        let long = vec![LINES.join(" "); 20];
        let long: Vec<&str> = long.iter().map(String::as_str).collect();
        assert!(
            !agrees(&reference, &lyrics(&long, &[])),
            "far longer than the reference"
        );
        assert!(!agrees(&reference, &Lyrics::default()));
    }

    #[test]
    fn alignment_matches_a_full_table() {
        let mut seed = 7u64;
        let mut next = |below: u64| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((seed >> 33) % below) as usize
        };
        for _ in 0..200 {
            let alphabet = 1 + next(12) as u64;
            let a: Vec<usize> = (0..next(150)).map(|_| next(alphabet)).collect();
            let b: Vec<usize> = (0..next(150)).map(|_| next(alphabet)).collect();
            let mut table = vec![vec![0; b.len() + 1]; a.len() + 1];
            for i in 1..=a.len() {
                for j in 1..=b.len() {
                    table[i][j] = if a[i - 1] == b[j - 1] {
                        table[i - 1][j - 1] + 1
                    } else {
                        table[i - 1][j].max(table[i][j - 1])
                    };
                }
            }
            let mut pairs = Vec::new();
            let kept = align(&a, &b, |i, j| pairs.push((i, j)));
            assert_eq!(kept, table[a.len()][b.len()]);
            assert_eq!(pairs.len(), kept);
            assert!(pairs.iter().all(|&(i, j)| a[i] == b[j]));
            assert!(pairs.windows(2).all(|w| w[0].0 < w[1].0 && w[0].1 < w[1].1));
        }
    }
}
