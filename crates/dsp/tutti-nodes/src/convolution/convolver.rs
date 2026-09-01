//! Mono FFT convolver primitive.
//!
//! Wraps [`fft_convolver::FFTConvolver`] with a `FftPartitionState`
//! scratch block so `process_sample` is allocation-free after `new()`.

use fft_convolver::FFTConvolver;

/// Default FFT block size when the caller doesn't pick one. Must be a
/// power of two; `FFTConvolver::init` rejects anything else.
pub(crate) const DEFAULT_BLOCK_SIZE: usize = 512;

/// Linear scratch blocks the partitioned-FFT convolver drains/fills
/// once per `block_size` samples.
///
/// Not a ring — the input block fills linearly, gets passed wholesale
/// to `FFTConvolver::process`, and resets. The output block is read
/// once per sample and rewritten in full on the next FFT cycle. This
/// shape is distinct enough from [`crate::buffer::CircularBuffer`]
/// that reusing the ring would be a forced fit.
#[derive(Clone)]
struct FftPartitionState {
    block_size: usize,
    input: Vec<f32>,
    input_fill: usize,
    output: Vec<f32>,
    output_cursor: usize,
}

impl FftPartitionState {
    fn new(block_size: usize) -> Self {
        Self {
            block_size,
            input: vec![0.0; block_size],
            input_fill: 0,
            output: vec![0.0; block_size],
            // Start drained — first `block_size` samples read zero (fill-up
            // latency).
            output_cursor: block_size,
        }
    }

    /// Next sample from the already-computed output block, or `0.0` if
    /// the block has been drained.
    #[inline]
    fn next_output(&mut self) -> f32 {
        if self.output_cursor < self.block_size {
            let s = self.output[self.output_cursor];
            self.output_cursor += 1;
            s
        } else {
            0.0
        }
    }

    /// Write `sample` into the input-accumulation block. Returns `true`
    /// when the block is full and `drain` must be run.
    #[inline]
    fn push_input(&mut self, sample: f32) -> bool {
        self.input[self.input_fill] = sample;
        self.input_fill += 1;
        self.input_fill >= self.block_size
    }

    /// Hand the filled input block to `fft`, overwrite the output
    /// block with the result, and reset both cursors.
    #[inline]
    fn drain_through(&mut self, fft: &mut FFTConvolver<f32>) {
        // Buffers are pre-sized to `block_size`, so `process` cannot fail here.
        // On the audio thread we still degrade to silence rather than panic
        // across the callback boundary; `debug_assert` catches a sizing
        // regression in tests.
        let result = fft.process(&self.input, &mut self.output);
        debug_assert!(
            result.is_ok(),
            "FFTConvolver::process failed on pre-sized buffers"
        );
        if result.is_err() {
            self.output.fill(0.0);
        }
        self.input_fill = 0;
        self.output_cursor = 0;
    }

    fn clear(&mut self) {
        self.input.fill(0.0);
        self.input_fill = 0;
        self.output.fill(0.0);
        self.output_cursor = self.block_size;
    }

    #[inline]
    fn footprint(&self) -> usize {
        (self.input.capacity() + self.output.capacity()) * core::mem::size_of::<f32>()
    }
}

/// Real-time-safe mono convolver.
///
/// Composes three pieces:
/// - the `fft-convolver` FFT engine,
/// - a `FftPartitionState` scratch block sized at construction,
/// - the IR length (for reporting only).
///
/// The hot path (`process_sample`) never allocates.
#[derive(Clone)]
pub struct Convolver {
    fft: FFTConvolver<f32>,
    partition: FftPartitionState,
    ir_length: usize,
}

impl Convolver {
    /// Create a new convolver from an impulse response.
    ///
    /// `block_size` is rounded up to the next power of two.
    pub fn new(ir: &[f32], block_size: usize) -> Self {
        let block_size = block_size.next_power_of_two().max(2);

        let mut fft = FFTConvolver::default();
        fft.init(block_size, ir)
            .expect("FFTConvolver::init rejected a power-of-two block size");

        Self {
            fft,
            partition: FftPartitionState::new(block_size),
            ir_length: ir.len(),
        }
    }

