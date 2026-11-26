//! YouTube's lyrics, or another site's better ones that agree with them:
//! word timing first, then timed lines, as the iOS app picks them.

mod formats;
pub mod hub;
mod matching;
mod sites;
mod youtube;

use std::{ops::Range, time::Duration};

pub(super) use youtube::timed;

use super::Track;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Lyrics {
    pub lines: Vec<String>,
    /// Each line's start in seconds when synced, else empty.
    pub starts: Vec<f64>,
    /// Each line's end where every line has one, else empty.
    pub ends: Vec<f64>,
    /// Each line's words when sung word by word, else empty; a line may still
    /// have none.
    pub words: Vec<Vec<Word>>,
    /// Each line's translation, `""` where it has none; empty without any.
    pub translations: Vec<String>,
    /// Each line in Latin letters, likewise.
    pub romanised: Vec<String>,
    /// Where they come from, such as "Source: Musixmatch".
    pub source: String,
}

/// A word or syllable as sung, its text keeping any space after it.
#[derive(Debug, Clone, PartialEq)]
pub struct Word {
    pub text: String,
    pub start: f64,
    pub end: f64,
}

impl Lyrics {
    /// Keeps the blank lines between verses.
    pub fn plain(body: &str, source: String) -> Self {
        Self {
            lines: body.lines().map(str::to_owned).collect(),
            source,
            ..Self::default()
        }
    }

    pub fn is_synced(&self) -> bool {
        !self.starts.is_empty() && self.starts.len() == self.lines.len()
    }

    pub fn is_word_synced(&self) -> bool {
        self.is_synced() && self.words.len() == self.lines.len()
    }

    /// Sung spans, sorted and merged: word timing where present, else each
    /// line to its end, at most `LINE_SECS` and never past the next; `None`
    /// if untimed.
    pub fn sung(&self) -> Option<Vec<Range<f64>>> {
        if !self.is_synced() {
            return None;
        }
        let ends = (self.ends.len() == self.lines.len()).then_some(&self.ends);
        let mut spans = Vec::new();
        for (i, (line, &start)) in self.lines.iter().zip(&self.starts).enumerate() {
            match self.words.get(i).filter(|w| !w.is_empty()) {
                Some(words) => spans.extend(words.iter().map(|w| w.start..w.end)),
                // A blank or "♪" line is a break, which still ends the one before.
                None if line.chars().any(char::is_alphanumeric) => {
                    let next = self.starts.get(i + 1).copied().unwrap_or(f64::INFINITY);
                    // YouTube's ends often run on to the next line, across a break.
                    let end = ends
                        .map(|ends| ends[i])
                        .filter(|&end| end > start)
                        .unwrap_or(f64::INFINITY)
                        .min(start + LINE_SECS)
                        .min(next);
                    spans.push(start..end);
                }
                None => {}
            }
        }
        spans.retain(|s| s.start.is_finite() && s.end.is_finite() && s.start < s.end);
        spans.sort_unstable_by(|a, b| a.start.total_cmp(&b.start));
        spans.dedup_by(|later, kept| {
            let joins = later.start - kept.end < SUNG_GAP_SECS;
            if joins {
                kept.end = kept.end.max(later.end);
            }
            joins
        });
        (!spans.is_empty()).then_some(spans)
    }
}

/// How long a line counts as sung at most, in seconds.
const LINE_SECS: f64 = 5.0;
/// Sung spans closer than this are one, as a breath isn't a gap.
const SUNG_GAP_SECS: f64 = 0.6;

/// `own`'s lyrics (YouTube's), or another site's that agree with them. Only
/// YouTube's failure fails, so the song is asked again rather than shown
/// unchecked.
pub(super) async fn find(
    track: &Track,
    own: impl Future<Output = super::Result<Option<Lyrics>>>,
) -> super::Result<Option<Lyrics>> {
    // A music video's timing is its own, which other sites' songs needn't share.
    if track.video {
        return own.await;
    }
    let sites = async { Ok::<_, String>(other_sites(track).await) };
    let (own, found) = tokio::try_join!(own, sites)?;
    let reference = own.as_ref().map(matching::Reference::new);
    let agree = |lyrics: &Lyrics| checked(lyrics, reference.as_ref());
    let apple: Vec<_> = found.apple.into_iter().filter(agree).collect();
    let mut others: Vec<_> = found.others.into_iter().filter(agree).collect();
    let mut agreed = apple.iter().chain(&others);
    // KuGou is slow and often has another version, so it's asked only for word
    // timing the rest lack, where YouTube's lyrics can vouch for it.
    let wants_words = own.is_some() && !agreed.clone().any(Lyrics::is_word_synced);
    // LRCLIB only for timing YouTube's lyrics lack, unless the hub asked it.
    let wants_lines = !own.as_ref().is_some_and(Lyrics::is_synced)
        && !found.asked_lrclib
        && !agreed.any(Lyrics::is_synced);
    let (kugou, lrclib) = tokio::join!(
        within(wants_words, Duration::from_secs(4), sites::kugou(track)),
        within(wants_lines, Duration::from_secs(7), sites::lrclib(track)),
    );
    others.extend([kugou, lrclib].into_iter().flatten().filter(agree));
    Ok(chosen(own, apple, others))
}

