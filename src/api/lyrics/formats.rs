//! The lyrics formats other sites serve: Apple Music's TTML, LRC, and Tencent Music's KRC and QRC.

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::Read;
use std::mem;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use flate2::read::ZlibDecoder;
use roxmltree::{Document, Node};

use super::{Lyrics, Word};

/// More lines than any song has, which the panel would build every one of.
pub(super) const MAX_LINES: usize = 2000;
/// Likewise for words, each of which the panel draws as its own text.
pub(super) const MAX_WORDS: usize = 10_000;

/// Apple Music's TTML, word timed or line timed; `None` when the XML doesn't parse or holds no lines.
pub(super) fn ttml(text: &str, source: &str) -> Option<Lyrics> {
    // roxmltree refuses DTDs by default, so no entity can expand or reach outside the document.
    let document = Document::parse(text).ok()?;
    let mut reader = Reader::default();
    reader.walk(document.root());
    if reader.lines.is_empty() {
        return None;
    }
    let starts = if reader.timed {
        reader.starts
    } else {
        Vec::new()
    };
    let [translations, romanised] = reader.extras.map(|lines| or_empty(lines, String::is_empty));
    Some(Lyrics {
        lines: reader.lines,
        starts,
        words: or_empty(reader.words, Vec::is_empty),
        translations,
        romanised,
        source: source.to_owned(),
    })
}

/// Empty unless some line has one.
fn or_empty<T>(lines: Vec<T>, empty: impl Fn(&T) -> bool) -> Vec<T> {
    if lines.iter().all(empty) {
        Vec::new()
    } else {
        lines
    }
}

/// A clock value in seconds: "27.395", "1:05.20", "1:02:03.4", "12.3s" or "500ms".
fn seconds(clock: &str) -> Option<f64> {
    let value = if let Some(ms) = clock.strip_suffix("ms") {
        ms.parse::<f64>().ok()? / 1000.0
    } else if let Some(s) = clock.strip_suffix('s') {
        s.parse().ok()?
    } else {
        let mut parts = clock.split(':');
        let value = parts.by_ref().take(3).try_fold(0.0, |value, part| {
            Some(value * 60.0 + part.parse::<f64>().ok()?)
        })?;
        if parts.next().is_some() {
            return None;
        }
        value
    };
    (value.is_finite() && value >= 0.0).then_some(value)
}

/// A span as opened, with its times only when it has both.
struct Span {
    times: Option<(f64, f64)>,
    has_child: bool,
}

/// Indices into `Reader`'s extras.
const TRANSLATION: usize = 0;
const ROMANISED: usize = 1;

/// TTML read in document order, as a SAX parser sees it.
#[derive(Default)]
struct Reader<'a> {
    lines: Vec<String>,
    starts: Vec<f64>,
    words: Vec<Vec<Word>>,
    /// Each line's translation and romanisation.
    extras: [Vec<String>; 2],
    timed: bool,
    // Only <body>'s <p> is sung; <head> holds the extras by line key.
    in_body: bool,
    in_line: bool,
    /// The head's first translation and romanisation, by line key.
    by_key: [HashMap<&'a str, String>; 2],
    /// The head's translation or romanisation being read.
    section: Option<usize>,
    key: Option<&'a str>,
    /// The line's own translation and romanisation spans.
    inline: [String; 2],
    begin: Option<f64>,
    line_words: Vec<Word>,
    /// Text since the last span boundary, its whitespace collapsed.
    pending: String,
    spans: Vec<Span>,
}

impl<'a> Reader<'a> {
    /// Visits every node without recursion, so deep nesting can't exhaust the stack.
    fn walk(&mut self, root: Node<'a, '_>) {
        let mut node = root;
        loop {
            let read = self.open(node);
            if read && let Some(child) = node.first_child() {
                node = child;
                continue;
            }
            if read {
                self.close(node);
            }
            loop {
                if node == root {
                    return;
                }
                if let Some(sibling) = node.next_sibling() {
                    node = sibling;
                    break;
                }
                let Some(parent) = node.parent() else { return };
                node = parent;
                self.close(node);
            }
        }
    }

