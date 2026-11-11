//! The phaser's swept first-order all-pass chain: the coefficient for a sweep
//! position and the per-sample stage loop.

use tutti_types::{Depth, Hz};

/// The all-pass centre frequency for one LFO value: the sweep range's `min`
/// at the LFO's trough, its `max` at the peak, scaled toward `min` by `depth`.
///
/// `lfo` is the oscillator's raw value in `-1..=1`; it is mapped to `0..=1`
/// (`lfo * 0.5 + 0.5`) first, and the sweep is `min + (max - min) * lfo01 *
/// depth`. A `depth` of 0 parks the centre at `min`, 1 sweeps the whole range.
/// The result is not held to any band: [`allpass_coeff`] clamps it.
#[inline]
#[must_use]
pub fn sweep_hz(min: Hz, max: Hz, lfo: f32, depth: Depth) -> Hz {
    let lfo = lfo * 0.5 + 0.5;
    let (min, max) = (min.get(), max.get());
    Hz::new(min + (max - min) * lfo * depth.get())
}

/// The lowest frequency an all-pass stage is tuned to (see [`allpass_coeff`]).
const SWEEP_FLOOR_HZ: f32 = 1.0;

/// One first-order all-pass coefficient for a sweep position.
///
/// `tan` of the normalised centre, folded into the all-pass form. It is solved
/// only at the control points — every 16 samples and a block's last sample —
/// and interpolated between, rather than per sample and channel.
///
/// The sweep is held inside `SWEEP_FLOOR_HZ..=0.49 · sr` first. Outside it
/// `tan` of the normalised frequency is negative (a sweep below 0 Hz, which a
/// negative `Depth` reaches: `set_depth` allows down to −1) or past its
/// pole (above Nyquist), `|c|` exceeds 1 and the all-pass diverges. Every
/// sweep a depth in `0..=1` makes lies in `[min_hz, max_hz]`, inside the hold
/// (`min_hz >= 20`, `max_hz <= 0.90` of Nyquist), so the hold changes nothing
/// there.
#[inline]
#[must_use]
pub fn allpass_coeff(sweep_hz: f32, sr: f32) -> f32 {
    let sweep_hz = sweep_hz.max(SWEEP_FLOOR_HZ).min(0.49 * sr);
    let w = core::f32::consts::PI * sweep_hz / sr;
    (w.tan() - 1.0) / (w.tan() + 1.0)
}

/// Runs `lane` (one sample per channel) through `stages` first-order all-pass
/// stages in turn, each stage across every channel in one inner loop.
///
/// `coeffs` holds one coefficient per channel; `x1` and `y1` are the stages'
/// previous input and output, `stages * coeffs.len()` words each, laid out
/// stage-major. Each stage's output becomes the next stage's input, and the
/// last is left in `lane`.
#[inline(always)]
pub fn allpass_chain(
    stages: usize,
    coeffs: &[f32],
    lane: &mut [f32],
    x1: &mut [f32],
    y1: &mut [f32],
) {
    let w = coeffs.len();
    for s in 0..stages {
        let x1 = &mut x1[s * w..(s + 1) * w];
        let y1 = &mut y1[s * w..(s + 1) * w];
        for c in 0..w {
            let input = lane[c];
            let out = coeffs[c] * (input - y1[c]) + x1[c];
            x1[c] = input;
            y1[c] = out;
            lane[c] = out;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sweep at the LFO's trough, centre and peak, for depth 0, half and 1,
    /// from the definition `min + (max - min) * (lfo + 1) / 2 * depth`.
    #[test]
    fn the_sweep_runs_from_min_to_max_by_lfo_and_depth() {
        let (lo, hi) = (Hz(200.0), Hz(4_000.0));
        let at = |lfo: f32, depth: f32| sweep_hz(lo, hi, lfo, Depth(depth)).get();
        // Depth 1: the whole range.
        assert_eq!(at(-1.0, 1.0), 200.0);
        assert_eq!(at(0.0, 1.0), 2_100.0);
        assert_eq!(at(1.0, 1.0), 4_000.0);
        // Depth 0: parked at the floor whatever the LFO does.
        for lfo in [-1.0, 0.0, 1.0] {
            assert_eq!(at(lfo, 0.0), 200.0);
        }
        // Half depth: half the excursion.
        assert_eq!(at(1.0, 0.5), 2_100.0);
        assert_eq!(at(0.0, 0.5), 1_150.0);
    }

    /// A zero-width range does not move whatever the LFO and depth.
    #[test]
    fn an_empty_range_is_a_fixed_frequency() {
        let f = Hz(440.0);
        assert_eq!(sweep_hz(f, f, 0.7, Depth(1.0)).get(), 440.0);
    }
}
