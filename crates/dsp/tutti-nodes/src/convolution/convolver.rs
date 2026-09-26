//! Mono FFT convolver primitive: a uniformly partitioned FFT convolution
//! whose transformed impulse response is **stored once**, in an [`IrSpectra`]
//! behind an `Arc`, and shared by every convolver built from it.
//!
//! The partitioned convolution is the one `fft-convolver` 0.3 runs (its
//! `FFTConvolver`, which this crate used until the native port), written here
//! over the same `realfft` transforms with the same arithmetic in the same
//! order: an output is bit-identical to it (checked bit for bit against
//! `fft-convolver` 0.3 when this landed, IRs of 0 to 5000 samples at
//! partitions of 2 to 512; the node's pinned goldens hold it since). It is owned here because
//! `fft-convolver` keeps the IR's spectra inside each convolver with no way to
//! share them, so every channel of a shared-IR node and every fork of a node
//! held its own copy — megabytes for a long reverb (design doc 013, Phase 4).
//! Now a channel, a clone and a fork hold one `Arc` to the spectra, which
//! nothing writes after they are built; each keeps only its own running state
//! (the input spectra history, the overlap, the FFT scratch).

use std::sync::Arc;

use realfft::num_complex::Complex;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};

/// Default FFT block size when the caller doesn't pick one. A power of two,
/// as every partition size here is.
pub(crate) const DEFAULT_BLOCK_SIZE: usize = 512;

/// Linear scratch blocks the partitioned-FFT convolver drains/fills
/// once per `block_size` samples.
///
/// Not a ring — the input block fills linearly, gets passed wholesale
/// to [`Partitioned::process`], and resets. The output block is read
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
    fn drain_through(&mut self, fft: &mut Partitioned) {
        fft.process(&self.input, &mut self.output);
        self.input_fill = 0;
        self.output_cursor = 0;
    }

    fn clear(&mut self) {
        self.input.fill(0.0);
        self.input_fill = 0;
        self.output.fill(0.0);
        self.output_cursor = self.block_size;
    }
}

/// An impulse response, transformed: one spectrum per `block_size`-long
/// partition, and the two transforms of `2 × block_size` that produced them
/// and that every convolver over it runs. Built once, read-only after that:
/// nothing on the audio thread (or anywhere) writes it, so any number of
/// convolvers — a node's channels, a clone, a fork — share one behind an
/// `Arc`.
pub struct IrSpectra {
    block_size: usize,
    ir_len: usize,
    /// `ceil(ir_len / block_size)` spectra of `block_size + 1` bins; empty
    /// for an empty IR, whose convolution is silence.
    segments: Vec<Vec<Complex<f32>>>,
    forward: Arc<dyn RealToComplex<f32>>,
    inverse: Arc<dyn ComplexToReal<f32>>,
}

impl IrSpectra {
    /// Partition and transform `ir` at `block_size`, which must be a power
    /// of two. Allocates; control thread.
    fn new(ir: &[f32], block_size: usize) -> Self {
        debug_assert!(block_size.is_power_of_two());
        let seg_size = 2 * block_size;
        let mut planner = RealFftPlanner::new();
        let forward = planner.plan_fft_forward(seg_size);
        let inverse = planner.plan_fft_inverse(seg_size);
        let bins = seg_size / 2 + 1;
        let seg_count = ir.len().div_ceil(block_size);
        let mut scratch = forward.make_scratch_vec();
        let mut buffer = vec![0.0f32; seg_size];
        let segments = (0..seg_count)
            .map(|i| {
                let part = &ir[i * block_size..ir.len().min((i + 1) * block_size)];
                copy_and_pad(&mut buffer, part);
                let mut spectrum = vec![Complex::new(0.0, 0.0); bins];
                forward
                    .process_with_scratch(&mut buffer, &mut spectrum, &mut scratch)
                    .expect("buffers sized for the plan");
                spectrum
            })
            .collect();
        Self {
            block_size,
            ir_len: ir.len(),
            segments,
            forward,
            inverse,
        }
    }
}