    /// Create with the default block size (`512`).
    pub fn with_ir(ir: &[f32]) -> Self {
        Self::new(ir, DEFAULT_BLOCK_SIZE)
    }

    /// IR length in samples.
    pub fn ir_length(&self) -> usize {
        self.ir_length
    }

    /// Latency introduced by the partitioned FFT (== block size).
    pub fn latency(&self) -> usize {
        self.partition.block_size
    }

    /// Produce one convolved sample. Returns `0.0` for the first
    /// `block_size` samples (fill-up latency).
    #[inline]
    pub fn process_sample(&mut self, input: f32) -> f32 {
        let out = self.partition.next_output();
        if self.partition.push_input(input) {
            self.partition.drain_through(&mut self.fft);
        }
        out
    }

    /// Zero the working buffers without discarding the IR. Safe to
    /// call on a stream discontinuity.
    ///
    /// **Both** halves of the state have to be cleared. Clearing only
    /// `partition` — the local scratch — leaves the `FFTConvolver`'s own
    /// partition buffers holding the previous signal's tail, which then bleeds
    /// into whatever plays next: with an 8-tap IR the first sample after a
    /// reset comes back at 3.5 instead of silence. `reset` is called precisely
    /// at stream discontinuities (a seek, a region change), so that leak lands
    /// exactly where a ghost of the old audio is most audible.
    pub fn reset(&mut self) {
        self.partition.clear();
        self.fft.reset();
    }

