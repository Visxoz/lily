//! Automix: plans a blend over where the outgoing track winds down or the
//! incoming one builds, on the beat where their tempos agree (tempo is never
//! stretched), and trades their bass partway so only one plays.

use std::{f32::consts::TAU, ops::RangeInclusive};

/// The current track's end held back and analysed: room for the longest
/// blend and the music before it, which sets its loud level.
pub(in crate::audio) const TAIL_SECS: f64 = 20.0;
/// How much of the next track opens ahead, for its build and its beats.
pub(super) const INTRO_SECS: f64 = MAX_SECS;
/// The shortest and longest blends, in seconds.
const MIN_SECS: f64 = 3.0;
const MAX_SECS: f64 = 12.0;
/// Analysis steps a second: 2.5 ms, so two tracks' beats, each placed to a
/// step, line up within 5 ms, short of a flam.
const HOPS_PER_SEC: u32 = 400;
/// Loudness is judged over half a second, so a lone hit or gap isn't.
const SMOOTH_HOPS: usize = HOPS_PER_SEC as usize / 2;
/// An onset is the energy of the last 10 ms rising over the loudest 10 ms of
/// the 80 ms before, so a kick stands out of the bass held under it, while a
/// held note's ripple, which peaks every waveform, never tops its last peak.
const RECENT_HOPS: usize = HOPS_PER_SEC as usize / 100;
const BEFORE_HOPS: usize = HOPS_PER_SEC as usize * 8 / 100;
/// The least rise that counts, in bels (3 dB), below which noise wavers.
const MIN_RISE: f32 = 0.3;
/// The most a rise counts for, in bels, so music entering out of a quiet
/// stretch doesn't outweigh every beat.
const MAX_RISE: f32 = 1.0;
/// How far under its loudest an intro is still building, in dB: half the
/// amplitude.
const QUIET_DB: f32 = 6.0;
/// How much a fade-out falls a second, at least, in dB; a level that holds
/// is the music, however soft.
const FADE_DB_PER_SEC: f32 = 1.0;
/// How long a level must hold to count as held, in seconds, so a slow fade's
/// wavering over a second or two doesn't.
const HELD_SECS: usize = 5;
/// How much quieter than a later second a second may be within a fade, in dB;
/// any more, and the quiet bars between loud ones are the music's own.
const SWELL_DB: f32 = 3.0;
/// How far under the loud level a held level is a noise floor a fade ends
/// in, as tape hiss, rather than the music, in dB.
const HISS_DB: f32 = 30.0;
/// How far under the loud level a fade-out ends, at least, in dB; a track
/// ending above it stops, however it got there.
const FADED_DB: f32 = 20.0;
/// The tempos searched, and the likeliest, which settles half or double time.
const MIN_BPM: f64 = 70.0;
const MAX_BPM: f64 = 180.0;
const LIKELY_BPM: f64 = 120.0;
/// How regular the onsets must be to count as a beat: their correlation
/// one beat apart, from 0 for noise to 1 for a metronome.
const MIN_PULSE: f64 = 0.3;
/// How far onsets are spread either side, in hops, for finding the tempo, so
/// hits whose attack is drawn out or played a little off still line up.
const SPREAD_HOPS: usize = 4;
/// How far either side of the autocorrelation's peak the period is sought,
/// in hops, as spread hits put the peak off the true lag.
const SEARCH_HOPS: i32 = 3;
/// How far either side of the whole-hop lag's anchor each finer period's
/// anchor is sought, in hops.
const ANCHOR_HOPS: i32 = 8;
/// The stretches, in seconds, each of which must keep to the grid found.
const STRETCH_SECS: f64 = 4.0;
/// How far either side of a beat its onsets are taken as its own, in hops.
const CENTRE_HOPS: f64 = 6.0;
/// How far a stretch's beats may fall from the grid's on average, in hops;
/// any further, the period found is off, or the tempo wanders.
const STEADY_HOPS: f64 = 4.0;
/// How many times more of a stretch's onset must lie near its beats than an
/// even spread would leave there.
const MIN_CLARITY: f64 = 5.5;
/// How much of the onset the stretches keeping to the grid must carry, of
/// all those carrying onset.
const KEPT_SHARE: f64 = 2.0 / 3.0;
/// The fewest beats that must sound for a grid to count.
const MIN_BEATS: usize = 6;
/// The furthest the two tracks' beats may drift apart over a blend, in beats.
const MAX_DRIFT: f64 = 0.1;
/// Where the bass splits from the rest, in Hz.
const BASS_HZ: f32 = 160.0;
/// The bass trade's length with no beat to time it, in seconds.
const TRADE_SECS: f64 = 0.5;
/// Energy of -100 dB, which keeps the log of silence finite.
const FLOOR: f32 = 1e-10;
/// Filter memory below this is flushed, as denormal floats are slow.
const DENORMAL: f32 = 1e-20;

/// Two cascaded one-pole low-passes: the bass's loudness, for kicks.
struct Lows {
    coeff: f32,
    a: f32,
    b: f32,
}

impl Lows {
    fn new(rate: u32) -> Self {
        Self {
            coeff: 1.0 - (-TAU * BASS_HZ / rate as f32).exp(),
            a: 0.0,
            b: 0.0,
        }
    }

    fn run(&mut self, x: f32) -> f32 {
        self.a += self.coeff * (x - self.a);
        self.b += self.coeff * (self.a - self.b);
        if self.b.abs() < DENORMAL {
            (self.a, self.b) = (0.0, 0.0);
        }
        self.b
    }
}

/// Cuts the bass: two Butterworth high-passes (RBJ's cookbook) in series,
/// 24 dB an octave, so a kick an octave down is 24 dB under.
#[derive(Clone, Copy)]
struct Cut {
    b0: f64,
    a1: f64,
    a2: f64,
    /// Each stage's transposed direct form II memory.
    z: [[f64; 2]; 2],
}