/// `dst[..src.len()] = src`, the rest zero.
#[inline]
fn copy_and_pad(dst: &mut [f32], src: &[f32]) {
    dst[..src.len()].copy_from_slice(src);
    dst[src.len()..].fill(0.0);
}

/// `result += a × b`, bin by bin: the complex multiply-accumulate of the
/// partitioned convolution, in `fft-convolver`'s operand order.
#[inline]
fn complex_multiply_accumulate(
    result: &mut [Complex<f32>],
    a: &[Complex<f32>],
    b: &[Complex<f32>],
) {
    debug_assert!(result.len() == a.len() && result.len() == b.len());
    for ((r, a), b) in result.iter_mut().zip(a).zip(b) {
        r.re += a.re * b.re - a.im * b.im;
        r.im += a.re * b.im + a.im * b.re;
    }
}

/// One convolver's running state over shared [`IrSpectra`]: the uniformly
/// partitioned overlap-add convolution, one `block_size` block per
/// [`process`](Self::process). Everything here is this convolver's own; the
/// spectra are only read.
struct Partitioned {
    spectra: Arc<IrSpectra>,
    /// The input's spectra, one per IR partition: a ring whose head is
    /// `current`.
    history: Vec<Vec<Complex<f32>>>,
    current: usize,
    /// `2 × block_size`: the padded input going in, the product coming out.
    buffer: Vec<f32>,
    forward_scratch: Vec<Complex<f32>>,
    inverse_scratch: Vec<Complex<f32>>,
    /// The older partitions' products, summed once per block.
    pre_multiplied: Vec<Complex<f32>>,
    conv: Vec<Complex<f32>>,
    /// The second half of the last block's product, added to the next.
    overlap: Vec<f32>,
}

impl Partitioned {
    /// A convolver over `spectra` in its freshly built state. Allocates its
    /// own state (never the spectra's).
    fn new(spectra: Arc<IrSpectra>) -> Self {
        let bs = spectra.block_size;
        let bins = bs + 1;
        let zero = Complex::new(0.0, 0.0);
        Self {
            history: vec![vec![zero; bins]; spectra.segments.len()],
            current: 0,
            buffer: vec![0.0; 2 * bs],
            forward_scratch: spectra.forward.make_scratch_vec(),
            inverse_scratch: spectra.inverse.make_scratch_vec(),
            pre_multiplied: vec![zero; bins],
            conv: vec![zero; bins],
            overlap: vec![0.0; bs],
            spectra,
        }
    }

    /// Convolve one block: `input` and `output` are `block_size` long.
    /// Allocation-free.
    fn process(&mut self, input: &[f32], output: &mut [f32]) {
        let spectra = &*self.spectra;
        let (bs, count) = (spectra.block_size, spectra.segments.len());
        if count == 0 {
            output.fill(0.0);
            return;
        }
        copy_and_pad(&mut self.buffer, input);
        // The transforms cannot fail on buffers sized for their plan. On the
        // audio thread a failure still degrades to silence rather than a
        // panic across the callback; `debug_assert` catches a sizing
        // regression in tests.
        let forward = spectra.forward.process_with_scratch(
            &mut self.buffer,
            &mut self.history[self.current],
            &mut self.forward_scratch,
        );
        debug_assert!(forward.is_ok(), "forward FFT on pre-sized buffers");
        if forward.is_err() {
            output.fill(0.0);
            return;
        }
        let zero = Complex::new(0.0, 0.0);
        self.pre_multiplied.fill(zero);
        for i in 1..count {
            let audio = (self.current + i) % count;
            complex_multiply_accumulate(
                &mut self.pre_multiplied,
                &spectra.segments[i],
                &self.history[audio],
            );
        }
        self.conv.copy_from_slice(&self.pre_multiplied);
        complex_multiply_accumulate(
            &mut self.conv,
            &self.history[self.current],
            &spectra.segments[0],
        );
        let inverse = spectra.inverse.process_with_scratch(
            &mut self.conv,
            &mut self.buffer,
            &mut self.inverse_scratch,
        );
        debug_assert!(inverse.is_ok(), "inverse FFT on pre-sized buffers");
        if inverse.is_err() {
            output.fill(0.0);
            return;
        }
        // The inverse is unnormalised.
        let len = self.buffer.len() as f32;
        for x in &mut self.buffer {
            *x /= len;
        }
        for ((o, &y), &v) in output.iter_mut().zip(&self.buffer[..bs]).zip(&self.overlap) {
            *o = y + v;
        }
        self.overlap.copy_from_slice(&self.buffer[bs..]);
        self.current = if self.current > 0 {
            self.current - 1
        } else {
            count - 1
        };
    }