    /// Scratch-buffer memory footprint (excludes the FFT engine's own
    /// internal state, which `fft-convolver` does not expose).
    pub(crate) fn scratch_footprint(&self) -> usize {
        self.partition.footprint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Direct (naive) convolution — the definition, used as ground truth.
    ///
    /// `y[n] = sum_k x[k] * h[n-k]`. O(N*M) and far too slow for audio, which is
    /// why the partitioned FFT exists; but for a few thousand samples in a test
    /// it is instant, and it cannot share a bug with the thing it is checking.
    fn direct_convolve(x: &[f32], h: &[f32]) -> Vec<f32> {
        let mut y = vec![0.0f32; x.len() + h.len() - 1];
        for (n, &xn) in x.iter().enumerate() {
            if xn == 0.0 {
                continue;
            }
            for (k, &hk) in h.iter().enumerate() {
                y[n + k] += xn * hk;
            }
        }
        y
    }

    /// Run `x` through the convolver and drop the reported latency.
    fn run(ir: &[f32], x: &[f32], block: usize) -> Vec<f32> {
        let mut c = Convolver::new(ir, block);
        let lat = c.latency();
        // Feed `lat` extra zeros so the tail that latency delays still comes out.
        let mut out: Vec<f32> = x
            .iter()
            .chain(std::iter::repeat_n(&0.0, lat))
            .map(|&s| c.process_sample(s))
            .collect();
        out.drain(..lat);
        out
    }

    /// The convolver must compute a convolution.
    ///
    /// Nothing asserted this. The node's tests check `is_finite()`,
    /// `!is_empty()`, and "produces output after latency" — all of which a
    /// convolver that scaled wrongly, misaligned its partitions, or dropped its
    /// tail would satisfy. This compares against the definition instead.
    ///
    /// Several block sizes, because the partitioning is where a boundary bug
    /// lives: a size that divides the input evenly can hide a fence-post error
    /// that a ragged one exposes.
    #[test]
    fn output_matches_direct_convolution() {
        // A short IR with distinct, non-round values, so a scale error or a
        // reversed kernel produces visibly wrong numbers rather than a plausible
        // rearrangement.
        let ir: Vec<f32> = vec![0.7, -0.35, 0.2, 0.9, -0.1, 0.05, 0.42, -0.6];
        // Input with an impulse, silence, and varying material — the impulse
        // alone would only prove the IR is stored, not that mixing is right.
        let mut x = vec![0.0f32; 600];
        x[3] = 1.0;
        x[100] = -0.5;
        for (i, s) in x.iter_mut().enumerate().skip(200).take(300) {
            *s = ((i as f32) * 0.1).sin() * 0.6;
        }

        for block in [2usize, 8, 64, 512] {
            let got = run(&ir, &x, block);
            let want = direct_convolve(&x, &ir);
            let n = got.len().min(want.len());
            assert!(n >= x.len(), "block {block}: too little output to compare");

            let worst = (0..n).fold(0.0f32, |a, i| a.max((got[i] - want[i]).abs()));
            assert!(
                worst < 1e-4,
                "block {block}: output diverges from direct convolution by {worst} \
                 (first mismatch near sample {})",
                (0..n)
                    .find(|&i| (got[i] - want[i]).abs() > 1e-4)
                    .unwrap_or(0)
            );
        }
    }

    /// A unit impulse IR is the identity, delayed by nothing.
    ///
    /// The simplest possible case, and the one that pins the convolver's *gain*.
    /// An FFT convolver that forgot to normalize its inverse transform scales
    /// every sample by the transform length — inaudible in a null test against
    /// itself, obvious here.
    #[test]
    fn a_unit_impulse_ir_passes_the_signal_through_unchanged() {
        let ir = [1.0f32];
        let x: Vec<f32> = (0..300).map(|i| ((i as f32) * 0.37).sin() * 0.5).collect();

        for block in [2usize, 16, 128] {
            let got = run(&ir, &x, block);
            for (i, (&g, &want)) in got.iter().zip(x.iter()).enumerate() {
                assert!(
                    (g - want).abs() < 1e-5,
                    "block {block}: sample {i} came back as {g}, expected {want} — \
                     a unit-impulse IR must be the identity"
                );
            }
        }
    }

    /// `latency()` must be the delay the output actually has.
    ///
    /// The value is documented as the block size and callers compensate by it,
    /// so a wrong figure smears every convolved track's alignment against the
    /// rest of the mix — silently, because the audio itself is fine.
    #[test]
    fn reported_latency_is_the_real_latency() {
        let ir = [1.0f32];
        for block in [2usize, 8, 64, 512] {
            let mut c = Convolver::new(&ir, block);
            let lat = c.latency();

            // Impulse at 0; with a unit IR the output impulse must land at
            // exactly `lat`.
            let n = lat * 3 + 16;
            let out: Vec<f32> = (0..n)
                .map(|i| c.process_sample(if i == 0 { 1.0 } else { 0.0 }))
                .collect();

            let at = out
                .iter()
                .position(|&s| s.abs() > 0.5)
                .unwrap_or_else(|| panic!("block {block}: the impulse never came out"));
            assert_eq!(
                at, lat,
                "block {block}: latency() reports {lat} but the impulse emerged at {at}"
            );
        }
    }

    /// `reset` must clear the tail, not just the input buffer.
    ///
    /// After a reset the convolver has to behave like a fresh one. If the FFT
    /// partitions keep their contents, the previous signal's tail bleeds into
    /// the next region — audible as a ghost at every discontinuity, which is
    /// exactly when `reset` is called.
    #[test]
    fn reset_clears_the_convolution_tail() {
        let ir: Vec<f32> = vec![0.9, 0.8, 0.7, 0.6, 0.5, 0.4, 0.3, 0.2];
        let block = 8;

        let mut c = Convolver::new(&ir, block);
        // Drive it hard so there is a substantial tail in flight.
        for _ in 0..64 {
            c.process_sample(1.0);
        }
        c.reset();

        // Silence in must give silence out — any non-zero is leftover tail.
        for i in 0..64 {
            let s = c.process_sample(0.0);
            assert!(
                s.abs() < 1e-6,
                "sample {i} after reset is {s}, expected silence — \
                 the previous signal's tail survived the reset"
            );
        }
    }
}
