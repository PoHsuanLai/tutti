//! The phaser's swept first-order all-pass chain: the coefficient for a sweep
//! position and the per-sample stage loop.

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