    /// Clear the running state; the spectra stay.
    fn reset(&mut self) {
        let zero = Complex::new(0.0, 0.0);
        self.buffer.fill(0.0);
        for h in &mut self.history {
            h.fill(zero);
        }
        self.pre_multiplied.fill(zero);
        self.conv.fill(zero);
        self.overlap.fill(0.0);
        self.current = 0;
    }
}

impl Clone for Partitioned {
    /// Shares the spectra; copies the running state.
    fn clone(&self) -> Self {
        Self {
            spectra: Arc::clone(&self.spectra),
            history: self.history.clone(),
            current: self.current,
            buffer: self.buffer.clone(),
            forward_scratch: self.forward_scratch.clone(),
            inverse_scratch: self.inverse_scratch.clone(),
            pre_multiplied: self.pre_multiplied.clone(),
            conv: self.conv.clone(),
            overlap: self.overlap.clone(),
        }
    }
}

/// Real-time-safe mono convolver.
///
/// Composes two pieces:
/// - the partitioned FFT convolution over the IR's [`IrSpectra`], which it
///   shares (a `Clone` shares them, and so does [`fresh`](Self::fresh));
/// - a `FftPartitionState` scratch block sized at construction.
///
/// The hot path (`process_sample`, `process_block`) never allocates.
#[derive(Clone)]
pub struct Convolver {
    fft: Partitioned,
    partition: FftPartitionState,
}

impl Convolver {
    /// Create a new convolver from an impulse response.
    ///
    /// `block_size` is rounded up to the next power of two.
    pub fn new(ir: &[f32], block_size: usize) -> Self {
        let block_size = block_size.next_power_of_two().max(2);
        Self::over(Arc::new(IrSpectra::new(ir, block_size)))
    }

    /// A convolver over `spectra`, in its freshly built state.
    fn over(spectra: Arc<IrSpectra>) -> Self {
        let block_size = spectra.block_size;
        Self {
            fft: Partitioned::new(spectra),
            partition: FftPartitionState::new(block_size),
        }
    }

    /// A convolver over this one's IR, sharing its spectra, in the state of
    /// a freshly built one: nothing of this one's running state is copied.
    /// Allocates the new state (the spectra are not copied).
    pub fn fresh(&self) -> Self {
        Self::over(Arc::clone(&self.fft.spectra))
    }

    /// The IR's spectra this convolver reads, shared.
    pub fn spectra(&self) -> &Arc<IrSpectra> {
        &self.fft.spectra
    }

    /// Create with the default block size (`512`).
    pub fn with_ir(ir: &[f32]) -> Self {
        Self::new(ir, DEFAULT_BLOCK_SIZE)
    }