    /// Whether to read the children: extras aren't sung.
    fn open(&mut self, node: Node<'a, '_>) -> bool {
        if node.is_text() {
            if self.in_line {
                collapse(node.text().unwrap_or_default(), &mut self.pending);
            }
            return false;
        }
        match node.tag_name().name() {
            "body" => self.in_body = true,
            name @ ("translation" | "transliteration") if !self.in_body => {
                let kind = if name == "translation" {
                    TRANSLATION
                } else {
                    ROMANISED
                };
                // Only the first language's, unread past it.
                if !self.by_key[kind].is_empty() {
                    return false;
                }
                self.section = Some(kind);
            }
            "text" if let Some(kind) = self.section => {
                if let Some(key) = attribute(node, "for") {
                    self.by_key[kind]
                        .entry(key)
                        .or_insert_with(|| text_of(node));
                }
                return false;
            }
            "p" if self.in_body => {
                self.in_line = true;
                self.begin = time(node, "begin");
                self.key = attribute(node, "key");
            }
            "span" if self.in_line => {
                let kind = match attribute(node, "role") {
                    Some("x-translation") => Some(TRANSLATION),
                    Some("x-roman") => Some(ROMANISED),
                    _ => None,
                };
                if let Some(kind) = kind {
                    if self.inline[kind].is_empty() {
                        self.inline[kind] = text_of(node);
                    }
                    return false;
                }
                self.flush();
                if let Some(parent) = self.spans.last_mut() {
                    parent.has_child = true;
                }
                self.spans.push(Span {
                    times: time(node, "begin").zip(time(node, "end")),
                    has_child: false,
                });
            }
            _ => {}
        }
        true
    }

    fn close(&mut self, node: Node) {
        match node.tag_name().name() {
            "body" => self.in_body = false,
            "translation" | "transliteration" => self.section = None,
            "p" if self.in_line => self.end_line(),
            "span" if self.in_line => self.end_span(),
            _ => {}
        }
    }

    /// The innermost timed spans are the words; any other span's text is untimed.
    fn end_span(&mut self) {
        let Some(Span {
            times: Some((start, end)),
            has_child: false,
        }) = self.spans.pop()
        else {
            self.flush();
            return;
        };
        // A leading space belongs after the word before, or nowhere on the first.
        if self.pending.starts_with(' ') {
            self.pending.remove(0);
            if let Some(last) = self.line_words.last_mut()
                && !last.text.ends_with(' ')
            {
                last.text.push(' ');
            }
        }
        if !self.pending.is_empty() {
            let text = mem::take(&mut self.pending);
            self.line_words.push(Word { text, start, end });
        }
    }

    fn end_line(&mut self) {
        self.flush();
        let mut words = mem::take(&mut self.line_words);
        if let Some(last) = words.last_mut()
            && last.text.ends_with(' ')
        {
            last.text.pop();
        }
        let text: String = if words.is_empty() {
            self.pending.trim().to_owned()
        } else {
            words.iter().map(|word| word.text.as_str()).collect()
        };
        if !text.is_empty() {
            let first = words.first().map_or(f64::INFINITY, |word| word.start);
            let start = self.begin.unwrap_or(f64::INFINITY).min(first);
            self.timed |= start.is_finite();
            let start = if start.is_finite() {
                start
            } else {
                self.starts.last().copied().unwrap_or(0.0)
            };
            for (kind, extras) in self.extras.iter_mut().enumerate() {
                let mut extra = mem::take(&mut self.inline[kind]);
                if extra.is_empty() {
                    extra = self
                        .key
                        .and_then(|key| self.by_key[kind].remove(key))
                        .unwrap_or_default();
                }
                // A copy of the line adds nothing.
                extras.push(if extra == text { String::new() } else { extra });
            }
            self.lines.push(text);
            self.starts.push(start);
            self.words.push(words);
        }
        self.in_line = false;
        self.inline = Default::default();
        self.pending.clear();
        self.spans.clear();
    }

    /// Untimed text, such as a space or comma, joins the word before; before the first word it waits, and is the
    /// whole line if no word comes.
    fn flush(&mut self) {
        if let Some(last) = self.line_words.last_mut()
            && !self.pending.is_empty()
        {
            collapse(&self.pending, &mut last.text);
            self.pending.clear();
        }
    }
}

/// Appends `text` with each whitespace run, pretty-printed newlines included, collapsed to one space.
fn collapse(text: &str, to: &mut String) {
    for c in text.chars() {
        if !c.is_whitespace() {
            to.push(c);
        } else if !to.ends_with(' ') {
            to.push(' ');
        }
    }
}

/// All the text within `node`, its whitespace collapsed and trimmed.
fn text_of(node: Node) -> String {
    let mut text = String::new();
    for part in node.descendants().filter(Node::is_text) {
        collapse(part.text().unwrap_or_default(), &mut text);
    }
    // `collapse` leaves at most one space at either end.
    if text.ends_with(' ') {
        text.pop();
    }
    if text.starts_with(' ') {
        text.remove(0);
    }
    text
}

/// Prefixes differ by host (tt:, itunes:, lrc:, ttm:), so attributes are matched by local name.
fn attribute<'a>(node: Node<'a, '_>, name: &str) -> Option<&'a str> {
    node.attributes()
        .find(|attribute| attribute.name() == name)
        .map(|attribute| attribute.value())
}

fn time(node: Node, name: &str) -> Option<f64> {
    attribute(node, name).and_then(seconds)
}