impl Cut {
    fn new(rate: u32) -> Self {
        let (sin, cos) = (std::f64::consts::TAU * f64::from(BASS_HZ) / f64::from(rate)).sin_cos();
        let alpha = sin * std::f64::consts::FRAC_1_SQRT_2;
        let a0 = 1.0 + alpha;
        Self {
            b0: (1.0 + cos) / 2.0 / a0,
            a1: -2.0 * cos / a0,
            a2: (1.0 - alpha) / a0,
            z: [[0.0; 2]; 2],
        }
    }

    fn run(&mut self, x: f32) -> f32 {
        // A high-pass's b1 is -2 b0 and its b2 is b0.
        let mut x = f64::from(x);
        for z in &mut self.z {
            let y = self.b0 * x + z[0];
            z[0] = -2.0 * self.b0 * x - self.a1 * y + z[1];
            z[1] = self.b0 * x - self.a2 * y;
            x = y;
        }
        // On the memory, as a high-pass's output nears 0 under a steady input
        // while its memory doesn't.
        if self
            .z
            .as_flattened()
            .iter()
            .all(|z| z.abs() < f64::from(DENORMAL))
        {
            self.z = [[0.0; 2]; 2];
        }
        x as f32
    }
}

/// A steady beat, in frames: between beats, and to the first.
#[derive(Debug, Clone, Copy)]
struct Beats {
    period: f64,
    first: f64,
}

/// What a blend is planned on, from one track's samples.
pub(super) struct Analysis {
    frames: usize,
    hop: usize,
    /// Smoothed loudness at each hop, in dB.
    level: Vec<f32>,
    beats: Option<Beats>,
}

pub(super) fn analyse(samples: &[f32], rate: u32, channels: usize) -> Analysis {
    let hop = (rate / HOPS_PER_SEC).max(1) as usize;
    let hops = samples.len() / (hop * channels);
    let mut lows = Lows::new(rate);
    // Running sums of each hop's energy, in f64 so a quiet stretch after a
    // loud one keeps its precision; a stretch's mean is then two lookups.
    let (mut all, mut low) = (vec![0.0f64], vec![0.0f64]);
    all.reserve_exact(hops);
    low.reserve_exact(hops);
    for block in samples.chunks_exact(hop * channels) {
        let (mut a, mut l) = (0.0, 0.0);
        for frame in block.chunks_exact(channels) {
            let x = frame.iter().sum::<f32>() / channels as f32;
            let lo = lows.run(x);
            a += x * x;
            l += lo * lo;
        }
        all.push(all[all.len() - 1] + f64::from(a / hop as f32));
        low.push(low[low.len() - 1] + f64::from(l / hop as f32));
    }
    // The bass's rises for kicks, the whole's for the rest, taken where they
    // step up, as a capped rise stays flat while its hit is recent.
    let mut last = 0.0;
    let onsets: Vec<f32> = (0..hops)
        .map(|i| {
            let r = rise(&all, i) + rise(&low, i);
            let onset = (r - last).max(0.0);
            last = r;
            onset
        })
        .collect();
    Analysis {
        frames: samples.len() / channels,
        hop,
        level: smooth(&all),
        beats: beats(&onsets, f64::from(rate) / hop as f64).map(|b| Beats {
            period: b.period * hop as f64,
            first: b.first * hop as f64,
        }),
    }
}

/// The mean energy of hops `from..to`.
fn mean(sums: &[f64], from: usize, to: usize) -> f64 {
    (sums[to] - sums[from]) / (to - from) as f64
}

/// An energy in bels, floored.
fn bels(energy: f64) -> f32 {
    (energy as f32).max(FLOOR).log10()
}

/// How far hop `i` and those just before it rise over the loudest stretch as
/// long before them, past `MIN_RISE` and up to `MAX_RISE`; none this early in.
fn rise(sums: &[f64], i: usize) -> f32 {
    let Some(before) = (i + 1).checked_sub(RECENT_HOPS + BEFORE_HOPS) else {
        return 0.0;
    };
    let recent = before + BEFORE_HOPS;
    let loudest = (before..=recent - RECENT_HOPS)
        .map(|from| mean(sums, from, from + RECENT_HOPS))
        .fold(0.0, f64::max);
    let rise = bels(mean(sums, recent, i + 1)) - bels(loudest);
    (rise.min(MAX_RISE) - MIN_RISE).max(0.0)
}

/// Each hop's energy averaged over `SMOOTH_HOPS` around it, in dB.
fn smooth(sums: &[f64]) -> Vec<f32> {
    let hops = sums.len() - 1;
    (0..hops)
        .map(|i| {
            let from = i.saturating_sub(SMOOTH_HOPS / 2);
            10.0 * bels(mean(sums, from, (i + SMOOTH_HOPS / 2).min(hops)))
        })
        .collect()
}