    /// IR length in samples.
    pub fn ir_length(&self) -> usize {
        self.fft.spectra.ir_len
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

    /// Convolve a block: `output[i]` is what `process_sample(input[i])` would
    /// have returned, for every `i`. `input` and `output` must be the same
    /// length.
    ///
    /// The same FFT runs on the same data at the same frame as the per-sample
    /// path, so the two are bit-identical; this one moves whole runs with
    /// `copy_from_slice` instead of paying a branch and two index bumps per
    /// sample. A run ends at the partition boundary, which is where the FFT has
    /// to fire before the next sample's output exists.
    ///
    /// The run arithmetic leans on one invariant of `FftPartitionState`: once
    /// the first partition has drained, the output cursor and the input fill
    /// advance in lockstep, so a run that fits in the input block also fits in
    /// the output block. Before that first drain the cursor sits at
    /// `block_size` (the fill-up latency) and the run reads silence.
    #[inline]
    pub fn process_block(&mut self, input: &[f32], output: &mut [f32]) {
        debug_assert_eq!(input.len(), output.len());
        let p = &mut self.partition;
        let mut done = 0;
        while done < input.len() {
            let run = (p.block_size - p.input_fill).min(input.len() - done);
            let out = &mut output[done..done + run];
            if p.output_cursor < p.block_size {
                debug_assert!(p.output_cursor + run <= p.block_size);
                out.copy_from_slice(&p.output[p.output_cursor..p.output_cursor + run]);
                p.output_cursor += run;
            } else {
                out.fill(0.0);
            }
            p.input[p.input_fill..p.input_fill + run].copy_from_slice(&input[done..done + run]);
            p.input_fill += run;
            if p.input_fill >= p.block_size {
                p.drain_through(&mut self.fft);
            }
            done += run;
        }
    }

    /// Zero the working buffers without discarding the IR. Safe to
    /// call on a stream discontinuity.
    ///
    /// **Both** halves of the state have to be cleared. Clearing only
    /// `partition` — the local scratch — leaves the convolution's own
    /// partition buffers holding the previous signal's tail, which then bleeds
    /// into whatever plays next: with an 8-tap IR the first sample after a
    /// reset comes back at 3.5 instead of silence. `reset` is called precisely
    /// at stream discontinuities (a seek, a region change), so that leak lands
    /// exactly where a ghost of the old audio is most audible.
    pub fn reset(&mut self) {
        self.partition.clear();
        self.fft.reset();
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

    /// The block path is the per-sample path, bit for bit, whatever the block
    /// size and however the calls straddle the partition boundary.
    ///
    /// Ragged call lengths (1, 7, 64, 3, …) against a small partition are what
    /// put a run boundary mid-call, before the first drain, and exactly on the
    /// drain — the three places the lockstep argument in `process_block` could
    /// be wrong.
    ///
    /// Mutation: dropping `p.output_cursor += run` (or reading
    /// `p.output[..run]` instead of from the cursor) makes the block path
    /// repeat the head of each output partition and fails this on the first
    /// call after the first drain.
    #[test]
    fn block_path_is_bit_identical_to_the_per_sample_path() {
        let ir: Vec<f32> = vec![0.7, -0.35, 0.2, 0.9, -0.1, 0.05, 0.42, -0.6, 0.3];
        let x: Vec<f32> = (0..700)
            .map(|i| ((i as f32) * 0.173).sin() * 0.8 + if i % 97 == 0 { 0.5 } else { 0.0 })
            .collect();
        for block in [2usize, 8, 64] {
            let mut per_sample = Convolver::new(&ir, block);
            let want: Vec<f32> = x.iter().map(|&s| per_sample.process_sample(s)).collect();

            let mut blocked = Convolver::new(&ir, block);
            let mut got = vec![0.0f32; x.len()];
            let lens = [1usize, 7, 64, 3, 16, 33, 5, 64, 9];
            let (mut at, mut k) = (0, 0);
            while at < x.len() {
                let n = lens[k % lens.len()].min(x.len() - at);
                blocked.process_block(&x[at..at + n], &mut got[at..at + n]);
                at += n;
                k += 1;
            }
            for i in 0..x.len() {
                assert_eq!(
                    got[i].to_bits(),
                    want[i].to_bits(),
                    "partition {block}: sample {i} differs ({} vs {})",
                    got[i],
                    want[i]
                );
            }
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