/// The hub's lyrics, and lrc.red's when the hub has no Apple Music word
/// timing and didn't hear from lrc.red; both run alongside YouTube's.
async fn other_sites(track: &Track) -> hub::Found {
    let mut found = hub::lyrics(track).await.unwrap_or_default();
    let wanted = !found.asked_lrc_red && !found.apple.iter().any(Lyrics::is_word_synced);
    if let Some(lyrics) = within(wanted, Duration::from_secs(6), sites::lrc_red(track)).await {
        found.apple.push(lyrics);
    }
    found
}

async fn within<T>(
    wanted: bool,
    deadline: Duration,
    fetch: impl Future<Output = Option<T>>,
) -> Option<T> {
    if !wanted {
        return None;
    }
    tokio::time::timeout(deadline, fetch).await.ok().flatten()
}

/// Another site's lyrics agree with YouTube's, always the recording playing,
/// in mostly the same words in time, or stand where YouTube has none.
fn checked(other: &Lyrics, own: Option<&matching::Reference>) -> bool {
    other.lines.len() <= formats::MAX_LINES
        && other.words.iter().map(Vec::len).sum::<usize>() <= formats::MAX_WORDS
        && own.is_none_or(|own| own.agrees(other))
}

/// Word timing, Apple Music's kind first; then timed lines, Apple Music's
/// kind, YouTube's, then the others'; then YouTube's text, or any. The other
/// sites' have passed `checked`, and come best first.
fn chosen(own: Option<Lyrics>, apple: Vec<Lyrics>, others: Vec<Lyrics>) -> Option<Lyrics> {
    let split = apple.len();
    let mut sites = apple;
    sites.extend(others);
    let pick = sites.iter().position(Lyrics::is_word_synced).or_else(|| {
        let timed = |range: &[Lyrics]| range.iter().position(Lyrics::is_synced);
        timed(&sites[..split]).or_else(|| {
            if own.as_ref().is_some_and(Lyrics::is_synced) {
                None
            } else {
                timed(&sites[split..]).map(|i| split + i)
            }
        })
    });
    match pick {
        Some(i) => Some(sites.swap_remove(i)),
        None if own.is_some() => own,
        // Any other site's before Apple Music's kind, which are untimed here.
        None if sites.len() > split => Some(sites.swap_remove(split)),
        None => sites.into_iter().next(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lyrics(starts: &[f64], words: bool, source: &str) -> Lyrics {
        Lyrics {
            lines: vec!["Alpha".into(), "Beta".into()],
            starts: starts.to_vec(),
            words: if words {
                vec![
                    vec![Word {
                        text: "Alpha".into(),
                        start: 1.0,
                        end: 2.0,
                    }],
                    vec![Word {
                        text: "Beta".into(),
                        start: 3.0,
                        end: 4.0,
                    }],
                ]
            } else {
                Vec::new()
            },
            source: source.into(),
            ..Lyrics::default()
        }
    }

    fn timed_lines(lines: &[&str], starts: &[f64], words: Vec<Vec<Word>>) -> Lyrics {
        Lyrics {
            lines: lines.iter().map(|&l| l.into()).collect(),
            starts: starts.to_vec(),
            words,
            ..Lyrics::default()
        }
    }

    fn words(spans: &[(f64, f64)]) -> Vec<Word> {
        spans
            .iter()
            .map(|&(start, end)| Word {
                text: "la ".into(),
                start,
                end,
            })
            .collect()
    }

    #[test]
    fn sung_by_words_joins_those_a_breath_apart() {
        let lyrics = timed_lines(
            &["One", "Two", "Three"],
            &[1.0, 5.0, 20.0],
            vec![
                words(&[(1.0, 1.5), (1.9, 2.4), (3.1, 3.5)]),
                // Out of order, overlapping, and one ending before it starts.
                words(&[(6.0, 7.0), (5.0, 6.5), (9.0, 8.0)]),
                Vec::new(),
            ],
        );
        assert_eq!(
            lyrics.sung(),
            Some(vec![1.0..2.4, 3.1..3.5, 5.0..7.0, 20.0..25.0]),
            "a line without words is sung as a line"
        );
    }

    #[test]
    fn sung_by_lines_runs_to_the_next_at_most_a_while() {
        let lyrics = timed_lines(
            &["One", "♪", "Two", "Three"],
            &[0.0, 10.0, 12.0, 13.0],
            Vec::new(),
        );
        assert_eq!(lyrics.sung(), Some(vec![0.0..5.0, 12.0..18.0]));
    }

    #[test]
    fn sung_by_lines_ends_where_they_end() {
        let lyrics = Lyrics {
            // Shorter than the cap, longer, past the next line, and not after
            // its start.
            ends: vec![3.0, 11.0, 20.0, 40.0, 31.0, 34.0],
            ..timed_lines(
                &["One", "♪", "Two", "Three", "♪", "Four"],
                &[0.0, 10.0, 12.0, 22.0, 25.0, 35.0],
                Vec::new(),
            )
        };
        assert_eq!(
            lyrics.sung(),
            Some(vec![0.0..3.0, 12.0..17.0, 22.0..25.0, 35.0..40.0])
        );
    }

    #[test]
    fn sung_is_unknown_without_timing() {
        assert_eq!(Lyrics::plain("One\nTwo", String::new()).sung(), None);
        assert_eq!(Lyrics::default().sung(), None);
        assert_eq!(
            timed_lines(&["", "♪"], &[0.0, 4.0], Vec::new()).sung(),
            None
        );
    }

    #[test]
    fn chooses_other_sites_only_where_they_agree_with_youtube() {
        let youtube = lyrics(&[1.0, 3.0], false, "");
        let plain = lyrics(&[], false, "");
        let apple = lyrics(&[1.0, 3.0], true, "Source: lrc.red");
        let late = lyrics(&[5.0, 7.0], true, "Source: lrc.red");
        let other = Lyrics {
            lines: vec!["Gamma".into(), "Delta".into()],
            ..lyrics(&[1.0, 3.0], false, "Source: LRCLIB")
        };
        let community = lyrics(&[1.2, 3.2], false, "Source: LRCLIB");
        let lines = lyrics(&[1.1, 3.1], false, "Source: lrc.red");
        let kugou = lyrics(&[1.3, 3.3], true, "Source: KuGou");
        let pick = |own: &Lyrics, apple: &[&Lyrics], others: &[&Lyrics]| {
            let reference = matching::Reference::new(own);
            let keep = |list: &[&Lyrics]| {
                list.iter()
                    .filter(|l| checked(l, Some(&reference)))
                    .map(|&l| l.clone())
                    .collect()
            };
            chosen(Some(own.clone()), keep(apple), keep(others))
        };
        assert_eq!(pick(&youtube, &[&apple], &[&kugou]), Some(apple.clone()));
        assert_eq!(
            pick(&youtube, &[&late], &[]),
            Some(youtube.clone()),
            "out of time"
        );
        assert_eq!(
            pick(&youtube, &[&plain], &[]),
            Some(youtube.clone()),
            "untimed never beats timed"
        );
        assert_eq!(
            pick(&youtube, &[&lines], &[&kugou]),
            Some(kugou.clone()),
            "words beat lines"
        );
        assert_eq!(pick(&youtube, &[&lines], &[]), Some(lines.clone()));
        assert_eq!(
            pick(&plain, &[], &[&kugou]),
            Some(kugou.clone()),
            "timing YouTube's words lack"
        );
        assert_eq!(
            pick(&plain, &[], &[&other]),
            Some(plain.clone()),
            "another song's"
        );
        assert_eq!(pick(&plain, &[], &[&community]), Some(community.clone()));
        assert_eq!(
            pick(&plain, &[], &[&plain]),
            Some(plain.clone()),
            "untimed adds nothing"
        );
        assert_eq!(
            chosen(None, vec![apple.clone()], vec![community]),
            Some(apple)
        );
        assert_eq!(
            chosen(None, Vec::new(), vec![plain.clone()]),
            Some(plain),
            "better than none"
        );
        assert_eq!(chosen(None, Vec::new(), Vec::new()), None);
    }
}