/// The beat in `onsets`, in hops: the likeliest tempo's lag, the comb grid
/// near it gathering the most onset, then fitted through its beats' onsets.
/// None unless the pulse is regular and the stretches keep to the grid.
fn beats(onsets: &[f32], hops_per_sec: f64) -> Option<Beats> {
    let n = onsets.len();
    let min_lag = (60.0 * hops_per_sec / MAX_BPM).floor() as usize;
    let max_lag = (60.0 * hops_per_sec / MIN_BPM).ceil() as usize;
    // Four of the slowest beats at least.
    if min_lag < 2 || n < 4 * max_lag {
        return None;
    }
    let blurred = spread(onsets);
    let average = blurred.iter().map(|&o| f64::from(o)).sum::<f64>() / n as f64;
    let d: Vec<f64> = blurred.iter().map(|&o| f64::from(o) - average).collect();
    let power: f64 = d.iter().map(|x| x * x).sum();
    if power <= 0.0 {
        return None;
    }
    // Scaled for the shrinking overlap, so long lags aren't penalised.
    let corr = |lag: usize| {
        let sum: f64 = d[..n - lag].iter().zip(&d[lag..]).map(|(a, b)| a * b).sum();
        sum / power * n as f64 / (n - lag) as f64
    };
    let prior = |lag: usize| {
        let octaves = (60.0 * hops_per_sec / lag as f64 / LIKELY_BPM).log2();
        (-0.5 * octaves * octaves).exp()
    };
    let (lag, pulse) = (min_lag..=max_lag)
        .map(|lag| (lag, corr(lag)))
        .max_by(|a, b| (a.1 * prior(a.0)).total_cmp(&(b.1 * prior(b.0))))?;
    if pulse < MIN_PULSE {
        return None;
    }
    // A whole-hop lag drifts off the beat over the window, so the period is
    // sought to a fiftieth of a hop. Anchored mid-window, the best anchor
    // barely moves with the period, so it is sought only near the lag's.
    let (mid, half) = ((n / 2) as i32, lag as i32 / 2);
    let (_, rough) = best(&blurred, [lag as f64], mid - half..=mid + half)?;
    let periods =
        (-50 * SEARCH_HOPS..=50 * SEARCH_HOPS).map(|step| lag as f64 + f64::from(step) / 50.0);
    let near = rough - ANCHOR_HOPS..=rough + ANCHOR_HOPS;
    let (period, anchor) = best(&blurred, periods, near)?;
    // The blur that lets the comb find the beat also flattens it over small
    // errors, so the grid is then fitted through where each beat's onsets
    // centre.
    let mut grid = (period, f64::from(anchor));
    for _ in 0..2 {
        grid = fit(&centres(onsets, grid))?;
    }
    let (period, anchor) = grid;
    // A few hits alone space out a tempo of their own.
    let teeth = centres(onsets, grid);
    let typical = teeth.iter().map(|t| t.mass).sum::<f64>() / teeth.len() as f64;
    if teeth.iter().filter(|t| t.mass >= typical / 2.0).count() < MIN_BEATS {
        return None;
    }
    // A period a little off, or a tempo that wanders, fits the whole window
    // but leaves some stretch off the grid. A stretch with little onset is
    // skipped; one on the off-beats, as in a breakdown, passes while such
    // stretches carry at most `1 - KEPT_SHARE` of the onset.
    let off_beats = centres(onsets, (period, anchor + period / 2.0));
    let stretches = ((n as f64 / (STRETCH_SECS * hops_per_sec)).round() as usize).max(1);
    let share = onsets.iter().map(|&o| f64::from(o)).sum::<f64>() / (2 * stretches) as f64;
    let (mut kept, mut lost) = (0.0, 0.0);
    for i in 0..stretches {
        let hops = i * n / stretches..(i + 1) * n / stretches;
        let mass: f64 = onsets[hops.clone()].iter().map(|&o| f64::from(o)).sum();
        let keeps = |teeth: &[Tooth], anchor: f64| {
            let (near, off) = teeth
                .iter()
                .filter(|t| hops.contains(&(t.at as usize)))
                .fold((0.0, 0.0), |(near, off), t| {
                    (
                        near + t.mass,
                        off + t.mass * (t.at - (anchor + t.k * period)),
                    )
                });
            near / mass * period / (2.0 * CENTRE_HOPS + 1.0) >= MIN_CLARITY
                && (off / near).abs() <= STEADY_HOPS
        };
        if mass < share {
            continue;
        } else if keeps(&teeth, anchor) {
            kept += mass;
        } else if keeps(&off_beats, anchor + period / 2.0) {
            lost += mass;
        } else {
            return None;
        }
    }
    (kept >= KEPT_SHARE * (kept + lost)).then_some(Beats {
        period,
        first: anchor.rem_euclid(period),
    })
}

/// The period from `periods` and anchor from `anchors` whose grid gathers the
/// most onset.
fn best(
    onsets: &[f32],
    periods: impl IntoIterator<Item = f64>,
    anchors: RangeInclusive<i32>,
) -> Option<(f64, i32)> {
    periods
        .into_iter()
        .flat_map(|period| anchors.clone().map(move |anchor| (period, anchor)))
        .map(|(period, anchor)| (comb(onsets, period, anchor), period, anchor))
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, period, anchor)| (period, anchor))
}

/// The onset gathered by a grid of beats `period` apart through `anchor`.
fn comb(onsets: &[f32], period: f64, anchor: i32) -> f32 {
    let anchor = f64::from(anchor);
    let from = (-anchor / period).ceil();
    (0..)
        .map(|k| (anchor + (from + f64::from(k)) * period).round())
        .take_while(|&t| t < onsets.len() as f64)
        .filter(|&t| t >= 0.0)
        .map(|t| onsets[t as usize])
        .sum()
}

/// One beat of a grid: its number from the anchor, and where the onset
/// within `CENTRE_HOPS` of it centres, and how much there is.
struct Tooth {
    k: f64,
    at: f64,
    mass: f64,
}

/// The beats of a grid `(period, anchor)` that have onset near them.
fn centres(onsets: &[f32], (period, anchor): (f64, f64)) -> Vec<Tooth> {
    let last = onsets.len() as f64 - 1.0;
    let ks = (-anchor / period).ceil() as i64..=((last - anchor) / period).floor() as i64;
    ks.filter_map(|k| {
        let k = k as f64;
        let beat = anchor + k * period;
        let hops = (beat - CENTRE_HOPS).ceil().max(0.0) as usize
            ..=(beat + CENTRE_HOPS).floor().min(last) as usize;
        let (mass, moment) = hops.fold((0.0, 0.0), |(mass, moment), i| {
            let o = f64::from(onsets[i]);
            (mass + o, moment + o * i as f64)
        });
        (mass > 0.0).then(|| Tooth {
            k,
            at: moment / mass,
            mass,
        })
    })
    .collect()
}

