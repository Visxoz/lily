//! Automix: plans a blend on the beat or bar, varispeeding the outgoing to a
//! near tempo and keeping two voices apart, and mixes it by trading bass,
//! sweeping filters or echoing out.

use std::{
    f32::consts::{FRAC_PI_2, TAU},
    ops::RangeInclusive,
};

use crate::audio::{Sung, output::Varispeed};

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
/// A level held this far under the loud one, in dB, is a noise floor such as
/// tape hiss, which a fade ends in, not the music.
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
/// How far each finer period's anchor is sought from the whole-hop lag's.
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
/// How much more bass onset the downbeats must carry than the next likeliest
/// beat of the bar, for a bar to count.
const BAR_CLARITY: f64 = 1.5;
/// Blend lengths of this many bars are the likeliest a DJ plays.
const PHRASE_BARS: [f64; 2] = [4.0, 8.0];
/// The most the outgoing's tempo is changed to meet the incoming's.
const MAX_VARISPEED: f64 = 0.06;
/// The least a tempo change takes, in seconds, in whole bars of at least two
/// where there are bars.
const RAMP_SECS: f64 = 4.0;
/// How much of a track's energy must be bass for it to carry bass.
const BASS_SHARE: f64 = 0.15;
/// How close to its loud level a track ending without a fade ends, in dB,
/// for it to stop hard.
const HARD_DB: f32 = 6.0;
/// The most beats an echo out replaces.
const ECHO_BEATS: f64 = 4.0;
/// How much of each echo feeds the next.
const FEEDBACK: f32 = 0.45;
/// How long the beat sent to the echo takes to fade in and out, in seconds.
const EDGE_SECS: f32 = 0.003;
/// The echo's damping, each repeat darker and thinner, in Hz.
const ECHO_LOW_HZ: f32 = 4_000.0;
const ECHO_HIGH_HZ: f32 = 150.0;
/// How far a sweep's low-pass falls on the outgoing, from open, in Hz.
const SWEEP_TO_HZ: f64 = 300.0;
const OPEN_HZ: f64 = 20_000.0;
/// Where the incoming's high-pass starts, and where it is open by, in Hz.
const SWEEP_FROM_HZ: f64 = 1_200.0;
const OPENED_HZ: f64 = 30.0;
/// How far into the blend the incoming is open, and the outgoing's low-pass
/// fully in, as a share of it.
const OPEN_BY: f32 = 0.6;
const FILTERED_BY: f32 = 0.25;
/// How often a sweep's filters are retuned, in frames.
const RETUNE_FRAMES: usize = 32;
/// Where skewed curves have the outgoing out and the incoming in from, as a
/// share of the blend, so two voices barely overlap.
const SKEW: f32 = 0.6;
/// How much later than its timing says the outgoing's singing may end, in
/// seconds: line-timed lyrics hold each line until the next.
const SUNG_SLACK_SECS: f64 = 1.5;
/// Where the bass splits from the rest, in Hz.
const BASS_HZ: f32 = 160.0;
/// The bass trade's length with no beat to time it, in seconds.
const TRADE_SECS: f64 = 0.5;
/// How long before the trade the outgoing's cut starts, so it has settled.
const WARM_SECS: f32 = 0.02;
/// Energy of -100 dB, which keeps the log of silence finite.
const FLOOR: f32 = 1e-10;
/// Filter memory below this is flushed, as denormal floats are slow.
const DENORMAL: f32 = 1e-20;

/// Two cascaded one-pole low-passes: the bass's loudness, for kicks, and an
/// echo's damping.
#[derive(Clone, Copy)]
struct Lows {
    coeff: f32,
    a: f32,
    b: f32,
}