/// Line-synced LRC. Enhanced `<mm:ss.xx>` word stamps are dropped, so lines carry no words.
pub(super) fn lrc(text: &str, source: &str) -> Option<Lyrics> {
    let mut timed: Vec<(f64, String)> = Vec::new();
    let mut offset = 0.0;
    let mut starts = Vec::new();
    for line in lines(text) {
        let mut rest = line;
        starts.clear();
        // `[00:12.00][01:30.00]chorus` is one line sung at each stamp.
        while let Some((stamp, after)) =
            rest.strip_prefix('[').and_then(|rest| rest.split_once(']'))
            && let Some(start) = stamp_seconds(stamp)
        {
            starts.push(start);
            // Here, so neither a hostile line's stamps nor the copies made for them grow unchecked.
            if timed.len() + starts.len() > MAX_LINES {
                return None;
            }
            rest = after.trim_start();
        }
        let Some((&last, earlier)) = starts.split_last() else {
            // Milliseconds; a positive offset shows the lines sooner.
            if let Some(ms) = offset_tag(line).map(str::trim).and_then(whole) {
                offset = ms / 1000.0;
            }
            continue;
        };
        let mut shown = unstamped(rest);
        // A bare stamp is a pause, which YouTube Music shows as a note.
        if shown.is_empty() {
            shown.push('♪');
        }
        timed.extend(earlier.iter().map(|&start| (start, shown.clone())));
        timed.push((last, shown));
    }
    if timed.is_empty() {
        return None;
    }
    // Stable, so lines sharing a start keep the file's order.
    timed.sort_by(|a, b| a.0.total_cmp(&b.0));
    let (starts, lines) = timed
        .into_iter()
        .map(|(start, line)| ((start - offset).max(0.0), line))
        .unzip();
    Some(Lyrics {
        lines,
        starts,
        source: source.to_owned(),
        ..Lyrics::default()
    })
}

/// `mm:ss`, `mm:ss.xx`, `mm:ss.xxx` or `mm:ss:xx`, as one division of exact integers so `13.13` equals its
/// literal; three-digit parts can't overflow.
fn stamp_seconds(stamp: &str) -> Option<f64> {
    let mut parts = stamp.split([':', '.']);
    let minutes = digits(parts.next()?)?;
    let whole = digits(parts.next()?)?;
    let (fraction, places) = match parts.next() {
        Some(part) => (digits(part)?, part.len()),
        None => (0, 0),
    };
    if parts.next().is_some() {
        return None;
    }
    let scale = 10u32.pow(places as u32);
    Some(f64::from((minutes * 60 + whole) * scale + fraction) / f64::from(scale))
}

/// One to three ASCII digits.
fn digits(part: &str) -> Option<u32> {
    if !(1..=3).contains(&part.len()) {
        return None;
    }
    part.bytes().try_fold(0, |n, b| {
        b.is_ascii_digit().then(|| n * 10 + u32::from(b - b'0'))
    })
}

fn unstamped(body: &str) -> String {
    let mut rest = body.trim_start();
    let mut text = String::with_capacity(rest.len());
    while let Some(open) = rest.find('<') {
        let after = &rest[open + 1..];
        // A stamp is at most 11 characters, so a stray `<` never scans the rest of the line.
        if let Some(close) = after.bytes().take(12).position(|b| b == b'>')
            && stamp_seconds(&after[..close]).is_some()
        {
            text.push_str(&rest[..open]);
            rest = &after[close + 1..];
            // `Alpha <00:01.40> beta` keeps one space between the words.
            if text.chars().next_back().is_none_or(char::is_whitespace) {
                rest = rest.trim_start();
            }
        } else {
            text.push_str(&rest[..=open]);
            rest = after;
        }
    }
    text.push_str(rest);
    text.truncate(text.trim_end().len());
    text
}

/// `[lineMs,lengthMs]<offsetMs,lengthMs,0>word…`, each word timed from its line's start.
pub(super) fn krc(text: &str, source: &str) -> Option<Lyrics> {
    tencent(text, source, |body, line_ms| {
        let mut words = Vec::new();
        let mut rest = body;
        while let Some((timing, next)) = rest
            .strip_prefix('<')
            .and_then(|inner| inner.split_once('>'))
        {
            let end = next.find('<').unwrap_or(next.len());
            if let Some((at, length)) = stamp(timing, 3) {
                append(&next[..end], line_ms + at, length, &mut words);
            }
            rest = &next[end..];
        }
        words
    })
}