/// The grid through `teeth` by least squares, each weighted by its onset:
/// its period and anchor, should two beats or more have onset.
fn fit(teeth: &[Tooth]) -> Option<(f64, f64)> {
    let (w, x, y) = teeth.iter().fold((0.0, 0.0, 0.0), |(w, x, y), t| {
        (w + t.mass, x + t.mass * t.k, y + t.mass * t.at)
    });
    let (x, y) = (x / w, y / w);
    let (cov, var) = teeth.iter().fold((0.0, 0.0), |(cov, var), t| {
        (
            cov + t.mass * (t.k - x) * (t.at - y),
            var + t.mass * (t.k - x).powi(2),
        )
    });
    let period = cov / var;
    (var > 0.0 && period > 0.0).then_some((period, y - period * x))
}

/// `onsets` each spread over `SPREAD_HOPS` either side, tapering.
fn spread(onsets: &[f32]) -> Vec<f32> {
    let n = onsets.len();
    (0..n)
        .map(|i| {
            (i.saturating_sub(SPREAD_HOPS)..(i + SPREAD_HOPS + 1).min(n))
                .map(|j| onsets[j] * (SPREAD_HOPS + 1 - i.abs_diff(j)) as f32)
                .sum()
        })
        .collect()
}

impl Analysis {
    /// The 90th percentile of the level: the track at its loud, not a peak.
    fn loud(&self) -> f32 {
        let mut level = self.level.clone();
        if level.is_empty() {
            return 0.0;
        }
        let i = level.len() * 9 / 10;
        *level.select_nth_unstable_by(i, f32::total_cmp).1
    }

    /// Frames the track fades out over: from an end `FADED_DB` under its loud
    /// level back to where its level last held, or last swelled; 0 for an
    /// ending that stops or holds a softer level to the end.
    fn outro(&self) -> f64 {
        let second = HOPS_PER_SEC as usize;
        // Each second's peak, the last first.
        let peaks: Vec<f32> = self
            .level
            .rchunks_exact(second)
            .map(|s| s.iter().copied().fold(f32::MIN, f32::max))
            .collect();
        let Some(last) = peaks.len().checked_sub(HELD_SECS) else {
            return 0.0;
        };
        let loud = self.loud();
        if self.level.last().is_none_or(|&end| end > loud - FADED_DB) {
            return 0.0;
        }
        let floor = loud - HISS_DB;
        let held = |from: usize| {
            let (low, high) = peaks[from..from + HELD_SECS]
                .iter()
                .fold((f32::MAX, f32::MIN), |(l, h), &p| (l.min(p), h.max(p)));
            high >= floor && high - low <= (HELD_SECS - 1) as f32 * FADE_DB_PER_SEC
        };
        let mut secs = (0..=last)
            .find(|&from| held(from))
            .unwrap_or(peaks.len() - 1);
        // A fade falls on to its end, so it starts after any second quieter
        // than a later one by `SWELL_DB`.
        let mut loudest = f32::MIN;
        for (j, &peak) in peaks.iter().enumerate().take(secs) {
            if peak < loudest - SWELL_DB {
                secs = j;
                break;
            }
            loudest = loudest.max(peak);
        }
        while secs > 0 && peaks[secs] - peaks[0] < secs as f32 * FADE_DB_PER_SEC {
            secs -= 1;
        }
        (secs * second * self.hop) as f64
    }

    /// Frames before the track first sounds near its loudest.
    fn build(&self) -> f64 {
        let quiet = self.level.iter().copied().fold(f32::MIN, f32::max) - QUIET_DB;
        self.level
            .iter()
            .position(|&l| l >= quiet)
            .map_or(self.frames as f64, |i| (i * self.hop) as f64)
    }
}

/// A planned blend over the tail's last `frames`, with the bass traded
/// over `trade_len` frames from `trade_at`.
#[derive(Debug)]
pub(super) struct Mix {
    pub(super) frames: usize,
    trade_at: usize,
    trade_len: usize,
}

/// Blends over the outgoing track's fade-out or the incoming one's build,
/// whichever is longer, ending as the outgoing ends, and over at most half
/// an incoming track shorter than its intro. Where both have a beat that
/// won't drift apart, it starts so the incoming's first beat lands on one of
/// the outgoing's.
pub(super) fn plan(tail: &Analysis, intro: &Analysis, rate: u32) -> Mix {
    let rate = f64::from(rate);
    let end = tail.frames as f64;
    let mut max = (MAX_SECS * rate).min(end);
    if (intro.frames as f64) < INTRO_SECS * rate {
        max = max.min(intro.frames as f64 / 2.0);
    }
    let min = (MIN_SECS * rate).min(max);
    let mut len = tail.outro().max(intro.build()).clamp(min, max);
    let mut aligned = false;
    if let (Some(out), Some(next)) = (tail.beats, intro.beats) {
        // The beats either side of the planned start, so one stays in range.
        let k = (end - len + next.first - out.first) / out.period;
        let drift = (1.0 / out.period - 1.0 / fold(next.period, out.period)).abs();
        if let Some(on_beat) = [k.floor(), k.ceil()]
            .map(|k| end - (out.first + k * out.period - next.first))
            .into_iter()
            .filter(|&a| (min..=max).contains(&a) && a * drift <= MAX_DRIFT)
            .min_by(|a, b| (a - len).abs().total_cmp(&(b - len).abs()))
        {
            (len, aligned) = (on_beat, true);
        }
    }
    let bpm = |b: Option<Beats>| {
        b.map_or("no".to_owned(), |b| {
            format!("{:.1}", 60.0 * rate / b.period)
        })
    };
    log::debug!(
        "automix: {:.1} s blend, {} BPM into {} BPM, {}",
        len / rate,
        bpm(tail.beats),
        bpm(intro.beats),
        if aligned {
            "on the beat"
        } else {
            "not aligned"
        }
    );
    // Over half a beat, ending on the incoming's beat nearest midway.
    let (trade_len, trade_end) =
        intro
            .beats
            .map_or((TRADE_SECS * rate, (len + TRADE_SECS * rate) / 2.0), |b| {
                let k = ((len / 2.0 - b.first) / b.period).round().max(0.0);
                (b.period / 2.0, b.first + k * b.period)
            });
    let trade_len = trade_len.min(len);
    Mix {
        frames: len as usize,
        trade_at: (trade_end - trade_len).clamp(0.0, len - trade_len) as usize,
        trade_len: (trade_len as usize).max(1),
    }
}

