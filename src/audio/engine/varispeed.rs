//! Automix's varispeed: the outgoing tail read faster or slower through a
//! windowed-sinc interpolator, pitch following tempo as on a record.

use std::{f64::consts::PI, ops::Range, sync::LazyLock};

use super::automix::MAX_VARISPEED;
use crate::audio::output::Varispeed;

/// The interpolator's taps: 8 either side of the point read.
const TAPS: usize = 16;
/// How many frames before the point read its taps reach.
pub(super) const REACH: usize = TAPS / 2 - 1;
/// Kernels tabulated between two samples, interpolated between, so no tap
/// needs a sine.
const PHASES: usize = 64;
/// The passband, of the input's Nyquist, under the fastest speed's; 16 taps
/// roll off gently, so a little above 20 kHz folds back.
const CUTOFF: f64 = 1.0 / (1.0 + MAX_VARISPEED);
static KERNELS: LazyLock<Vec<[f32; TAPS]>> = LazyLock::new(kernels);

/// The output `frames`, counted from the curve's start, of interleaved
/// `input` read from frame `start` on at `curve`'s speeds, for up to two
/// channels. Past either end reads silence.
pub(super) fn resample(
    input: &[f32],
    channels: usize,
    start: usize,
    curve: Varispeed,
    frames: Range<usize>,
) -> impl Iterator<Item = f32> {
    let table = &*KERNELS;
    let len = (input.len() / channels) as isize;
    frames.flat_map(move |t| {
        let mut kernel = [0f32; TAPS];
        let at = start as f64 + curve.song_frames(t as f64);
        let whole = at.floor();
        let phase = (at - whole) * PHASES as f64;
        let row = (phase as usize).min(PHASES - 1);
        let w = (phase - row as f64) as f32;
        for ((k, a), b) in kernel.iter_mut().zip(&table[row]).zip(&table[row + 1]) {
            *k = a + w * (b - a);
        }
        let first = whole as isize + 1 - (TAPS / 2) as isize;
        let lo = (-first).clamp(0, TAPS as isize) as usize;
        let hi = (len - first).clamp(lo as isize, TAPS as isize) as usize;
        let from = (first + lo as isize).clamp(0, len) as usize * channels;
        let taps = &input[from..from + (hi - lo) * channels];
        let mut frame = [0f32; 2];
        for (k, sample) in kernel[lo..hi].iter().zip(taps.chunks_exact(channels)) {
            for (f, s) in frame.iter_mut().zip(sample) {
                *f += k * s;
            }
        }
        frame.into_iter().take(channels)
    })
}

/// The kernel at each of `PHASES + 1` points from one sample to the next: a
/// Lanczos-windowed sinc, each summing to 1 so no phase changes the level.
fn kernels() -> Vec<[f32; TAPS]> {
    let sinc = |x: f64| {
        if x == 0.0 {
            1.0
        } else {
            (PI * x).sin() / (PI * x)
        }
    };
    let half = (TAPS / 2) as f64;
    (0..=PHASES)
        .map(|p| {
            let frac = p as f64 / PHASES as f64;
            let taps: [f64; TAPS] = std::array::from_fn(|j| {
                let d = j as f64 - (half - 1.0) - frac;
                CUTOFF * sinc(CUTOFF * d) * sinc(d / half)
            });
            let sum: f64 = taps.iter().sum();
            taps.map(|k| (k / sum) as f32)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::f64::consts::TAU;

    use super::*;

    const RATE: f64 = 48_000.0;

    /// A stereo sine at `hz`, the right side inverted.
    fn sine(hz: f64, frames: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                let s = (TAU * hz * i as f64 / RATE).sin() as f32 * 0.5;
                [s, -s]
            })
            .collect()
    }

    #[test]
    fn reads_a_sine_at_the_speeds_asked() {
        let input = sine(1_000.0, 96_000);
        let curve = Varispeed {
            ramp: 24_000.0,
            speed: 1.06,
        };
        let out: Vec<f32> = resample(&input, 2, 1_000, curve, 0..48_000).collect();
        assert_eq!(out.len(), 96_000);
        // In parts as in one.
        let parts: Vec<f32> = resample(&input, 2, 1_000, curve, 0..20_000)
            .chain(resample(&input, 2, 1_000, curve, 20_000..48_000))
            .collect();
        assert!(parts == out, "split");
        for (t, frame) in out.as_chunks::<2>().0.iter().enumerate() {
            let at = 1_000.0 + curve.song_frames(t as f64);
            let want = (TAU * 1_000.0 * at / RATE).sin() as f32 * 0.5;
            assert!(
                (frame[0] - want).abs() < 2e-3,
                "{t}: {} for {want}",
                frame[0]
            );
            assert_eq!(frame[1], -frame[0], "channels kept apart");
        }
    }

    #[test]
    fn plays_on_unchanged_at_speed() {
        // Below the cutoff, so the passband passes it as it is.
        let input = sine(5_000.0, 4_800);
        let curve = Varispeed {
            ramp: 1.0,
            speed: 1.0,
        };
        let out: Vec<f32> = resample(&input, 2, 0, curve, 0..4_800).collect();
        for (i, (a, b)) in out
            .iter()
            .zip(&input)
            .enumerate()
            .skip(32)
            .take(out.len() - 64)
        {
            assert!((a - b).abs() < 2e-3, "{i}: {a} for {b}");
        }
    }

    #[test]
    fn reads_silence_past_either_end() {
        let curve = Varispeed {
            ramp: 1.0,
            speed: 1.0,
        };
        let read =
            |input: &[f32], start| resample(input, 1, start, curve, 0..30).collect::<Vec<_>>();
        // As if zeros lay either side, which the taps reach from the start.
        let padded: Vec<f32> = [[0.0; 8], [0.5; 8], [0.0; 8]].concat();
        let out = read(&[0.5; 8], 0);
        assert_eq!(out, read(&padded, 8));
        assert!((out[4] - 0.5).abs() < 0.05, "{}", out[4]);
        assert_eq!(out[16..], [0.0; 14], "past the end by its taps");
    }
}