/// `[lineMs,lengthMs]word(startMs,lengthMs)…`, each word timed from the song's start.
pub(super) fn qrc(text: &str, source: &str) -> Option<Lyrics> {
    tencent(text, source, |body, _| {
        let mut words = Vec::new();
        let mut rest = body;
        // A word's text runs to its stamp; brackets that aren't a stamp, as in "(oh)", are words' own.
        while let Some((text, start, length, after)) = qrc_stamp(rest) {
            if let (Some(start), Some(length)) = (whole(start), whole(length)) {
                append(text, start, length, &mut words);
            }
            rest = after;
        }
        words
    })
}

/// The `LyricContent` of QQ Music's QRC document, read by hand, as an XML parser would fold its lines into one.
pub(super) fn qrc_content(xml: &str) -> Option<String> {
    let (_, content) = xml.split_once("LyricContent=\"")?;
    let (content, _) = content.split_once('"')?;
    Some(unescaped(content).into_owned())
}

/// The text of a KRC download's base64 `content`; `None` unless it unmasks to a zlib stream.
pub(super) fn krc_text(content: &str) -> Option<String> {
    let mut data = STANDARD.decode(content).ok()?;
    if !data.starts_with(b"krc1") {
        return None;
    }
    let masked = &mut data[4..];
    for (byte, key) in masked.iter_mut().zip(KRC_KEY.iter().cycle()) {
        *byte ^= key;
    }
    let mut text = Vec::new();
    ZlibDecoder::new(&*masked)
        .take(KRC_LIMIT as u64 + 1)
        .read_to_end(&mut text)
        .ok()?;
    // The download is the network's to choose, so a small stream can't inflate without bound.
    if text.len() > KRC_LIMIT {
        return None;
    }
    Some(
        String::from_utf8(text)
            .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned()),
    )
}

/// The mask every KuGou client shares.
const KRC_KEY: [u8; 16] = [
    0x40, 0x47, 0x61, 0x77, 0x5E, 0x32, 0x74, 0x47, 0x51, 0x36, 0x31, 0x2D, 0xCE, 0xD2, 0x6E, 0x69,
];

/// Far more than any song's lyrics inflate to.
const KRC_LIMIT: usize = 1 << 20;

struct Row {
    start: f64,
    length: f64,
    line: String,
    words: Vec<Word>,
}

/// The lines with timed words, less the "title - artist" line and credits both sites open with: those are laid
/// end to end from the start, all of one made-up length, as sung lines never are.
fn tencent(text: &str, source: &str, words: impl Fn(&str, f64) -> Vec<Word>) -> Option<Lyrics> {
    let mut rows: Vec<Row> = Vec::new();
    let mut offset = 0.0;
    for line in lines(text) {
        // A positive offset shows the lines sooner, as in LRC.
        if let Some(ms) = offset_tag(line).and_then(whole) {
            offset = ms;
            continue;
        }
        let Some((stamp_text, body)) = line.strip_prefix('[').and_then(|rest| rest.split_once(']'))
        else {
            continue;
        };
        let Some((start, length)) = stamp(stamp_text, 2) else {
            continue;
        };
        let mut found = words(body, start);
        let Some(first) = found.first_mut() else {
            continue;
        };
        let lead = first.text.len() - first.text.trim_start().len();
        first.text.drain(..lead);
        if let Some(last) = found.last_mut() {
            last.text.truncate(last.text.trim_end().len());
        }
        let line = found.iter().map(|word| word.text.as_str()).collect();
        rows.push(Row {
            start,
            length,
            line,
            words: found,
        });
    }
    // Stable, so lines sharing a start keep the file's order.
    rows.sort_by(|a, b| a.start.total_cmp(&b.start));
    if let Some(first) = rows.first()
        && first.start < 1000.0
        && first.line.contains(" - ")
    {
        let length = first.length;
        let credits = 1 + rows
            .windows(2)
            .take_while(|pair| {
                (pair[1].start - pair[0].start - pair[0].length).abs() <= 20.0
                    && (pair[1].length - length).abs() <= 20.0
            })
            .count();
        rows.drain(..credits);
    }
    if rows.is_empty() {
        return None;
    }
    let seconds = |ms: f64| (ms - offset).max(0.0) / 1000.0;
    let mut lyrics = Lyrics {
        lines: Vec::with_capacity(rows.len()),
        starts: Vec::with_capacity(rows.len()),
        words: Vec::with_capacity(rows.len()),
        source: source.to_owned(),
        ..Lyrics::default()
    };
    for mut row in rows {
        for word in &mut row.words {
            word.start = seconds(word.start);
            word.end = seconds(word.end);
        }
        lyrics.lines.push(row.line);
        lyrics.starts.push(seconds(row.start));
        lyrics.words.push(row.words);
    }
    Some(lyrics)
}