/// `period` halved or doubled to within `to`'s range, as a track in double
/// time still lands on every other beat.
fn fold(mut period: f64, to: f64) -> f64 {
    while period > to * 1.5 {
        period /= 2.0;
    }
    while period < to * 0.75 {
        period *= 2.0;
    }
    period
}

/// A blend's bass trade: the incoming track comes in with its bass cut,
/// which it takes back as the outgoing one gives up its own. For mono or
/// stereo, which is all the decoder gives.
pub(super) struct Trade {
    at: usize,
    len: usize,
    old: [Cut; 2],
    new: [Cut; 2],
}

impl Trade {
    pub(super) fn new(mix: Mix, rate: u32) -> Self {
        Self {
            at: mix.trade_at,
            len: mix.trade_len,
            old: [Cut::new(rate); 2],
            new: [Cut::new(rate); 2],
        }
    }

    /// Mixes the `frame`th frame of the blend in place, each track following
    /// its curve; the outgoing one's carries its gain.
    pub(super) fn mix(
        &mut self,
        frame: usize,
        new: &mut [f32],
        old: &[f32],
        (fade_in, fade_out): (f32, f32),
    ) {
        // Eased, so the trade neither clicks nor lingers.
        let s = (frame.saturating_sub(self.at) as f32 / self.len as f32).min(1.0);
        let s = s * s * (3.0 - 2.0 * s);
        let traded = frame >= self.at + self.len;
        let cuts = self.new.iter_mut().zip(&mut self.old);
        for ((sample, &old), (cut_new, cut_old)) in new.iter_mut().zip(old).zip(cuts) {
            let old_cut = cut_old.run(old);
            // Once traded, the incoming filter's output is never heard again.
            if !traded {
                let cut = cut_new.run(*sample);
                *sample = cut + (*sample - cut) * s;
            }
            *sample = *sample * fade_in + (old + (old_cut - old) * s) * fade_out;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    /// Stereo clicks at `bpm` from `offset` seconds, for `secs`.
    fn clicks(bpm: f64, offset: f64, secs: f64) -> Vec<f32> {
        let frames = (secs * f64::from(RATE)) as usize;
        let period = 60.0 / bpm * f64::from(RATE);
        let mut samples = vec![0.0; frames * 2];
        let mut at = offset * f64::from(RATE);
        while (at as usize) < frames {
            let start = at as usize;
            for frame in start..(start + 240).min(frames) {
                samples[frame * 2] = 0.8;
                samples[frame * 2 + 1] = 0.8;
            }
            at += period;
        }
        samples
    }

    /// Steady stereo noise at `gain(t)`, `t` in seconds.
    fn noise(secs: f64, gain: impl Fn(f64) -> f32) -> Vec<f32> {
        let mut seed = 1u32;
        (0..(secs * f64::from(RATE)) as usize * 2)
            .map(|i| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let x = (seed >> 8) as f32 / (1 << 24) as f32 - 0.5;
                x * gain((i / 2) as f64 / f64::from(RATE))
            })
            .collect()
    }

    /// `samples` scaled by `gain(t)`, `t` in seconds.
    fn shaped(samples: &[f32], gain: impl Fn(f64) -> f32) -> Vec<f32> {
        let at = |i: usize| (i / 2) as f64 / f64::from(RATE);
        samples
            .iter()
            .enumerate()
            .map(|(i, s)| s * gain(at(i)))
            .collect()
    }

    fn secs(frames: f64) -> f64 {
        frames / f64::from(RATE)
    }

    /// The blend, in seconds, from `tail` into a steady loud intro.
    fn blend_secs(tail: &[f32]) -> f64 {
        let intro = analyse(&noise(12.0, |_| 1.0), RATE, 2);
        secs(plan(&analyse(tail, RATE, 2), &intro, RATE).frames as f64)
    }

    /// How far `at` lies from the nearest beat at `bpm` from `offset`, in
    /// seconds.
    fn off_beat(at: f64, offset: f64, bpm: f64) -> f64 {
        let period = 60.0 / bpm;
        let phase = (at - offset).rem_euclid(period);
        phase.min(period - phase)
    }

    /// 40 dB down over the last 10 s of 20.
    fn fade_out(t: f64) -> f32 {
        10f32.powf(-2.0 * ((t - 10.0).max(0.0) / 10.0) as f32)
    }

    /// Bars of 2.5 s, every other 12 dB down.
    fn bars(t: f64) -> f32 {
        if ((t / 2.5) as u32).is_multiple_of(2) {
            1.0
        } else {
            0.25
        }
    }

    #[test]
    fn finds_the_tempo_and_phase_of_clicks() {
        for bpm in [92.0, 120.0, 128.0, 174.0] {
            let beats = analyse(&clicks(bpm, 0.13, 20.0), RATE, 2).beats.unwrap();
            let found = 60.0 / secs(beats.period);
            // Clicks at 174 are as much 87 in half time.
            let heard = if found < bpm * 0.75 { bpm / 2.0 } else { bpm };
            assert!(
                (found - heard).abs() / heard < 0.002,
                "{bpm}: found {found}"
            );
            // On a click, which in half time may be either of a pair.
            let off = off_beat(secs(beats.first), 0.13, bpm);
            assert!(off < 0.003, "{bpm}: {beats:?}");
        }
    }

    #[test]
    fn hears_no_beat_in_noise_or_silence() {
        let beats = analyse(&noise(20.0, |_| 1.0), RATE, 2).beats;
        assert!(beats.is_none(), "noise: {beats:?}");
        let beats = analyse(&vec![0.0; 48_000 * 20], RATE, 2).beats;
        assert!(beats.is_none(), "silence: {beats:?}");
        let beats = analyse(&clicks(120.0, 0.0, 1.0), RATE, 2).beats;
        assert!(beats.is_none(), "too short: {beats:?}");
    }

    /// A sine at `hz` and `gain`, `secs` long, in stereo.
    fn bass(hz: f32, secs: f64, gain: f32) -> Vec<f32> {
        (0..(secs * f64::from(RATE)) as usize * 2)
            .map(|i| gain * (TAU * hz * (i / 2) as f32 / RATE as f32).sin())
            .collect()
    }

    /// A sawtooth at `hz` and `gain`, `secs` long, in stereo.
    fn saw(hz: f32, secs: f64, gain: f32) -> Vec<f32> {
        (0..(secs * f64::from(RATE)) as usize * 2)
            .map(|i| gain * (2.0 * (hz * (i / 2) as f32 / RATE as f32).fract() - 1.0))
            .collect()
    }

    #[test]
    fn hears_no_beat_in_a_held_bass_note() {
        for hz in [35.0, 55.0, 110.0] {
            let beats = analyse(&bass(hz, 20.0, 0.5), RATE, 2).beats;
            assert!(beats.is_none(), "{hz} Hz: {beats:?}");
        }
        // A sawtooth's energy, and a fifth's over its root, ripple once a
        // waveform, regular as a beat.
        for hz in [30.0, 41.2, 55.0] {
            let fifth = mixed(&[saw(hz, 20.0, 0.4), saw(hz * 1.5, 20.0, 0.3)]);
            for samples in [saw(hz, 20.0, 0.5), fifth] {
                let beats = analyse(&samples, RATE, 2).beats;
                assert!(beats.is_none(), "{hz} Hz: {beats:?}");
            }
        }
    }

    #[test]
    fn hears_clicks_over_a_held_bass_note() {
        let samples = mixed(&[clicks(124.0, 0.13, 20.0), bass(55.0, 20.0, 0.3)]);
        let beats = analyse(&samples, RATE, 2).beats.unwrap();
        let found = 60.0 / secs(beats.period);
        assert!((found - 124.0).abs() < 0.25, "found {found}");
        assert!(
            off_beat(secs(beats.first), 0.13, 124.0) < 0.006,
            "{beats:?}"
        );
    }

    #[test]
    fn blends_over_a_fade_out() {
        // Found to the second.
        let len = blend_secs(&noise(20.0, fade_out));
        assert!((9.0..=10.0).contains(&len), "{len} s");
    }

    #[test]
    fn blends_over_a_quiet_intro() {
        let tail = analyse(&noise(20.0, |_| 1.0), RATE, 2);
        // Up to loud only in the intro's last half second, too.
        for quiet in [7.0, 11.5] {
            let intro = analyse(&noise(12.0, |t| if t < quiet { 0.1 } else { 1.0 }), RATE, 2);
            let mix = plan(&tail, &intro, RATE);
            assert!((secs(mix.frames as f64) - quiet).abs() < 0.4, "{mix:?}");
        }
    }

    #[test]
    fn blends_hard_ends_briefly_and_never_past_the_tail() {
        assert_eq!(blend_secs(&noise(20.0, |_| 1.0)), MIN_SECS);
        let loud = analyse(&noise(20.0, |_| 1.0), RATE, 2);
        let short = analyse(&noise(2.0, |_| 1.0), RATE, 2);
        assert_eq!(plan(&short, &loud, RATE).frames, 96_000);
    }

    #[test]
    fn lands_the_incoming_beat_on_the_outgoing() {
        // A hard end plans the shortest blend and a 16 s fade the longest, so
        // alignment must stay within both bounds whatever the phases.
        let hard = clicks(124.0, 0.21, 20.0);
        let faded = shaped(&hard, |t| {
            10f32.powf(-2.0 * ((t - 4.0).max(0.0) / 16.0) as f32)
        });
        let tails = [hard, faded].map(|s| analyse(&s, RATE, 2));
        for step in 0..40 {
            let offset = 0.01 + 0.012 * f64::from(step);
            let intro = analyse(&clicks(124.0, offset, 12.0), RATE, 2);
            let next = intro.beats.unwrap();
            for (which, tail) in tails.iter().enumerate() {
                let mix = plan(tail, &intro, RATE);
                // Against where the clicks really are, not where they were heard.
                let start = secs(tail.frames as f64 - mix.frames as f64);
                let off = off_beat(start + offset, 0.21, 124.0);
                assert!(off < 0.003, "{which}/{step}: {off} s off the beat");
                let len = secs(mix.frames as f64);
                assert!(
                    (MIN_SECS..=MAX_SECS).contains(&len),
                    "{which}/{step}: {len} s"
                );
                // Traded over half a beat, ending on an incoming beat.
                assert_eq!(
                    mix.trade_len,
                    (next.period / 2.0) as usize,
                    "{which}/{step}: {mix:?}"
                );
                let end = (mix.trade_at + mix.trade_len) as f64 - next.first;
                assert!(
                    (end / next.period - (end / next.period).round()).abs() < 0.01,
                    "{which}/{step}: the trade ends {end} frames from the first beat"
                );
            }
        }
    }

    /// A kick at `bpm` from 0.13 s: a falling 60 Hz thump over 150 ms.
    fn kicks(bpm: f64, secs: f64, gain: f32) -> Vec<f32> {
        let period = 60.0 / bpm;
        (0..(secs * f64::from(RATE)) as usize * 2)
            .map(|i| {
                let t = (i / 2) as f64 / f64::from(RATE) - 0.13;
                let since = t.rem_euclid(period) as f32;
                if t < 0.0 {
                    0.0
                } else {
                    gain * (-since / 0.05).exp() * (TAU * 60.0 * since).sin()
                }
            })
            .collect()
    }

    fn mixed(parts: &[Vec<f32>]) -> Vec<f32> {
        (0..parts[0].len())
            .map(|i| parts.iter().map(|p| p[i]).sum())
            .collect()
    }

    #[test]
    fn hears_a_kick_over_a_bass_line() {
        // A sine bass at some tempos cancels every other kick's thump, leaving
        // a pulse at half the tempo, under the range sought, so it is tried at
        // 124 alone.
        let basses = [90.0, 120.0, 126.0, 135.0, 174.0]
            .map(|bpm| (bpm, saw(41.2, 20.0, 0.25)))
            .into_iter()
            .chain([(124.0, bass(41.0, 20.0, 0.3))]);
        for (bpm, bass) in basses {
            let samples = mixed(&[kicks(bpm, 20.0, 0.6), bass, noise(20.0, |_| 0.2)]);
            let beats = analyse(&samples, RATE, 2).beats.unwrap();
            let found = fold(60.0 / secs(beats.period), bpm);
            assert!((found - bpm).abs() / bpm < 0.002, "{bpm}: {beats:?}");
        }
    }

    #[test]
    fn leaves_a_tempo_that_wanders() {
        let mut samples = clicks(120.0, 0.13, 10.0);
        samples.extend(clicks(123.0, 0.0, 10.0));
        let beats = analyse(&samples, RATE, 2).beats;
        assert!(beats.is_none(), "a step: {beats:?}");
        // Or speeds up throughout, landing the grid's end off the beat.
        let frames = 20 * RATE as usize;
        let mut ramp = vec![0.0; frames * 2];
        let mut t = 0.13;
        while t < 20.0 {
            let at = (t * f64::from(RATE)) as usize;
            ramp[at * 2..(at + 240).min(frames) * 2].fill(0.8);
            t += 60.0 / (120.0 + 2.0 * t / 20.0);
        }
        let beats = analyse(&ramp, RATE, 2).beats;
        assert!(beats.is_none(), "a ramp: {beats:?}");
    }

    /// `samples` silenced outside `secs`.
    fn gated(samples: &[f32], secs: std::ops::Range<f64>) -> Vec<f32> {
        shaped(samples, |t| if secs.contains(&t) { 1.0 } else { 0.0 })
    }

    #[test]
    fn hears_a_beat_that_comes_in_late_or_stops() {
        // Kicks in for the intro's last 5 s, and out for the tail's last 10.
        for (long, kicking) in [(12.0, 7.0..12.0), (20.0, 0.0..10.0)] {
            let samples = mixed(&[
                gated(&kicks(124.0, long, 0.6), kicking.clone()),
                saw(41.2, long, 0.25),
                noise(long, |_| 0.2),
            ]);
            let beats = analyse(&samples, RATE, 2).beats.unwrap();
            assert!(
                (60.0 / secs(beats.period) - 124.0).abs() < 0.25,
                "{beats:?}"
            );
            // On the kicks, where they play.
            let middle = (kicking.start + kicking.end) / 2.0;
            let k = ((middle - secs(beats.first)) / secs(beats.period)).round();
            let at = secs(beats.first + k * beats.period);
            assert!(off_beat(at, 0.13, 124.0) < 0.012, "{beats:?}");
        }
    }

    #[test]
    fn blends_over_a_slow_fade_of_music() {
        // 40 dB over 30 s, which the music's own swell slows to under 1 dB in
        // some seconds.
        let music = mixed(&[
            kicks(124.0, 24.0, 0.6),
            saw(41.2, 24.0, 0.25),
            noise(24.0, |_| 0.2),
        ]);
        let faded: Vec<f32> = music
            .iter()
            .enumerate()
            .map(|(i, s)| s * 10f32.powf(-2.0 * (i / 2) as f32 / RATE as f32 / 30.0))
            .collect();
        assert_eq!(blend_secs(&faded), MAX_SECS);
    }

    #[test]
    fn blends_over_a_fade_into_hiss() {
        // 60 dB over 10 s, then tape hiss 44 dB down.
        let gain = |t: f64| 10f32.powf(-3.0 * ((t - 10.0).max(0.0) / 10.0) as f32) + 0.006;
        let len = blend_secs(&noise(22.0, gain));
        assert!((11.0..=12.0).contains(&len), "{len} s");
    }

    #[test]
    fn hears_the_beat_past_a_quiet_stretch() {
        // The whole mix 40 dB down for a second, as at a stop before a chorus.
        let samples = shaped(
            &mixed(&[kicks(124.0, 20.0, 0.6), bass(41.0, 20.0, 0.15)]),
            |t| if (8.0..9.0).contains(&t) { 0.01 } else { 1.0 },
        );
        let beats = analyse(&samples, RATE, 2).beats.unwrap();
        assert!(
            (60.0 / secs(beats.period) - 124.0).abs() < 0.25,
            "{beats:?}"
        );
        assert!(
            off_beat(secs(beats.first), 0.13, 124.0) < 0.012,
            "{beats:?}"
        );
    }

    #[test]
    fn leaves_a_softer_section_held_to_the_end_alone() {
        // Applause 12 dB under the music, to the end.
        let tail = noise(24.0, |t| if t < 10.0 { 1.0 } else { 0.25 });
        assert_eq!(blend_secs(&tail), MIN_SECS);
    }

    #[test]
    fn hears_no_beat_in_a_few_hits() {
        // Two tom hits over a pad, their gap as regular as any tempo.
        let mut samples = mixed(&[bass(220.0, 12.0, 0.2), noise(12.0, |_| 0.002)]);
        for at in [5.0, 5.6] {
            let from = (at * f64::from(RATE)) as usize * 2;
            for (i, s) in samples[from..from + 24_000].iter_mut().enumerate() {
                *s += 0.8
                    * (-((i / 2) as f32) / 4_800.0).exp()
                    * (TAU * 110.0 * (i / 2) as f32 / RATE as f32).sin();
            }
        }
        let beats = analyse(&samples, RATE, 2).beats;
        assert!(beats.is_none(), "{beats:?}");
    }

    #[test]
    fn keeps_the_beat_through_a_breakdown_off_it() {
        // 4 s of offbeat clicks alone in place of the beat.
        let on = clicks(124.0, 0.13, 20.0);
        let off = clicks(124.0, 0.13 + 30.0 / 124.0, 20.0);
        let samples = mixed(&[
            gated(&on, 0.0..8.0),
            gated(&off, 8.0..12.0),
            gated(&on, 12.0..20.0),
        ]);
        let beats = analyse(&samples, RATE, 2).beats.unwrap();
        assert!(
            off_beat(secs(beats.first), 0.13, 124.0) < 0.003,
            "{beats:?}"
        );
    }

    #[test]
    fn blends_only_over_the_fade_of_music_that_swells() {
        // Faded 40 dB over the last 3 s.
        let tail = noise(20.0, |t| {
            bars(t) * 10f32.powf(-2.0 * ((t - 17.0).max(0.0) / 3.0) as f32)
        });
        let len = blend_secs(&tail);
        assert!(len <= 6.0, "{len} s");
    }

    #[test]
    fn leaves_music_that_swells_and_stops_alone() {
        // No 5 s hold.
        assert_eq!(blend_secs(&noise(20.0, bars)), MIN_SECS);
    }

    #[test]
    fn blends_over_a_fade_out_of_a_quieter_section() {
        let gain = |t: f64| match t {
            t if t < 4.0 => 1.0,
            t if t < 14.0 => 0.4,
            t => 0.4 * 10f32.powf(-0.25 * (t - 14.0) as f32),
        };
        let len = blend_secs(&noise(24.0, gain));
        assert!((9.0..=10.0).contains(&len), "{len} s");
    }

    #[test]
    fn leaves_a_soft_ending_that_stops_alone() {
        // A last verse 8 dB down whose final chord rings out, 80 dB over `ring`.
        for ring in [0.0, 1.0, 3.0] {
            let gain = |t: f64| match t {
                t if t < 6.0 => 1.0,
                t if t < 18.0 => 0.4,
                t => 0.4 * 10f32.powf((-4.0 * (t - 18.0) / ring) as f32),
            };
            let len = blend_secs(&noise(18.0 + ring, gain));
            assert_eq!(len, MIN_SECS, "rung out over {ring} s");
        }
    }

    #[test]
    fn blends_over_a_fade_linear_in_amplitude() {
        // Over 10 s, its first 4 s losing only 3 dB, which holds as the music.
        let len = blend_secs(&noise(20.0, |t| 1.0 - (t as f32 - 10.0).max(0.0) / 10.0));
        assert!((6.0..=7.0).contains(&len), "{len} s");
    }

    #[test]
    fn blends_over_at_most_half_a_short_track() {
        let tail = analyse(&noise(20.0, fade_out), RATE, 2);
        let mix = plan(&tail, &analyse(&noise(5.0, |_| 1.0), RATE, 2), RATE);
        assert_eq!(secs(mix.frames as f64), 2.5);
    }

    #[test]
    fn leaves_beats_that_would_drift_apart() {
        let tail = analyse(&clicks(120.0, 0.21, 20.0), RATE, 2);
        let intro = analyse(&clicks(128.0, 0.37, 12.0), RATE, 2);
        assert_eq!(
            plan(&tail, &intro, RATE).frames,
            (MIN_SECS * f64::from(RATE)) as usize
        );
    }

    #[test]
    fn folds_double_time_onto_the_beat() {
        assert_eq!(fold(250.0, 500.0), 500.0);
        assert_eq!(fold(1_000.0, 500.0), 500.0);
        assert_eq!(fold(600.0, 500.0), 600.0);
    }

    #[test]
    fn trades_the_bass_rather_than_summing_it() {
        let bass =
            |phase: f32| (0..48_000).map(move |i| (TAU * 50.0 * i as f32 / 48_000.0 + phase).sin());
        let len = 48_000;
        let mix = Mix {
            frames: len,
            trade_at: len / 2,
            trade_len: 4_800,
        };
        let mut trade = Trade::new(mix, RATE);
        let mut peak = 0f32;
        for (i, (new, old)) in bass(0.3).zip(bass(0.0)).enumerate() {
            let t = i as f32 / len as f32 * std::f32::consts::FRAC_PI_2;
            let mut frame = [new];
            trade.mix(i, &mut frame, &[old], t.sin_cos());
            peak = peak.max(frame[0].abs());
        }
        // Equal-power curves alone would sum the two near 1.4.
        assert!(peak < 1.05, "peaked at {peak}");
    }

    #[test]
    fn ends_on_the_incoming_whole() {
        let mut trade = Trade::new(
            Mix {
                frames: 100,
                trade_at: 0,
                trade_len: 1,
            },
            RATE,
        );
        let mut frame = [0.5, -0.25];
        trade.mix(10, &mut frame, &[0.9, 0.9], (1.0, 0.0));
        assert!((frame[0] - 0.5).abs() < 1e-6 && (frame[1] + 0.25).abs() < 1e-6);
    }
}
