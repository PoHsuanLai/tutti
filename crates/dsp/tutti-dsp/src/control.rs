//! Control-rate constants shared by the filters and delay lines.

/// Samples between coefficient solves on a parameter that moves inside a block.
///
/// A filter whose cutoff is swept (by a block ramp or an audio-rate param port)
/// needs a `tan` per coefficient solve. Solving every sample is the dominant
/// cost of a modulated SVF; solving every 16 samples and interpolating the
/// coefficients linearly between solves costs a quarter of a 64-frame block's
/// `tan`s. 16 samples is 0.33 ms at 48 kHz — far below the rate any musical
/// sweep moves at, so the interpolation error is a small fraction of the change
/// between two solves. Measured against the per-sample solve: 3e-5 worst on a
/// 200 Hz → 8 kHz cutoff sweep in 85 ms (4.6e-4 at 64 samples; exactly 0 at
/// 1), 3.5e-6 on a 2 Hz phaser. The `*_swept`
/// goldens in `tutti-nodes`'s `tests/width_generic_golden.rs` pin the result.
pub const COEFF_INTERVAL: usize = 16;