/// The first two of exactly `count` comma-separated whole numbers, empty fields skipped as Swift's `split` does.
fn stamp(text: &str, count: usize) -> Option<(f64, f64)> {
    let mut fields = text.split(',').filter(|field| !field.is_empty());
    let (start, length) = (fields.next()?, fields.next()?);
    if fields.count() + 2 != count {
        return None;
    }
    Some((whole(start)?, whole(length)?))
}

/// Times are whole milliseconds, read as integers so "inf", "nan" or hex can't get through.
fn whole(ms: &str) -> Option<f64> {
    ms.parse::<i64>().ok().map(|ms| ms as f64)
}

/// The first `(start,length)` stamp: the text before it, its two numbers, and the text after.
fn qrc_stamp(text: &str) -> Option<(&str, &str, &str, &str)> {
    text.match_indices('(').find_map(|(open, _)| {
        let (start, rest) = leading_digits(&text[open + 1..])?;
        let (length, rest) = leading_digits(rest.strip_prefix(',')?)?;
        Some((&text[..open], start, length, rest.strip_prefix(')')?))
    })
}

/// A non-empty run of ASCII digits and the text after it.
fn leading_digits(text: &str) -> Option<(&str, &str)> {
    let end = text
        .bytes()
        .position(|b| !b.is_ascii_digit())
        .unwrap_or(text.len());
    (end > 0).then(|| text.split_at(end))
}

/// A space timed alone joins the word before, as a line's text is its words joined. Times stay in milliseconds
/// for `tencent` to convert.
fn append(text: &str, start: f64, length: f64, words: &mut Vec<Word>) {
    let text = unescaped(text);
    if text.chars().all(char::is_whitespace) {
        if let Some(last) = words.last_mut() {
            last.text.push_str(&text);
        }
    } else {
        words.push(Word {
            text: text.into_owned(),
            start,
            end: start + length,
        });
    }
}