impl Lows {
    fn new(hz: f32, rate: u32) -> Self {
        Self {
            coeff: 1.0 - (-TAU * hz / rate as f32).exp(),
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

#[derive(Clone, Copy)]
enum Pass {
    Low,
    High,
}

/// Two Butterworth biquads (RBJ's cookbook) in series, 24 dB an octave, so
/// as a high-pass at `BASS_HZ` a kick an octave down is 24 dB under.
#[derive(Clone, Copy)]
struct Cut {
    b0: f64,
    b1: f64,
    a1: f64,
    a2: f64,
    /// Each stage's transposed direct form II memory.
    z: [[f64; 2]; 2],
}

impl Cut {
    fn new(pass: Pass, hz: f64, rate: u32) -> Self {
        let mut cut = Self {
            b0: 0.0,
            b1: 0.0,
            a1: 0.0,
            a2: 0.0,
            z: [[0.0; 2]; 2],
        };
        cut.tune(pass, hz, rate);
        cut
    }

    /// Moves the corner to `hz`, keeping the memory, so a sweep runs smoothly.
    fn tune(&mut self, pass: Pass, hz: f64, rate: u32) {
        let (sin, cos) = (std::f64::consts::TAU * hz / f64::from(rate)).sin_cos();
        let alpha = sin * std::f64::consts::FRAC_1_SQRT_2;
        let a0 = 1.0 + alpha;
        // b2 is b0 either way, and b1 is twice it, negated for a high-pass.
        (self.b0, self.b1) = match pass {
            Pass::Low => ((1.0 - cos) / 2.0 / a0, (1.0 - cos) / a0),
            Pass::High => ((1.0 + cos) / 2.0 / a0, -(1.0 + cos) / a0),
        };
        self.a1 = -2.0 * cos / a0;
        self.a2 = (1.0 - alpha) / a0;
    }

    fn run(&mut self, x: f32) -> f32 {
        let mut x = f64::from(x);
        for z in &mut self.z {
            let y = self.b0 * x + z[0];
            z[0] = self.b1 * x - self.a1 * y + z[1];
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

/// A steady beat, in frames: between beats, to the first, and to the first
/// downbeat of a bar of four should one stand out.
#[derive(Debug, Clone, Copy)]
struct Beats {
    period: f64,
    first: f64,
    downbeat: Option<f64>,
}

/// What a blend is planned on, from one track's samples.
pub(super) struct Analysis {
    frames: usize,
    hop: usize,
    /// Smoothed loudness at each hop, in dB.
    level: Vec<f32>,
    /// The level's 90th percentile: the track at its loud, not a peak.
    loud: f32,
    beats: Option<Beats>,
    /// Whether bass is a good share of it, for trading.
    bass: bool,
}

pub(super) fn analyse(samples: &[f32], rate: u32, channels: usize) -> Analysis {
    let hop = (rate / HOPS_PER_SEC).max(1) as usize;
    let hops = samples.len() / (hop * channels);
    let mut lows = Lows::new(BASS_HZ, rate);
    // Running sums of each hop's energy, in f64 so a quiet stretch after a
    // loud one keeps its precision; a stretch's mean is then two lookups.
    let (mut all, mut low) = (vec![0.0f64], vec![0.0f64]);
    all.reserve_exact(hops);
    low.reserve_exact(hops);
    let per_channel = 1.0 / channels as f32;
    for block in samples.chunks_exact(hop * channels) {
        let (mut a, mut l) = (0.0, 0.0);
        for frame in block.chunks_exact(channels) {
            let x = frame.iter().sum::<f32>() * per_channel;
            let lo = lows.run(x);
            a += x * x;
            l += lo * lo;
        }
        all.push(all[all.len() - 1] + f64::from(a / hop as f32));
        low.push(low[low.len() - 1] + f64::from(l / hop as f32));
    }
    // The bass's rises for kicks, the whole's for the rest, taken where they
    // step up, as a capped rise stays flat while its hit is recent. The
    // bass's uncapped, which an accent shows in, mark the downbeats.
    let (all_recent, low_recent) = (recent(&all), recent(&low));
    let mut last = 0.0;
    let (mut onsets, mut hits) = (Vec::with_capacity(hops), Vec::with_capacity(hops));
    for i in 0..hops {
        let (bass, hit) = rise(&low_recent, i);
        let r = rise(&all_recent, i).0 + bass;
        onsets.push((r - last).max(0.0));
        hits.push(hit);
        last = r;
    }
    let beats = beats(&onsets, f64::from(rate) / hop as f64).map(|b| Beats {
        period: b.period * hop as f64,
        first: b.first * hop as f64,
        downbeat: downbeat(&hits, b).map(|d| d * hop as f64),
    });
    let level = smooth(&all);
    Analysis {
        frames: samples.len() / channels,
        hop,
        loud: loud(&level),
        level,
        beats,
        // Strictly, so silence carries none.
        bass: low[hops] > BASS_SHARE * all[hops],
    }
}

/// The 90th percentile of `level`.
fn loud(level: &[f32]) -> f32 {
    if level.is_empty() {
        return 0.0;
    }
    let mut level = level.to_vec();
    let i = level.len() * 9 / 10;
    *level.select_nth_unstable_by(i, f32::total_cmp).1
}

/// The mean energy of hops `from..to`.
fn mean(sums: &[f64], from: usize, to: usize) -> f64 {
    (sums[to] - sums[from]) / (to - from) as f64
}

/// An energy in bels, floored.
fn bels(energy: f64) -> f32 {
    (energy as f32).max(FLOOR).log10()
}

/// The energy of `RECENT_HOPS` from each hop on, from running `sums`.
fn recent(sums: &[f64]) -> Vec<f64> {
    sums.windows(RECENT_HOPS + 1)
        .map(|w| w[RECENT_HOPS] - w[0])
        .collect()
}

/// How far hop `i` and those just before it rise over the loudest stretch as
/// long before them, from `recent`'s sums: in bels past `MIN_RISE` and up to
/// `MAX_RISE`, and in energy; none this early in.
fn rise(recent: &[f64], i: usize) -> (f32, f32) {
    let Some(before) = (i + 1).checked_sub(RECENT_HOPS + BEFORE_HOPS) else {
        return (0.0, 0.0);
    };
    let from = before + BEFORE_HOPS;
    let width = RECENT_HOPS as f64;
    let loudest = recent[before..=from - RECENT_HOPS]
        .iter()
        .copied()
        .fold(0.0, f64::max)
        / width;
    let now = recent[from] / width;
    let rise = bels(now) - bels(loudest);
    (
        (rise.min(MAX_RISE) - MIN_RISE).max(0.0),
        (now - loudest).max(0.0) as f32,
    )
}

/// The first downbeat of `beats`, in hops: of the bar's four beats, the one
/// whose bass `hits` are clearly the strongest, taking bars as of four.
fn downbeat(hits: &[f32], beats: Beats) -> Option<f64> {
    let weights: [f64; 4] = std::array::from_fn(|p| {
        let anchor = beats.first + p as f64 * beats.period;
        centres(hits, (4.0 * beats.period, anchor))
            .iter()
            .map(|t| t.mass)
            .sum()
    });
    let mut sorted = weights;
    sorted.sort_by(|a, b| b.total_cmp(a));
    let p = weights.iter().position(|&w| w == sorted[0])?;
    (sorted[0] > BAR_CLARITY * sorted[1]).then_some(beats.first + p as f64 * beats.period)
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
    let (lag, pulse, _) = (min_lag..=max_lag)
        .map(|lag| {
            let c = corr(lag);
            (lag, c, c * prior(lag))
        })
        .max_by(|a, b| a.2.total_cmp(&b.2))?;
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
    let anchors = rough - ANCHOR_HOPS..=rough + ANCHOR_HOPS;
    let (period, anchor) = best(&blurred, periods, anchors)?;
    // The blur that lets the comb find the beat also flattens it over small
    // errors, so the grid is refitted through each beat's own onsets.
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
        downbeat: None,
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
    /// Frames the track fades out over: from an end `FADED_DB` under its loud
    /// level back to where its level last held, or last swelled; 0 for an
    /// ending that stops or holds a softer level to the end.
    fn outro(&self) -> f64 {
        if self
            .level
            .last()
            .is_none_or(|&end| end > self.loud - FADED_DB)
        {
            return 0.0;
        }
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
        let floor = self.loud - HISS_DB;
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

    /// What an echo out replaces of a track on a beat that stops hard, with
    /// no fade-out (`outro` 0) and its last second peaking within `HARD_DB`
    /// of its loud level: its last up to `ECHO_BEATS` beats from a beat, and
    /// the beat, in frames.
    fn echo(&self, max: f64, outro: f64) -> Option<(f64, f64)> {
        let last = self.level.len().saturating_sub(HOPS_PER_SEC as usize);
        let hard = outro == 0.0 && self.level[last..].iter().any(|&l| l >= self.loud - HARD_DB);
        let beats = self.beats.filter(|_| hard)?;
        let end = self.frames as f64;
        let k = ((end - ECHO_BEATS * beats.period - beats.first) / beats.period)
            .ceil()
            .max(0.0);
        let len = end - beats.first - k * beats.period;
        (beats.period..=max)
            .contains(&len)
            .then_some((len, beats.period))
    }
}

/// How a blend is mixed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Style {
    /// Equal-power curves, the bass traded over `.1` frames from `.0` where
    /// both tracks carry it.
    Blend { trade: Option<(usize, usize)> },
    /// The outgoing low-passed away as the incoming opens from a high-pass,
    /// `skew`ed curves keeping two voices' overlap short.
    Sweep { skew: bool },
    /// The outgoing's last beats replaced by echoes of the first, `beat`
    /// frames apart, under the incoming.
    Echo { beat: usize },
}

impl Style {
    /// A crossfade's: equal-power curves alone.
    pub(super) const CROSSFADE: Self = Self::Blend { trade: None };
}

/// A planned blend over the tail's last `frames`, counted at the incoming's
/// speed, mixed in `style`.
#[derive(Debug)]
pub(super) struct Mix {
    pub(super) frames: usize,
    /// The tail from this frame on played by the curve, so the blend is at
    /// the incoming's tempo.
    pub(super) warp: Option<(usize, Varispeed)>,
    pub(super) style: Style,
}

/// Where the two songs are sung about the blend, in frames: from the
/// outgoing's last word to its tail's end, and from the incoming's start to
/// its first word.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Voices {
    last: Option<f64>,
    first: Option<f64>,
}

impl Voices {
    /// From `sung`, for a tail ending at `tail_end` and an intro starting at
    /// `intro_start`, in song seconds.
    pub(super) fn new(sung: &Sung, tail_end: f64, intro_start: f64, rate: u32) -> Self {
        let rate = f64::from(rate);
        // No spans, or none past the intro's start, is as good as unknown.
        let last = sung.current.as_ref().and_then(|spans| {
            let end = spans.iter().map(|s| s.end).reduce(f64::max)?;
            Some(((tail_end - end) * rate).max(0.0))
        });
        let first = sung.next.as_ref().and_then(|spans| {
            let start = spans
                .iter()
                .filter(|s| s.end > intro_start)
                .map(|s| s.start)
                .reduce(f64::min)?;
            Some(((start - intro_start) * rate).max(0.0))
        });
        Self { last, first }
    }

    /// The longest blend, the outgoing played at `speed`, after which the
    /// incoming's first word follows the outgoing's last; none where either
    /// is unknown.
    fn longest(self, speed: f64, rate: f64) -> Option<f64> {
        Some(self.first? + self.last? / speed + SUNG_SLACK_SECS * rate)
    }
}

/// Blend lengths `step` apart through `at`.
#[derive(Clone, Copy)]
struct Lengths {
    at: f64,
    step: f64,
}

impl Lengths {
    fn within(self, min: f64, max: f64) -> impl Iterator<Item = f64> {
        let k = |len: f64| (len - self.at) / self.step;
        (k(min).ceil() as i64..=k(max).floor() as i64).map(move |k| self.at + k as f64 * self.step)
    }
}

/// How the outgoing's beat is met: its tail at `speed` after a `ramp` of song
/// frames, and the blends up to `longest` on the beat, or on the bar.
#[derive(Clone, Copy)]
struct Sync {
    speed: f64,
    ramp: f64,
    longest: f64,
    beats: Lengths,
    /// The bar lengths, and the incoming's first downbeat, which they reach.
    bars: Option<(Lengths, f64)>,
}

impl Sync {
    /// Meets the beat as is while the drift stays under `MAX_DRIFT`, else
    /// with the outgoing varispeeded to the incoming's folded `period`, should
    /// `speed` be within `MAX_VARISPEED`.
    fn new(
        out: Beats,
        next: Beats,
        (period, speed): (f64, f64),
        end: f64,
        max: f64,
        rate: f64,
    ) -> impl Iterator<Item = Self> {
        let drift = (1.0 / out.period - 1.0 / period).abs();
        let as_is = Self::at_speed(out, next, end, 1.0, 0.0, max.min(MAX_DRIFT / drift));
        let warped = ((speed - 1.0).abs() <= MAX_VARISPEED).then(|| {
            let ramp = out.downbeat.map_or(RAMP_SECS * rate, |_| {
                let bar = 4.0 * out.period;
                (RAMP_SECS * rate / bar).ceil().max(2.0) * bar
            });
            Self::at_speed(out, next, end, speed, ramp, max.min((end - ramp) / speed))
        });
        [Some(as_is), warped].into_iter().flatten()
    }

    fn at_speed(out: Beats, next: Beats, end: f64, speed: f64, ramp: f64, longest: f64) -> Self {
        // An outgoing beat at `o` lies `(end - o) / speed` before the end once
        // played at speed, so a blend that long, plus `n`, lands one on it.
        let lengths = |o: f64, n: f64, step: f64| Lengths {
            at: (end - o) / speed + n,
            step: step / speed,
        };
        Self {
            speed,
            ramp,
            longest,
            beats: lengths(out.first, next.first, out.period),
            bars: out
                .downbeat
                .zip(next.downbeat)
                .map(|(o, n)| (lengths(o, n, 4.0 * out.period), n)),
        }
    }

    /// The blend no longer than the singing allows (`sung`) nearest `want`: on the bar where
    /// there are bars, a phrase of 4 or 8 by up to half a bar further, else
    /// on the beat; and whether on the bar.
    fn pick(self, min: f64, sung: f64, want: f64) -> Option<(f64, bool)> {
        let max = sung.min(self.longest);
        let nearest = |lengths: Lengths, min: f64, phrase: bool| {
            let cost = |len: f64| {
                let phrased = phrase && PHRASE_BARS.contains(&(len / lengths.step).round());
                (len - want).abs() - if phrased { lengths.step / 2.0 } else { 0.0 }
            };
            lengths
                .within(min, max)
                .min_by(|a, b| cost(*a).total_cmp(&cost(*b)))
        };
        // Reaching the incoming's first downbeat, so a bar really meets.
        self.bars
            .and_then(|(bars, first)| nearest(bars, min.max(first), true))
            .map(|len| (len, true))
            .or_else(|| nearest(self.beats, min, false).map(|len| (len, false)))
    }

    /// Where the tail's varispeed starts for a blend of `len`, and its curve.
    fn warp(self, end: f64, len: f64) -> Option<(usize, Varispeed)> {
        (self.ramp > 0.0).then(|| {
            let curve = Varispeed {
                ramp: (self.ramp * 2.0 / (1.0 + self.speed)).round(),
                speed: self.speed,
            };
            let start = end - self.speed * len - curve.song_frames(curve.ramp);
            (start.round().max(0.0) as usize, curve)
        })
    }
}

/// Blends over the outgoing's fade-out or the incoming's build, on the bar or
/// beat where both have one, else echoed out, swept or blended as they are.
pub(super) fn plan(tail: &Analysis, intro: &Analysis, rate: u32, voices: Voices) -> Mix {
    let rate = f64::from(rate);
    let end = tail.frames as f64;
    let mut max = (MAX_SECS * rate).min(end);
    if (intro.frames as f64) < INTRO_SECS * rate {
        max = max.min(intro.frames as f64 / 2.0);
    }
    let min = (MIN_SECS * rate).min(max);
    let outro = tail.outro();
    let want = outro.max(intro.build()).clamp(min, max);
    // With the incoming's period folded to the outgoing's, and the speed meeting it.
    let both = tail.beats.zip(intro.beats).map(|(out, next)| {
        let period = fold(next.period, out.period);
        (out, next, (period, out.period / period))
    });
    let longest = |speed| voices.longest(speed, rate).unwrap_or(f64::INFINITY);
    let blend = |len| Style::Blend {
        trade: (tail.bass && intro.bass).then(|| trade(intro, len, rate)),
    };
    let synced = both.and_then(|(out, next, tempo)| {
        Sync::new(out, next, tempo, end, max, rate)
            .find_map(|s| Some((s, s.pick(min, longest(s.speed), want)?)))
    });
    let mut warp = None;
    let (len, style, how) = if let Some((s, (len, on_bar))) = synced {
        warp = s.warp(end, len);
        (
            len,
            blend(len),
            if on_bar { "on the bar" } else { "on the beat" },
        )
    } else if let Some((len, beat)) = tail
        .echo(max, outro)
        .filter(|&(len, _)| len <= longest(1.0))
    {
        let style = Style::Echo {
            beat: beat.round() as usize,
        };
        (len, style, "echoed out")
    } else if min <= longest(1.0) {
        let len = want.min(longest(1.0));
        let style = if both.is_some() {
            Style::Sweep { skew: false }
        } else {
            blend(len)
        };
        (len, style, "not aligned")
    } else {
        (min, Style::Sweep { skew: true }, "sung over")
    };
    let bpm = |b: Option<Beats>| {
        b.map_or("no".to_owned(), |b| {
            let bars = if b.downbeat.is_some() { " in bars" } else { "" };
            format!("{:.1}{bars}", 60.0 * rate / b.period)
        })
    };
    log::debug!(
        "automix: {:.1} s blend, {} BPM into {} BPM, tempo ratio {}, {how}, {style:?}, speed {:.3}, vocals {}",
        len / rate,
        bpm(tail.beats),
        bpm(intro.beats),
        both.map_or("-".to_owned(), |(_, _, (_, speed))| format!("{speed:.3}")),
        warp.map_or(1.0, |(_, c)| c.speed),
        voices
            .longest(1.0, rate)
            .map_or("unknown".to_owned(), |l| format!(
                "apart within {:.1} s",
                l / rate
            )),
    );
    Mix {
        frames: len as usize,
        warp,
        style,
    }
}

/// The bass trade over half a beat, ending on the incoming's bar line
/// nearest midway, or its beat, as `(from, frames)`.
fn trade(intro: &Analysis, len: f64, rate: f64) -> (usize, usize) {
    let (trade_len, trade_end) =
        intro
            .beats
            .map_or((TRADE_SECS * rate, (len + TRADE_SECS * rate) / 2.0), |b| {
                let (first, step) = b
                    .downbeat
                    .map_or((b.first, b.period), |d| (d, 4.0 * b.period));
                let k = ((len / 2.0 - first) / step).round().max(0.0);
                (b.period / 2.0, first + k * step)
            });
    let trade_len = trade_len.min(len);
    (
        (trade_end - trade_len).clamp(0.0, len - trade_len) as usize,
        (trade_len as usize).max(1),
    )
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

/// Equal-power curves in and out at `t` through a blend, keeping the
/// loudness steady where linear ones dip midway.
fn equal_power(t: f32) -> (f32, f32) {
    (t * FRAC_PI_2).sin_cos()
}

/// Eased from 0 to 1, so a change neither clicks nor lingers.
fn ease(s: f32) -> f32 {
    let s = s.clamp(0.0, 1.0);
    s * s * (3.0 - 2.0 * s)
}

/// The corner a low-pass is open at, short of Nyquist.
fn open_hz(rate: u32) -> f64 {
    OPEN_HZ.min(0.45 * f64::from(rate))
}

/// Mixes a blend in its style, for mono or stereo, which is all the decoder
/// gives.
pub(super) struct Mixer {
    /// `1 / frames` of the blend, so a frame's place in it is a multiply.
    step: f32,
    /// Frames mixed so far.
    mixed: usize,
    channels: usize,
    style: Styled,
}

/// Each style's own state.
enum Styled {
    Plain,
    Trade(Trade),
    Sweep(Sweep),
    Echo(Echo),
}

/// A frame of the blend: the incoming's samples, mixed in place, the
/// outgoing's, and its number in the blend.
type Frame<'a> = ((&'a mut [f32], &'a [f32]), usize);

impl Mixer {
    pub(super) fn new(frames: usize, style: Style, rate: u32, channels: usize) -> Self {
        let cuts = |pass, hz| [Cut::new(pass, hz, rate); 2];
        let style = match style {
            Style::Blend { trade: None } => Styled::Plain,
            Style::Blend {
                trade: Some((at, len)),
            } => Styled::Trade(Trade {
                at,
                len,
                warm: at.saturating_sub((WARM_SECS * rate as f32) as usize),
                old: cuts(Pass::High, BASS_HZ.into()),
                new: cuts(Pass::High, BASS_HZ.into()),
            }),
            Style::Sweep { skew } => Styled::Sweep(Sweep {
                skew,
                rate,
                old: cuts(Pass::Low, open_hz(rate)),
                new: cuts(Pass::High, SWEEP_FROM_HZ),
            }),
            Style::Echo { beat } => Styled::Echo(Echo {
                frames,
                beat,
                line: vec![0.0; beat * channels],
                damp: [(Lows::new(ECHO_LOW_HZ, rate), Lows::new(ECHO_HIGH_HZ, rate)); 2],
                edge: (EDGE_SECS * rate as f32).max(1.0),
            }),
        };
        Self {
            step: 1.0 / frames as f32,
            mixed: 0,
            channels,
            style,
        }
    }

    /// Mixes the blend's next frames into `new` in place, the outgoing
    /// track's `old` at `old_gain` to the incoming.
    pub(super) fn mix(&mut self, new: &mut [f32], old: &[f32], old_gain: f32) {
        let first = self.mixed;
        self.mixed += new.len() / self.channels;
        let frames = new
            .chunks_exact_mut(self.channels)
            .zip(old.chunks_exact(self.channels))
            .zip(first..);
        let step = self.step;
        match &mut self.style {
            Styled::Plain => {
                for ((new, old), frame) in frames {
                    let (fade_in, fade_out) = equal_power(frame as f32 * step);
                    let fade_out = fade_out * old_gain;
                    for (sample, old) in new.iter_mut().zip(old) {
                        *sample = *sample * fade_in + old * fade_out;
                    }
                }
            }
            Styled::Trade(trade) => trade.mix(frames, step, old_gain),
            Styled::Sweep(sweep) => sweep.mix(frames, step, old_gain),
            Styled::Echo(echo) => echo.mix(frames, old_gain),
        }
    }
}

/// The bass trade over `len` frames from `at`, its cuts a channel each.
struct Trade {
    at: usize,
    len: usize,
    /// Where the outgoing's cut starts, so it has settled by the trade.
    warm: usize,
    old: [Cut; 2],
    new: [Cut; 2],
}

impl Trade {
    /// The incoming comes in with its bass cut, which it takes back as the
    /// outgoing gives up its own, so only one plays.
    fn mix<'a>(&mut self, frames: impl Iterator<Item = Frame<'a>>, step: f32, old_gain: f32) {
        for ((new, old), frame) in frames {
            let (fade_in, fade_out) = equal_power(frame as f32 * step);
            let fade_out = fade_out * old_gain;
            let s = ease(frame.saturating_sub(self.at) as f32 / self.len as f32);
            // Once traded, the incoming's cut is never heard again, nor the
            // outgoing's before the trade.
            let (traded, warm) = (frame >= self.at + self.len, frame >= self.warm);
            let cuts = self.new.iter_mut().zip(&mut self.old);
            for ((sample, &old), (cut_new, cut_old)) in new.iter_mut().zip(old).zip(cuts) {
                if !traded {
                    let cut = cut_new.run(*sample);
                    *sample = cut + (*sample - cut) * s;
                }
                let old = if warm {
                    old + (cut_old.run(old) - old) * s
                } else {
                    old
                };
                *sample = *sample * fade_in + old * fade_out;
            }
        }
    }
}

/// The sweep's low-passes on the outgoing and high-passes on the incoming,
/// a channel each.
struct Sweep {
    skew: bool,
    rate: u32,
    old: [Cut; 2],
    new: [Cut; 2],
}

impl Sweep {
    /// The outgoing's low-pass falls from open as the incoming's high-pass
    /// opens. Each starts or ends dry, where its filter is open, so nothing
    /// steps, and the incoming ends whole.
    fn mix<'a>(&mut self, frames: impl Iterator<Item = Frame<'a>>, step: f32, old_gain: f32) {
        let rate = self.rate;
        let glide = |from: f64, to: f64, t: f32| from * (to / from).powf(f64::from(t));
        // Both channels share a corner, so the second takes the first's.
        let tune = |cuts: &mut [Cut; 2], pass, hz| {
            cuts[0].tune(pass, hz, rate);
            cuts[1] = Cut {
                z: cuts[1].z,
                ..cuts[0]
            };
        };
        for ((new, old), frame) in frames {
            let t = frame as f32 * step;
            // Once open, the incoming's high-pass is never run again, nor the
            // outgoing's low-pass once a skewed fade has it out.
            let opening = t < OPEN_BY;
            let fading = !self.skew || t < SKEW;
            // By the blend's frame, so a split packet tunes as a whole one.
            if frame.is_multiple_of(RETUNE_FRAMES) {
                if fading {
                    let low = glide(open_hz(rate), SWEEP_TO_HZ, t);
                    tune(&mut self.old, Pass::Low, low);
                }
                if opening {
                    let high = glide(SWEEP_FROM_HZ, OPENED_HZ, t / OPEN_BY);
                    tune(&mut self.new, Pass::High, high);
                }
            }
            let (fade_in, fade_out) = if self.skew {
                let fade_in = equal_power(((t - (1.0 - SKEW)) / SKEW).max(0.0)).0;
                (fade_in, equal_power((t / SKEW).min(1.0)).1)
            } else {
                equal_power(t)
            };
            let fade_out = fade_out * old_gain;
            let filtered = ease(t / FILTERED_BY);
            let dry = ease((t - (OPEN_BY - FILTERED_BY)) / FILTERED_BY);
            let cuts = self.new.iter_mut().zip(&mut self.old);
            for ((sample, &old), (cut_new, cut_old)) in new.iter_mut().zip(old).zip(cuts) {
                if opening {
                    let high = cut_new.run(*sample);
                    *sample = high + (*sample - high) * dry;
                }
                *sample *= fade_in;
                if fading {
                    let low = cut_old.run(old);
                    *sample += (old + (low - old) * filtered) * fade_out;
                }
            }
        }
    }
}

/// The echo out of `beat`s over the blend's `frames`: its delay line,
/// interleaved, each channel's damping, and the frames the beat sent to it
/// fades in and out over.
struct Echo {
    frames: usize,
    beat: usize,
    line: Vec<f32>,
    damp: [(Lows, Lows); 2],
    edge: f32,
}

impl Echo {
    /// The outgoing's first beat fades as the incoming comes in, and repeats
    /// a beat apart, each echo darker and thinner, rung out by the end.
    fn mix<'a>(&mut self, frames: impl Iterator<Item = Frame<'a>>, old_gain: f32) {
        let (len, beat, edge) = (self.frames, self.beat, self.edge);
        for ((new, old), frame) in frames {
            // Only the first beat fades and is sent, and only the last rings
            // out; a blend of a beat does both.
            let (fade_in, dry, send) = if frame < beat {
                let (fade_in, dry) = equal_power(frame as f32 / beat as f32);
                // Eased in and out at the beat's ends, so the repeats don't click.
                let send = ease((frame + 1) as f32 / edge) * ease((beat - frame) as f32 / edge);
                (fade_in, dry, send)
            } else {
                (1.0, 0.0, 0.0)
            };
            let left = len - frame;
            let ring = if left < beat {
                (left as f32 / beat as f32 * FRAC_PI_2).sin()
            } else {
                1.0
            };
            let at = frame % beat * new.len();
            let line = &mut self.line[at..at + new.len()];
            for (((sample, &old), wet), (lows, highs)) in
                new.iter_mut().zip(old).zip(line).zip(&mut self.damp)
            {
                let echo = *wet;
                let fed = lows.run(old * send + FEEDBACK * echo);
                *wet = fed - highs.run(fed);
                *sample = *sample * fade_in + (old * dry + echo * ring) * old_gain;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::engine::varispeed;

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

    /// Stereo `secs` long, each sample `at(frame)`.
    fn stereo(secs: f64, mut at: impl FnMut(usize) -> f32) -> Vec<f32> {
        (0..(secs * f64::from(RATE)) as usize * 2)
            .map(|i| at(i / 2))
            .collect()
    }

    /// Steady stereo noise at `gain(t)`, `t` in seconds.
    fn noise(secs: f64, gain: impl Fn(f64) -> f32) -> Vec<f32> {
        let mut seed = 1u32;
        stereo(secs, |frame| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let x = (seed >> 8) as f32 / (1 << 24) as f32 - 0.5;
            x * gain(frame as f64 / f64::from(RATE))
        })
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

    fn tempo(beats: Beats) -> f64 {
        60.0 / secs(beats.period)
    }

    fn analysed(samples: &[f32]) -> Analysis {
        analyse(samples, RATE, 2)
    }

    fn plan(tail: &Analysis, intro: &Analysis) -> Mix {
        super::plan(tail, intro, RATE, Voices::default())
    }

    /// The blend, in seconds, from `tail` into a steady loud intro.
    fn blend_secs(tail: &[f32]) -> f64 {
        let intro = analysed(&noise(12.0, |_| 1.0));
        secs(plan(&analysed(tail), &intro).frames as f64)
    }

    /// How far `at` lies from the nearest of `every` from `from`, in seconds.
    fn off_grid(at: f64, from: f64, every: f64) -> f64 {
        let phase = (at - from).rem_euclid(every);
        phase.min(every - phase)
    }

    /// 40 dB down over the last 10 s of 20.
    fn fade_out(t: f64) -> f32 {
        10f32.powf(-2.0 * ((t - 10.0).max(0.0) / 10.0) as f32)
    }

    /// 40 dB down over the last 16 s of 20, so the blend runs long.
    fn long_fade(t: f64) -> f32 {
        10f32.powf(-2.0 * ((t - 4.0).max(0.0) / 16.0) as f32)
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
            let beats = analysed(&clicks(bpm, 0.13, 20.0)).beats.unwrap();
            let found = tempo(beats);
            // Clicks at 174 are as much 87 in half time.
            let heard = if found < bpm * 0.75 { bpm / 2.0 } else { bpm };
            assert!(
                (found - heard).abs() / heard < 0.002,
                "{bpm}: found {found}"
            );
            // On a click, which in half time may be either of a pair.
            let off = off_grid(secs(beats.first), 0.13, 60.0 / bpm);
            assert!(off < 0.003, "{bpm}: {beats:?}");
        }
    }

    #[test]
    fn hears_no_beat_in_noise_or_silence() {
        let beats = analysed(&noise(20.0, |_| 1.0)).beats;
        assert!(beats.is_none(), "noise: {beats:?}");
        let silence = analysed(&vec![0.0; 48_000 * 20]);
        assert!(silence.beats.is_none(), "silence: {:?}", silence.beats);
        assert!(!silence.bass, "silence carries no bass");
        let beats = analysed(&clicks(120.0, 0.0, 1.0)).beats;
        assert!(beats.is_none(), "too short: {beats:?}");
    }

    /// A sine at `hz` and `gain`, `secs` long, in stereo.
    fn bass(hz: f32, secs: f64, gain: f32) -> Vec<f32> {
        stereo(secs, |frame| {
            gain * (TAU * hz * frame as f32 / RATE as f32).sin()
        })
    }

    /// A sawtooth at `hz` and `gain`, `secs` long, in stereo.
    fn saw(hz: f32, secs: f64, gain: f32) -> Vec<f32> {
        stereo(secs, |frame| {
            gain * (2.0 * (hz * frame as f32 / RATE as f32).fract() - 1.0)
        })
    }

    #[test]
    fn hears_no_beat_in_a_held_bass_note() {
        for hz in [35.0, 55.0, 110.0] {
            let beats = analysed(&bass(hz, 20.0, 0.5)).beats;
            assert!(beats.is_none(), "{hz} Hz: {beats:?}");
        }
        // A sawtooth's energy, and a fifth's over its root, ripple once a
        // waveform, regular as a beat.
        for hz in [30.0, 41.2, 55.0] {
            let fifth = mixed(&[saw(hz, 20.0, 0.4), saw(hz * 1.5, 20.0, 0.3)]);
            for samples in [saw(hz, 20.0, 0.5), fifth] {
                let beats = analysed(&samples).beats;
                assert!(beats.is_none(), "{hz} Hz: {beats:?}");
            }
        }
    }

    #[test]
    fn hears_clicks_over_a_held_bass_note() {
        let samples = mixed(&[clicks(124.0, 0.13, 20.0), bass(55.0, 20.0, 0.3)]);
        let beats = analysed(&samples).beats.unwrap();
        let found = tempo(beats);
        assert!((found - 124.0).abs() < 0.25, "found {found}");
        assert!(
            off_grid(secs(beats.first), 0.13, 60.0 / 124.0) < 0.006,
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
        let tail = analysed(&noise(20.0, |_| 1.0));
        // Up to loud only in the intro's last half second, too.
        for quiet in [7.0, 11.5] {
            let intro = analysed(&noise(12.0, |t| if t < quiet { 0.1 } else { 1.0 }));
            let mix = plan(&tail, &intro);
            assert!((secs(mix.frames as f64) - quiet).abs() < 0.4, "{mix:?}");
        }
    }

    #[test]
    fn blends_hard_ends_briefly_and_never_past_the_tail() {
        assert_eq!(blend_secs(&noise(20.0, |_| 1.0)), MIN_SECS);
        let loud = analysed(&noise(20.0, |_| 1.0));
        let short = analysed(&noise(2.0, |_| 1.0));
        assert_eq!(plan(&short, &loud).frames, 96_000);
    }

    fn faded_clicks(bpm: f64) -> Vec<f32> {
        shaped(&clicks(bpm, 0.21, 20.0), long_fade)
    }

    #[test]
    fn lands_the_incoming_beat_on_the_outgoing() {
        // A hard end plans the shortest blend and a 16 s fade the longest, so
        // alignment must stay within both bounds whatever the phases.
        let tails = [clicks(124.0, 0.21, 20.0), faded_clicks(124.0)].map(|s| analysed(&s));
        for step in 0..40 {
            let offset = 0.01 + 0.012 * f64::from(step);
            let intro = analysed(&clicks(124.0, offset, 12.0));
            let next = intro.beats.unwrap();
            for (which, tail) in tails.iter().enumerate() {
                let mix = plan(tail, &intro);
                // Against where the clicks really are, not where they were heard.
                let start = secs(tail.frames as f64 - mix.frames as f64);
                let off = off_grid(start + offset, 0.21, 60.0 / 124.0);
                assert!(off < 0.003, "{which}/{step}: {off} s off the beat");
                let len = secs(mix.frames as f64);
                assert!(
                    (MIN_SECS..=MAX_SECS).contains(&len),
                    "{which}/{step}: {len} s"
                );
                // Traded over half a beat, ending on an incoming beat.
                let Style::Blend {
                    trade: Some((trade_at, trade_len)),
                } = mix.style
                else {
                    panic!("{which}/{step}: {mix:?}");
                };
                assert_eq!(trade_len, (next.period / 2.0) as usize);
                let end = (trade_at + trade_len) as f64 - next.first;
                assert!(
                    (end / next.period - (end / next.period).round()).abs() < 0.01,
                    "{which}/{step}: the trade ends {end} frames from the first beat"
                );
            }
        }
    }

    /// A kick at `bpm` from 0.13 s, the `k`th at `gain(k)`: a falling 60 Hz
    /// thump over 150 ms.
    fn kicks(bpm: f64, secs: f64, gain: impl Fn(usize) -> f32) -> Vec<f32> {
        let period = 60.0 / bpm;
        stereo(secs, |frame| {
            let t = frame as f64 / f64::from(RATE) - 0.13;
            let since = t.rem_euclid(period) as f32;
            if t < 0.0 {
                0.0
            } else {
                gain((t / period) as usize) * (-since / 0.05).exp() * (TAU * 60.0 * since).sin()
            }
        })
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
            let samples = mixed(&[kicks(bpm, 20.0, |_| 0.6), bass, noise(20.0, |_| 0.2)]);
            let beats = analysed(&samples).beats.unwrap();
            let found = fold(tempo(beats), bpm);
            assert!((found - bpm).abs() / bpm < 0.002, "{bpm}: {beats:?}");
        }
    }

    #[test]
    fn leaves_a_tempo_that_wanders() {
        let mut samples = clicks(120.0, 0.13, 10.0);
        samples.extend(clicks(123.0, 0.0, 10.0));
        let beats = analysed(&samples).beats;
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
        let beats = analysed(&ramp).beats;
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
                gated(&kicks(124.0, long, |_| 0.6), kicking.clone()),
                saw(41.2, long, 0.25),
                noise(long, |_| 0.2),
            ]);
            let beats = analysed(&samples).beats.unwrap();
            assert!((tempo(beats) - 124.0).abs() < 0.25, "{beats:?}");
            // On the kicks, where they play.
            let middle = (kicking.start + kicking.end) / 2.0;
            let k = ((middle - secs(beats.first)) / secs(beats.period)).round();
            let at = secs(beats.first + k * beats.period);
            assert!(off_grid(at, 0.13, 60.0 / 124.0) < 0.012, "{beats:?}");
        }
    }

    #[test]
    fn blends_over_a_slow_fade_of_music() {
        // 40 dB over 30 s, which the music's own swell slows to under 1 dB in
        // some seconds.
        let music = mixed(&[
            kicks(124.0, 24.0, |_| 0.6),
            saw(41.2, 24.0, 0.25),
            noise(24.0, |_| 0.2),
        ]);
        let faded = shaped(&music, |t| 10f32.powf((-2.0 * t / 30.0) as f32));
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
            &mixed(&[kicks(124.0, 20.0, |_| 0.6), bass(41.0, 20.0, 0.15)]),
            |t| if (8.0..9.0).contains(&t) { 0.01 } else { 1.0 },
        );
        let beats = analysed(&samples).beats.unwrap();
        assert!((tempo(beats) - 124.0).abs() < 0.25, "{beats:?}");
        assert!(
            off_grid(secs(beats.first), 0.13, 60.0 / 124.0) < 0.012,
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
        let beats = analysed(&samples).beats;
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
        let beats = analysed(&samples).beats.unwrap();
        assert!(
            off_grid(secs(beats.first), 0.13, 60.0 / 124.0) < 0.003,
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
        let tail = analysed(&noise(20.0, fade_out));
        let mix = plan(&tail, &analysed(&noise(5.0, |_| 1.0)));
        assert_eq!(secs(mix.frames as f64), 2.5);
    }

    #[test]
    fn leaves_beats_that_would_drift_apart() {
        // 6.7% apart, past what varispeed takes: a track that stops is echoed
        // out over its last four beats, one that fades swept across.
        let intro = analysed(&clicks(128.0, 0.37, 12.0));
        let hard = clicks(120.0, 0.21, 20.0);
        let mix = plan(&analysed(&hard), &intro);
        let Style::Echo { beat } = mix.style else {
            panic!("{mix:?}");
        };
        assert!(beat.abs_diff(24_000) < 50, "{mix:?}");
        assert!(mix.warp.is_none());
        assert!((3.0..=4.0).contains(&(mix.frames as f64 / beat as f64)));
        let mix = plan(&analysed(&shaped(&hard, fade_out)), &intro);
        assert_eq!(mix.style, Style::Sweep { skew: false }, "{mix:?}");
        assert!(mix.warp.is_none());
    }

    #[test]
    fn folds_double_time_onto_the_beat() {
        assert_eq!(fold(250.0, 500.0), 500.0);
        assert_eq!(fold(1_000.0, 500.0), 500.0);
        assert_eq!(fold(600.0, 500.0), 600.0);
    }

    /// Kicks as `kicks` plays them over a little noise, each bar's `phase`th
    /// beat of four accented.
    fn accented(bpm: f64, secs: f64, phase: usize) -> Vec<f32> {
        let gain = |k: usize| if k % 4 == phase { 0.6 } else { 0.25 };
        mixed(&[kicks(bpm, secs, gain), noise(secs, |_| 0.05)])
    }

    #[test]
    fn finds_the_downbeat_of_an_accented_bar() {
        let beat = 60.0 / 124.0;
        for phase in 0..4 {
            let beats = analysed(&accented(124.0, 20.0, phase)).beats.unwrap();
            let downbeat = secs(beats.downbeat.expect("bars"));
            let off = off_grid(downbeat, 0.13 + phase as f64 * beat, 4.0 * beat);
            assert!(off < 0.012, "{phase}: {beats:?}");
        }
        // Even beats have no bars.
        let even = mixed(&[kicks(124.0, 20.0, |_| 0.6), noise(20.0, |_| 0.05)]);
        for samples in [even, clicks(124.0, 0.13, 20.0)] {
            let beats = analysed(&samples).beats.unwrap();
            assert!(beats.downbeat.is_none(), "{beats:?}");
        }
    }

    #[test]
    fn lands_the_first_downbeat_on_a_downbeat() {
        let (beat, bar) = (60.0 / 124.0, 4.0 * 60.0 / 124.0);
        let hard = accented(124.0, 20.0, 1);
        let faded = shaped(&hard, long_fade);
        for (which, tail) in [hard, faded].iter().enumerate() {
            let tail = analysed(tail);
            for phase in 0..4 {
                let intro = analysed(&accented(124.0, 12.0, phase));
                let next = intro.beats.unwrap();
                let mix = plan(&tail, &intro);
                let start = secs(tail.frames as f64 - mix.frames as f64);
                // Against where the downbeats really are.
                let lands = start + 0.13 + phase as f64 * beat;
                let off = off_grid(lands, 0.13 + beat, bar);
                assert!(off < 0.012, "{which}/{phase}: {off} s off, {mix:?}");
                // Traded on an incoming bar line.
                let Style::Blend {
                    trade: Some((at, len)),
                } = mix.style
                else {
                    panic!("{which}/{phase}: {mix:?}");
                };
                let end = secs((at + len) as f64 - next.downbeat.unwrap());
                assert!(off_grid(end, 0.0, bar) < 0.001, "{which}/{phase}: {mix:?}");
            }
        }
    }

    #[test]
    fn prefers_a_phrase_of_four_or_eight_bars() {
        let bar = 2.0 * f64::from(RATE);
        let sync = Sync {
            speed: 1.0,
            ramp: 0.0,
            longest: 12.0 * f64::from(RATE),
            beats: Lengths {
                at: 0.0,
                step: bar / 4.0,
            },
            bars: Some((
                Lengths {
                    at: 0.3 * bar,
                    step: bar,
                },
                0.0,
            )),
        };
        let pick =
            |sync: Sync, want: f64| sync.pick(3.0 * f64::from(RATE), f64::INFINITY, want * bar);
        let on_bar = |k: f64| Some((0.3 * bar + k * bar, true));
        assert_eq!(pick(sync, 3.0), on_bar(3.0), "the nearest");
        assert_eq!(pick(sync, 3.6), on_bar(4.0), "4 bars over 3");
        assert_eq!(pick(sync, 5.2), on_bar(5.0), "but not by over half a bar");
        assert_eq!(pick(sync, 4.5), on_bar(4.0));
        let late = Sync {
            bars: sync.bars.map(|(bars, _)| (bars, 3.5 * bar)),
            ..sync
        };
        assert_eq!(pick(late, 3.0), on_bar(4.0), "reaching the first downbeat");
        let beats = Sync { bars: None, ..sync };
        assert_eq!(pick(beats, 3.6), Some((3.5 * bar, false)), "on the beat");
    }

    #[test]
    fn meets_a_near_tempo_by_varispeed() {
        for bpm in [117.0, 124.0] {
            let tail = faded_clicks(120.0);
            let intro = analysed(&clicks(bpm, 0.05, 12.0));
            let next = intro.beats.unwrap();
            let mix = plan(&analysed(&tail), &intro);
            let (start, curve) = mix.warp.expect("varispeed");
            assert!((curve.speed - bpm / 120.0).abs() < 0.002, "{mix:?}");
            assert!(curve.ramp >= RAMP_SECS * f64::from(RATE) * 0.95, "{mix:?}");
            let ramp = curve.ramp as usize;
            let warped: Vec<f32> =
                varispeed::resample(&tail, 2, start, curve, ramp..ramp + mix.frames).collect();
            // The blend at the incoming's tempo, its beats on the incoming's.
            let blend = analysed(&warped).beats.unwrap();
            assert!(
                (blend.period / next.period - 1.0).abs() < 0.002,
                "{bpm}: {blend:?} for {next:?}"
            );
            let off = off_grid(secs(blend.first), secs(next.first), secs(next.period));
            assert!(off < 0.004, "{bpm}: {off} s off the beat");
            // Ending as the tail does.
            let last = start as f64 + curve.song_frames((ramp + mix.frames) as f64);
            assert!((last - (tail.len() / 2) as f64).abs() < 2.0, "{last}");
        }
    }

    #[test]
    fn meets_by_varispeed_where_singing_rules_out_every_beat_as_is() {
        // 1.25% apart, together as is for 4 s: some shorter blends the
        // singing allows land on no beat as is, but on one sped up.
        let tail = analysed(&faded_clicks(120.0));
        let intro = analysed(&clicks(121.5, 0.05, 12.0));
        assert!(plan(&tail, &intro).warp.is_none(), "as is where it can");
        let warped = (0..50)
            .map(|i| super::plan(&tail, &intro, RATE, voices(0.0, 1.5 + 0.01 * f64::from(i))))
            .filter(|mix| mix.warp.is_some())
            .count();
        assert!(warped > 0, "never sped up");
    }

    #[test]
    fn reads_where_each_song_is_sung() {
        let rate = f64::from(RATE);
        let sung = Sung {
            current: Some(vec![10.0..12.0, 170.0..176.0]),
            next: Some(vec![0.2..0.5, 2.0..3.0]),
        };
        let voices = Voices::new(&sung, 180.0, 0.8, RATE);
        assert_eq!(voices.last, Some(4.0 * rate));
        assert_eq!(voices.first, Some(1.2 * rate), "past the cut silence");
        let unknown = Voices::new(&Sung::default(), 180.0, 0.0, RATE);
        assert_eq!(unknown.longest(1.0, rate), None);
        let unsung = Sung {
            current: Some(Vec::new()),
            next: Some(vec![0.2..0.5]),
        };
        let voices = Voices::new(&unsung, 180.0, 0.8, RATE);
        assert_eq!((voices.last, voices.first), (None, None), "no words left");
    }

    fn voices(last: f64, first: f64) -> Voices {
        let rate = f64::from(RATE);
        Voices {
            last: Some(last * rate),
            first: Some(first * rate),
        }
    }

    #[test]
    fn shortens_a_blend_so_the_incoming_sings_after_the_outgoing() {
        let tail = analysed(&noise(20.0, fade_out));
        let intro = analysed(&noise(12.0, |_| 1.0));
        let blend = |voices| super::plan(&tail, &intro, RATE, voices);
        assert_eq!(blend(voices(9.0, 1.0)).frames, plan(&tail, &intro).frames);
        let mix = blend(voices(4.0, 1.0));
        assert_eq!(secs(mix.frames as f64), 4.0 + 1.0 + SUNG_SLACK_SECS);
        assert!(matches!(mix.style, Style::Blend { .. }), "{mix:?}");
        // Too close to part, so swept with the overlap kept short.
        let mix = blend(voices(0.0, 0.5));
        assert_eq!(mix.style, Style::Sweep { skew: true });
        assert_eq!(secs(mix.frames as f64), MIN_SECS);
    }

    #[test]
    fn shortens_a_blend_on_the_beat() {
        let tail = analysed(&faded_clicks(124.0));
        let intro = analysed(&clicks(124.0, 0.05, 12.0));
        let mix = super::plan(&tail, &intro, RATE, voices(5.0, 1.0));
        let len = secs(mix.frames as f64);
        assert!((7.0..=7.5).contains(&len), "{mix:?}");
        let start = secs(tail.frames as f64) - len;
        assert!(
            off_grid(start + 0.05, 0.21, 60.0 / 124.0) < 0.003,
            "{mix:?}"
        );
    }

    fn mixer(frames: usize, style: Style, channels: usize) -> Mixer {
        Mixer::new(frames, style, RATE, channels)
    }

    #[test]
    fn trades_the_bass_rather_than_summing_it() {
        let bass =
            |phase: f32| (0..48_000).map(move |i| (TAU * 50.0 * i as f32 / 48_000.0 + phase).sin());
        let trade = Some((24_000, 4_800));
        let mut mixer = mixer(48_000, Style::Blend { trade }, 1);
        let mut peak = 0f32;
        for (new, old) in bass(0.3).zip(bass(0.0)) {
            let mut frame = [new];
            mixer.mix(&mut frame, &[old], 1.0);
            peak = peak.max(frame[0].abs());
        }
        // Equal-power curves alone would sum the two near 1.4.
        assert!(peak < 1.05, "peaked at {peak}");
    }

    const STYLES: [Style; 4] = [
        Style::Blend {
            trade: Some((2_000, 500)),
        },
        Style::Sweep { skew: false },
        Style::Sweep { skew: true },
        Style::Echo { beat: 1_100 },
    ];

    #[test]
    fn ends_every_style_on_the_incoming_whole() {
        for style in STYLES {
            let mut mixer = mixer(4_800, style, 2);
            let mut frame = [0.0; 2];
            for _ in 0..4_800 {
                frame = [0.5, -0.25];
                mixer.mix(&mut frame, &[0.9, 0.9], 1.0);
            }
            assert!(
                (frame[0] - 0.5).abs() < 1e-3 && (frame[1] + 0.25).abs() < 1e-3,
                "{style:?}: {frame:?}"
            );
        }
    }

    #[test]
    fn mixes_every_style_across_packets_as_in_one() {
        let tail: Vec<f32> = (0..9_600).map(|i| (i as f32 * 0.01).sin()).collect();
        let new: Vec<f32> = (0..9_600).map(|i| (i as f32 * 0.03).cos()).collect();
        for style in STYLES {
            let run = |part: usize| {
                let mut mixer = mixer(4_800, style, 2);
                let mut out = new.clone();
                for (new, old) in out.chunks_mut(part).zip(tail.chunks(part)) {
                    mixer.mix(new, old, 0.5);
                }
                out
            };
            assert_eq!(run(122), run(9_600), "{style:?}");
        }
    }

    #[test]
    fn sweeps_the_outgoing_down_as_the_incoming_opens() {
        // 4 kHz, which the outgoing's low-pass ends far under and the
        // incoming's high-pass lets through from the start.
        let tone = |i: usize| (TAU * 4_000.0 * i as f32 / RATE as f32).sin();
        let style = Style::Sweep { skew: false };
        let (mut old, mut new) = (mixer(48_000, style, 1), mixer(48_000, style, 1));
        let (mut old_late, mut new_late) = (0f32, 0f32);
        for i in 0..48_000 {
            // As the mixer places the frame.
            let t = i as f32 * (1.0 / 48_000.0);
            let (fade_in, fade_out) = equal_power(t);
            let mut frame = [0.0];
            old.mix(&mut frame, &[tone(i)], 1.0);
            if t > 0.8 {
                old_late = old_late.max(frame[0].abs() / fade_out);
            }
            let mut frame = [tone(i)];
            new.mix(&mut frame, &[0.0], 1.0);
            if t >= OPEN_BY {
                new_late = new_late.max((frame[0] - tone(i) * fade_in).abs());
            }
        }
        assert!(old_late < 0.01, "the outgoing at {old_late}");
        assert_eq!(new_late, 0.0, "the incoming whole once open");
    }

    #[test]
    fn echoes_the_first_beat_and_rings_out_by_the_end() {
        let beat = 4_800;
        let mut mixer = mixer(4 * beat, Style::Echo { beat }, 1);
        let mut heard = Vec::new();
        for i in 0..4 * beat {
            let mut frame = [0.0];
            // A tone throughout, which only the first beat should carry on.
            let old = (TAU * 440.0 * i as f32 / RATE as f32).sin() * 0.5;
            mixer.mix(&mut frame, &[old], 1.0);
            heard.push(frame[0]);
        }
        let peak = |beats: std::ops::Range<usize>| {
            heard[beats.start * beat..beats.end * beat]
                .iter()
                .fold(0f32, |m, s| m.max(s.abs()))
        };
        let (second, third) = (peak(1..2), peak(2..3));
        assert!(second > 0.3, "the first echo at {second}");
        assert!(third < second * 0.6 && third > 0.05, "fed back at {third}");
        let end = heard[4 * beat - 48..]
            .iter()
            .fold(0f32, |m, s| m.max(s.abs()));
        assert!(end < 0.01, "rung out to {end}");
    }
}