/// XML's five entities; none decodes to an `&`-led name, so one pass matches replacing them in turn.
fn unescaped(text: &str) -> Cow<'_, str> {
    const ENTITIES: [(&str, char); 5] = [
        ("&apos;", '\''),
        ("&quot;", '"'),
        ("&lt;", '<'),
        ("&gt;", '>'),
        ("&amp;", '&'),
    ];
    if !text.contains('&') {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let (entity, c) = ENTITIES
            .iter()
            .find(|(entity, _)| rest.starts_with(entity))
            .copied()
            .unwrap_or(("&", '&'));
        out.push(c);
        rest = &rest[entity.len()..];
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// The value of an `[offset:ms]` tag.
fn offset_tag(line: &str) -> Option<&str> {
    line.strip_prefix("[offset:")?.split(']').next()
}

/// The lines after any byte order mark, split at every newline Swift's `isNewline` knows, empty ones dropped.
fn lines(text: &str) -> impl Iterator<Item = &str> {
    text.strip_prefix('\u{FEFF}')
        .unwrap_or(text)
        .split([
            '\n', '\r', '\u{0B}', '\u{0C}', '\u{85}', '\u{2028}', '\u{2029}',
        ])
        .filter(|line| !line.is_empty())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use flate2::Compression;
    use flate2::write::ZlibEncoder;

    use super::*;

    fn word(text: &str, start: f64, end: f64) -> Word {
        Word {
            text: text.to_owned(),
            start,
            end,
        }
    }

    fn owned(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|&line| line.to_owned()).collect()
    }

    /// lrc.red's document shape, with dummy words.
    fn document(body: &str, head: &str, timing: &str) -> Option<Lyrics> {
        ttml(
            &format!(
                r#"<tt xmlns="http://www.w3.org/ns/ttml" xmlns:lrc="http://lrc.red/lyric-ttml-internal" xmlns:ttm="http://www.w3.org/ns/ttml#metadata" lrc:timing="{timing}" xml:lang="en"><head><metadata><ttm:agent type="person" xml:id="v1"/><sourceMetadata xmlns="http://lrc.red/lyric-ttml-internal" leadingSilence="0">{head}</sourceMetadata></metadata></head><body dur="1:00.000"><div begin="1.000" end="9.000" lrc:songPart="Verse">
{body}
</div></body></tt>"#
            ),
            "Source: Apple Music",
        )
    }

    #[test]
    fn reads_words_with_syllables_and_the_spaces_between() {
        let read = document(
            r#"<p begin="1.200" end="3.000" lrc:key="L1" ttm:agent="v1"><span begin="1.000" end="1.500">Lorem</span> <span begin="1.500" end="1.750">ip</span><span begin="1.750" end="2.000">sum</span> <span begin="2.000" end="2.500">dolor</span>, <span begin="2.500" end="3.000">sit</span></p>
<p begin="4.000" end="5.000" lrc:key="L2" ttm:agent="v1"><span begin="4.000" end="5.000">Amet</span></p>"#,
            "",
            "Word",
        )
        .unwrap();
        let words = vec![
            vec![
                word("Lorem ", 1.0, 1.5),
                word("ip", 1.5, 1.75),
                word("sum ", 1.75, 2.0),
                word("dolor, ", 2.0, 2.5),
                word("sit", 2.5, 3.0),
            ],
            vec![word("Amet", 4.0, 5.0)],
        ];
        let expected = Lyrics {
            lines: owned(&["Lorem ipsum dolor, sit", "Amet"]),
            starts: vec![1.0, 4.0],
            words,
            source: "Source: Apple Music".to_owned(),
            ..Lyrics::default()
        };
        assert_eq!(read, expected);
        assert!(read.is_word_synced());
    }

    #[test]
    fn reads_the_heads_first_translation_and_romanisation_by_line() {
        let head = r#"<translations><translation type="replacement" xml:lang="en-US"><text for="L1"><span begin="1.000" end="2.000" xmlns="http://www.w3.org/ns/ttml">Other</span></text><text for="L2">Same</text></translation><translation xml:lang="fr"><text for="L1">Autre</text><text for="L3">Encore</text></translation></translations><transliterations><transliteration xml:lang="ko-Latn"><text for="L1"><span begin="1.000" end="2.000" xmlns="http://www.w3.org/ns/ttml">Roman</span> <span ttm:role="x-bg" xmlns="http://www.w3.org/ns/ttml"><span begin="2.000" end="2.500">(echo)</span></span></text></transliteration></transliterations>"#;
        let body = r#"<p begin="1.000" end="2.000" lrc:key="L1"><span begin="1.000" end="2.000">Lorem</span></p>
<p begin="3.000" end="4.000" lrc:key="L2">Same</p>
<p begin="5.000" end="6.000" lrc:key="L3">Ipsum</p>"#;
        let read = document(body, head, "Word").unwrap();
        assert_eq!(read.lines, ["Lorem", "Same", "Ipsum"]);
        assert_eq!(read.words[0], [word("Lorem", 1.0, 2.0)]);
        assert_eq!(
            read.translations,
            ["Other", "", ""],
            "the same line is left out"
        );
        assert_eq!(read.romanised, ["Roman (echo)", "", ""]);
        let plain = document(body, "", "Word").unwrap();
        assert!(plain.translations.is_empty() && plain.romanised.is_empty());
    }

    #[test]
    fn keeps_background_vocals_inline_and_takes_translated_spans_aside() {
        let body = r#"<p begin="1.000" end="2.500"><span begin="1.000" end="1.500">Lorem</span> <span ttm:role="x-bg"><span begin="1.400" end="2.000">(Ip-</span><span begin="2.000" end="2.500">sum)</span></span><span ttm:role="x-translation" xml:lang="fr"><span begin="1.000" end="2.000">Autre</span></span><span ttm:role="x-roman">Roma</span></p>"#;
        let head = r#"<translations><translation xml:lang="fr"><text for="L1">Tête</text></translation></translations>"#;
        let read = document(&body.replace("<p ", r#"<p lrc:key="L1" "#), head, "Word").unwrap();
        assert_eq!(read.lines, ["Lorem (Ip-sum)"]);
        assert_eq!(
            read.words,
            [[
                word("Lorem ", 1.0, 1.5),
                word("(Ip-", 1.4, 2.0),
                word("sum)", 2.0, 2.5)
            ]]
        );
        assert_eq!(
            read.translations,
            ["Autre"],
            "the line's own over the head's"
        );
        assert_eq!(read.romanised, ["Roma"]);
    }

    #[test]
    fn reads_line_timed_documents() {
        let body = r#"<p begin="1.000" end="2.000">Lorem ipsum</p>
<p begin="00:00:03.500" end="4.000">
    Dolor,
    sit &amp; amet
</p>"#;
        let read = document(body, "", "Line").unwrap();
        let expected = Lyrics {
            lines: owned(&["Lorem ipsum", "Dolor, sit & amet"]),
            starts: vec![1.0, 3.5],
            source: "Source: Apple Music".to_owned(),
            ..Lyrics::default()
        };
        assert_eq!(read, expected);
        assert!(read.is_synced());
        let untimed = document("<p>Lorem</p><p>Ipsum</p>", "", "None").unwrap();
        assert!(!untimed.is_synced(), "unsynced without any times");
    }

    #[test]
    fn matches_prefixed_names_and_keeps_lines_without_words() {
        let read = ttml(
            r#"<tt:tt xmlns:tt="http://www.w3.org/ns/ttml" xmlns:itunes="http://music.apple.com/lyric-ttml-internal" itunes:timing="Word"><tt:head/><tt:body><tt:div>
  <tt:p begin="1.000" end="2.000"><tt:span begin="1.000" end="1.500">Lorem</tt:span>
    <tt:span begin="1.500" end="2.000">ipsum</tt:span>
  </tt:p>
  <tt:p begin="3.000" end="4.000">Dolor</tt:p>
</tt:div></tt:body></tt:tt>"#,
            "",
        )
        .unwrap();
        assert_eq!(read.lines, ["Lorem ipsum", "Dolor"]);
        assert_eq!(
            read.words,
            [
                vec![word("Lorem ", 1.0, 1.5), word("ipsum", 1.5, 2.0)],
                vec![]
            ]
        );
        assert!(read.is_word_synced());
    }

    #[test]
    fn reads_clock_values() {
        assert_eq!(seconds("27.395"), Some(27.395));
        assert_eq!(seconds("1:05.25"), Some(65.25));
        assert_eq!(seconds("1:02:03.5"), Some(3723.5));
        assert_eq!(seconds("00:00:00.458"), Some(0.458));
        assert_eq!(seconds("12.5s"), Some(12.5));
        assert_eq!(seconds("500ms"), Some(0.5));
        assert_eq!(seconds(""), None);
        assert_eq!(seconds("soon"), None);
        assert_eq!(seconds("1:02:03:04"), None);
    }

    #[test]
    fn malformed_or_empty_is_none() {
        assert_eq!(
            ttml(r#"<tt><body><p begin="1.000">Lorem</body></tt>"#, ""),
            None
        );
        assert_eq!(ttml("{}", ""), None);
        assert_eq!(document("", "", "Word"), None);
    }

    #[test]
    fn reads_every_stamp_of_a_line() {
        let lyrics = lrc(
            "[00:01.50]One\n[00:03.00] [00:05.250]Chorus\n[00:04.00][Two]",
            "S",
        );
        let expected = Lyrics {
            lines: owned(&["One", "Chorus", "[Two]", "Chorus"]),
            starts: vec![1.5, 3.0, 4.0, 5.25],
            source: "S".to_owned(),
            ..Lyrics::default()
        };
        assert_eq!(lyrics, Some(expected));
    }

    #[test]
    fn skips_metadata_and_honours_the_offset() {
        let text = "[ar:Someone]\r\n[ti:Something]\r\n[length:03:19.13]\r\n[offset:+500]\r\n[00:02.00]One\r\n[01:00.00]Two\r\n";
        let expected = Lyrics {
            lines: owned(&["One", "Two"]),
            starts: vec![1.5, 59.5],
            ..Lyrics::default()
        };
        assert_eq!(lrc(text, ""), Some(expected));
        assert_eq!(
            lrc("\u{FEFF}[00:01.00]One", "").unwrap().lines,
            ["One"],
            "a byte order mark"
        );
    }

    #[test]
    fn reads_stamps_as_their_literals() {
        let lyrics = lrc("[00:13.13]Alpha\n[00:16.56]\n[00:27.16]Beta", "").unwrap();
        assert_eq!(lyrics.starts, [13.13, 16.56, 27.16]);
    }

    #[test]
    fn drops_word_stamps() {
        let lyrics = lrc(
            "[00:01.00]<00:01.00>Alpha <00:01.40>beta<00:02.00>\n[00:02.00] <00:02.00> Gamma <00:02.50> delta\n[00:03.00]a <3 b",
            "",
        )
        .unwrap();
        assert_eq!(lyrics.lines, ["Alpha beta", "Gamma delta", "a <3 b"]);
        assert!(lyrics.words.is_empty());
    }

    #[test]
    fn sorts_by_start_keeping_the_files_order() {
        let lyrics = lrc("[00:05.00]Late\n[00:01.00]Early\n[00:05.00]Also late", "");
        let expected = Lyrics {
            lines: owned(&["Early", "Late", "Also late"]),
            starts: vec![1.0, 5.0, 5.0],
            ..Lyrics::default()
        };
        assert_eq!(lyrics, Some(expected));
    }

    #[test]
    fn a_bare_stamp_is_a_pause() {
        let lyrics = lrc("[00:01.00]One\n[00:04.00]\n[00:09.00]  \n", "").unwrap();
        assert_eq!(lyrics.lines, ["One", "♪", "♪"]);
    }

    #[test]
    fn too_many_lines_is_none() {
        let stamps = "[00:01.00]".repeat(MAX_LINES + 1);
        assert_eq!(lrc(&format!("{stamps}Chorus"), ""), None);
    }

    #[test]
    fn unstamped_text_is_none() {
        assert_eq!(lrc("", ""), None);
        assert_eq!(
            lrc("[ar:Someone]\nplain words\n[0:xx]not a stamp", ""),
            None
        );
    }

    /// Made-up lyrics, compressed and masked by a script apart from the decoder: a title line and a credit, laid
    /// end to end with one length as KuGou lays them, then two sung lines.
    const DOWNLOAD: &str = concat!(
        "a3JjMTjbGsglTQAO96N6RKC8yyswOT8Nch0jMzMnZe1/ekra+ENF48GkItsLVFQtrMmGcdpdebOEKpzfSy7ZNRbzWrLAQ0H1VIh+cdcwQQN8q9I1",
        "kWdil/iq9xLV0SaI2olHscA/dxfP0oibhcqNeV6gbfkJAWe/6AiSUzUSGw0XSvoz0k/QpZYmmEVKdSHu0h6nRREMTb9xhFpByDADZcl2fAia9Pgs",
        "zr4O/TUTHjrOGZMl7w==",
    );

    fn sung() -> Lyrics {
        Lyrics {
            lines: owned(&["Alpha beta it's", "晴天"]),
            starts: vec![10.0, 12.0],
            words: vec![
                vec![
                    word("Alpha ", 10.0, 10.5),
                    word("beta ", 10.5, 11.0),
                    word("it's", 11.25, 12.0),
                ],
                vec![word("晴", 12.0, 12.5), word("天", 12.5, 13.0)],
            ],
            source: "S".to_owned(),
            ..Lyrics::default()
        }
    }

    #[test]
    fn reads_kugous_lines_word_by_word() {
        let text = krc_text(DOWNLOAD).unwrap();
        assert_eq!(krc(&text, "S"), Some(sung()));
    }

    #[test]
    fn reads_qqs_lines_word_by_word() {
        let xml = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
            <QrcInfos><LyricInfo LyricCount=\"1\"><Lyric_1 LyricType=\"1\" LyricContent=\"[ti:Dummy Song]\n\
            [offset:0]\n\
            [0,1500]Dummy Song(0,500) - (500,500)Dummy Artist(1000,500)\n\
            [1500,1500]Words: (1500,700)Someone(2200,800)\n\
            [10000,2000]Alpha (10000,500)beta(10500,500) (11000,250)it&apos;s(11250,750)\n\
            [12000,1000]晴(12000,500)天(12500,500)\n\
            \"/></LyricInfo></QrcInfos>";
        let text = qrc_content(xml).unwrap();
        assert_eq!(qrc(&text, "S"), Some(sung()));
    }

    #[test]
    fn honours_the_offset() {
        let lyrics = krc("[offset:500]\n[1000,500]<0,500,0>One", "");
        let expected = Lyrics {
            lines: owned(&["One"]),
            starts: vec![0.5],
            words: vec![vec![word("One", 0.5, 1.0)]],
            ..Lyrics::default()
        };
        assert_eq!(lyrics, Some(expected));
    }

    #[test]
    fn keeps_sung_lines_that_look_like_credits() {
        let duet = krc(
            "[1000,500]<0,500,0>男：<500,500,0>One\n[2000,500]<0,500,0>合：<500,500,0>Two",
            "",
        )
        .unwrap();
        assert_eq!(duet.lines, ["男：One", "合：Two"]);
        let later = krc("[9000,500]<0,500,0>Now - then", "").unwrap();
        assert_eq!(later.lines, ["Now - then"], "a title-like line sung later");
        let title = qrc("[0,900]Me - you(0,900)\n[950,2000]Sung(950,2000)", "").unwrap();
        assert_eq!(title.lines, ["Sung"], "the title alone");
    }

    #[test]
    fn anything_else_is_none() {
        assert_eq!(krc_text("not base64"), None);
        assert_eq!(krc_text(&STANDARD.encode("krc2abcdef")), None);
        assert_eq!(
            krc_text(&STANDARD.encode("krc1abcdef")),
            None,
            "not a zlib stream once unmasked"
        );
        assert_eq!(
            krc(
                "[ti:Song]\n[0,500]<0,500,0>Song - Band\n[500,500]<0,500,0>Lyrics by: Someone\n[2000,500]",
                ""
            ),
            None
        );
        assert_eq!(qrc_content("<QrcInfos/>"), None);
        assert_eq!(
            krc("[inf,500]<0,500,0>One\n[1000,500]<nan,500,0>Two", ""),
            None,
            "times that aren't whole numbers"
        );
    }

    #[test]
    fn caps_the_inflated_size() {
        let download = |size| {
            let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(&vec![b'a'; size]).unwrap();
            let stream = encoder.finish().unwrap();
            let mut data = b"krc1".to_vec();
            data.extend(
                stream
                    .iter()
                    .zip(KRC_KEY.iter().cycle())
                    .map(|(byte, key)| byte ^ key),
            );
            STANDARD.encode(data)
        };
        assert_eq!(
            krc_text(&download(KRC_LIMIT)).map(|text| text.len()),
            Some(KRC_LIMIT)
        );
        assert_eq!(krc_text(&download(KRC_LIMIT + 1)), None);
    }
}
