//! Mono FFT convolver primitive: a uniformly partitioned FFT convolution
//! whose transformed impulse response is **stored once**, in an [`IrSpectra`]
//! behind an `Arc`, and shared by every convolver built from it.
//!
//! The partitioned convolution is the one `fft-convolver` 0.3 runs (its
//! `FFTConvolver`), with the same arithmetic in the same order, over
//! [`RealFft`], the engine's CPU-independent real FFT (rustfft's scalar
//! planner, where `fft-convolver`'s `realfft` picks SIMD at run time). An
//! output is therefore bit-identical across CPUs on one OS and libm, and equal
//! to `fft-convolver`'s up to the transforms' rounding. It is written here
//! rather than depended on because
//! `fft-convolver` keeps the IR's spectra inside each convolver with no way to
//! share them, so every channel of a shared-IR node and every fork of a node
//! would hold its own copy — megabytes for a long reverb. Here a channel, a
//! clone and a fork hold one `Arc` to the spectra, which nothing writes after
//! they are built; each keeps only its own running state (the input spectra
//! history, the overlap, the FFT scratch).
//!
//! **The work is spread across the blocks of a partition.** Only the newest
//! partition's multiply, the two transforms and the overlap-add need the
//! input that completes a partition; the products of the older partitions with
//! the older history do not, and they are fixed as soon as the previous
//! partition's transform is done. [`Convolver::process_block`] therefore
//! sums them a few partitions per call, in proportion to the samples
//! consumed, finishing a call's length before the boundary, so a call costs
//! about the same wherever in the cycle it falls and the peak is no longer
//! one call in `partition / block` paying for the whole IR. Each bin still
//! adds its partitions in ascending order from zero, so the output has the
//! same bits as summing everything at the boundary, and the latency is
//! unchanged (one partition).

use std::sync::Arc;

use tutti_types::{Complex32, RealFft};

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
    segments: Vec<Vec<Complex32>>,
    /// Both directions, `2 × block_size` points.
    fft: RealFft,
}

impl IrSpectra {
    /// Partition and transform `ir` at `block_size`, which must be a power
    /// of two. Allocates; control thread.
    fn new(ir: &[f32], block_size: usize) -> Self {
        debug_assert!(block_size.is_power_of_two());
        let seg_size = 2 * block_size;
        let fft = RealFft::new(seg_size);
        let bins = seg_size / 2 + 1;
        let seg_count = ir.len().div_ceil(block_size);
        let mut scratch = fft.make_scratch();
        let mut buffer = vec![0.0f32; seg_size];
        let segments = (0..seg_count)
            .map(|i| {
                let part = &ir[i * block_size..ir.len().min((i + 1) * block_size)];
                copy_and_pad(&mut buffer, part);
                let mut spectrum = vec![Complex32::new(0.0, 0.0); bins];
                fft.forward(&buffer, &mut spectrum, &mut scratch);
                spectrum
            })
            .collect();
        Self {
            block_size,
            ir_len: ir.len(),
            segments,
            fft,
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
fn complex_multiply_accumulate(result: &mut [Complex32], a: &[Complex32], b: &[Complex32]) {
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
    history: Vec<Vec<Complex32>>,
    current: usize,
    /// `2 × block_size`: the padded input going in, the product coming out.
    buffer: Vec<f32>,
    /// Both transforms' scratch.
    scratch: Vec<Complex32>,
    /// The older partitions' products, summed a few partitions at a time
    /// between two boundaries ([`accumulate_to`](Self::accumulate_to)):
    /// partitions `1..=accumulated` are in, and `begun` says the sum was
    /// zeroed for this block.
    pre_multiplied: Vec<Complex32>,
    accumulated: usize,
    begun: bool,
    conv: Vec<Complex32>,
    /// The second half of the last block's product, added to the next.
    overlap: Vec<f32>,
}

impl Partitioned {
    /// A convolver over `spectra` in its freshly built state. Allocates its
    /// own state (never the spectra's).
    fn new(spectra: Arc<IrSpectra>) -> Self {
        let bs = spectra.block_size;
        let bins = bs + 1;
        let zero = Complex32::new(0.0, 0.0);
        Self {
            history: vec![vec![zero; bins]; spectra.segments.len()],
            current: 0,
            buffer: vec![0.0; 2 * bs],
            scratch: spectra.fft.make_scratch(),
            pre_multiplied: vec![zero; bins],
            accumulated: 0,
            begun: false,
            conv: vec![zero; bins],
            overlap: vec![0.0; bs],
            spectra,
        }
    }

    /// The older partitions' products for the next block, partitions
    /// `1..=upto` of them summed into `pre_multiplied`. Everything they read
    /// (the spectra, and the history slots other than `current`, which the
    /// next block overwrites) is fixed between two blocks, and each bin sums
    /// its partitions in ascending order from zero, as one whole-block pass
    /// would, so any split of the partitions into calls gives the same bits.
    fn accumulate_to(&mut self, upto: usize) {
        let spectra = &*self.spectra;
        let count = spectra.segments.len();
        if !self.begun {
            self.pre_multiplied.fill(Complex32::new(0.0, 0.0));
            self.begun = true;
        }
        let upto = upto.min(count.saturating_sub(1));
        for i in self.accumulated + 1..=upto {
            let slot = (self.current + i) % count;
            complex_multiply_accumulate(
                &mut self.pre_multiplied,
                &spectra.segments[i],
                &self.history[slot],
            );
        }
        self.accumulated = self.accumulated.max(upto);
    }

    /// Convolve one block: `input` and `output` are `block_size` long.
    /// Allocation-free. Sums whatever of the older partitions' products
    /// [`accumulate_to`](Self::accumulate_to) has not, so a caller that never
    /// calls it pays the whole sum here.
    fn process(&mut self, input: &[f32], output: &mut [f32]) {
        self.accumulate_to(usize::MAX);
        self.accumulated = 0;
        self.begun = false;
        let spectra = &*self.spectra;
        let (bs, count) = (spectra.block_size, spectra.segments.len());
        if count == 0 {
            output.fill(0.0);
            return;
        }
        copy_and_pad(&mut self.buffer, input);
        spectra.fft.forward(
            &self.buffer,
            &mut self.history[self.current],
            &mut self.scratch,
        );
        self.conv.copy_from_slice(&self.pre_multiplied);
        complex_multiply_accumulate(
            &mut self.conv,
            &self.history[self.current],
            &spectra.segments[0],
        );
        // A non-zero imaginary part at DC or Nyquist, which the spectrum of a
        // real input has only when the input held NaN or ±∞, is ignored by
        // the inverse and the transform completes, so the block is used as
        // computed: a non-finite input propagates as through any linear
        // filter, the same in debug and release, and leaves with the
        // partition that carried it.
        spectra
            .fft
            .inverse(&mut self.conv, &mut self.buffer, &mut self.scratch);
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
        let zero = Complex32::new(0.0, 0.0);
        self.buffer.fill(0.0);
        for h in &mut self.history {
            h.fill(zero);
        }
        self.pre_multiplied.fill(zero);
        self.accumulated = 0;
        self.begun = false;
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
            scratch: self.scratch.clone(),
            pre_multiplied: self.pre_multiplied.clone(),
            accumulated: self.accumulated,
            begun: self.begun,
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
    /// Creates a new convolver from an impulse response.
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

    /// Creates a convolver with the default block size (`512`).
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

    /// Produces one convolved sample. Returns `0.0` for the first
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
        let mut crossed = false;
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
                crossed = true;
            } else if !crossed {
                // Spread the older partitions' sum over the block: by the
                // time `horizon` samples are in, every partition is in. The
                // horizon stops short of the boundary by this call's length
                // (at most half a partition), so the call that crosses the
                // boundary pays for the transforms and the newest partition
                // only. A call that has crossed does no spread work for the
                // next cycle: a later call or the next boundary does it.
                let horizon = (p.block_size - input.len().min(p.block_size / 2)).max(1);
                let older = self.fft.spectra.segments.len().saturating_sub(1);
                self.fft.accumulate_to(older * p.input_fill / horizon);
            }
            done += run;
        }
    }

    /// Zeros the working buffers without discarding the IR. Safe to
    /// call on a stream discontinuity.
    ///
    /// **Both** halves of the state have to be cleared. Clearing only
    /// `partition` — the local scratch — leaves the convolution's own
    /// partition buffers holding the previous signal's tail, which then bleeds
    /// into whatever plays next: with an 8-tap IR the first sample after a
    /// reset comes back at 3.5 instead of silence. `reset` is called precisely
    /// at stream discontinuities (a seek, a region change), so that leak lands
    /// exactly where a ghost of the previous audio is most audible.
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

    /// A NaN input propagates and then leaves: it does not panic (in debug
    /// builds either), it reaches the output as through any linear filter,
    /// and once its partition has left the spectra history and the overlap,
    /// silence in is exactly silence out again.
    ///
    /// The NaN's spectrum is NaN in every bin, including the DC and Nyquist
    /// imaginary parts, which the inverse ignores. With an
    /// 8-tap IR at block 4 (two partitions), a NaN fed at sample 0 is in the
    /// history for two FFT cycles and in the overlap for one more; past the
    /// latency plus three blocks the output is pure convolution of zeros.
    ///
    /// The inverse's handling of those parts is mutation-tested in
    /// `tutti-types`; here the NaN reaches the output with or without them.
    ///
    /// Mutation (run): the overlap keeps a NaN it already holds → the NaN
    /// outlasts its partition → fails.
    /// Mutation (run): `*o = y + v` without the overlap `v` → the NaN is
    /// gone from the third block → fails the early-exit check.
    #[test]
    fn a_nan_input_propagates_and_then_leaves() {
        let ir: Vec<f32> = vec![0.9, 0.8, 0.7, 0.6, 0.5, 0.4, 0.3, 0.2];
        let block = 4;
        let mut c = Convolver::new(&ir, block);
        let out: Vec<f32> = std::iter::once(f32::NAN)
            .chain(std::iter::repeat_n(0.0, 64))
            .map(|x| c.process_sample(x))
            .collect();
        assert!(
            out.iter().any(|y| y.is_nan()),
            "the NaN never reached the output"
        );
        let latency = c.latency();
        // The NaN is still in the output through the third block after the
        // latency and gone from the fourth on.
        assert!(
            out[latency + 2 * block..latency + 3 * block]
                .iter()
                .any(|y| y.is_nan()),
            "the NaN left early: {:?}",
            &out[latency + 2 * block..latency + 3 * block]
        );
        let clean = latency + 3 * block;
        assert!(
            out[clean..].iter().all(|&y| y == 0.0),
            "the NaN stayed past its partition: {:?}",
            &out[clean..]
        );
    }

    /// A deterministic IR of `len` taps with decaying, sign-varying values.
    fn long_ir(len: usize) -> Vec<f32> {
        (0..len)
            .map(|i| ((i as f32) * 0.37).sin() * (-(i as f32) / 900.0).exp())
            .collect()
    }

    /// Feed `x` through `process_block` in `lens`-cycled calls.
    fn run_blocks(c: &mut Convolver, x: &[f32], lens: &[usize]) -> Vec<f32> {
        let mut got = vec![0.0f32; x.len()];
        let (mut at, mut k) = (0, 0);
        while at < x.len() {
            let n = lens[k % lens.len()].min(x.len() - at);
            c.process_block(&x[at..at + n], &mut got[at..at + n]);
            at += n;
            k += 1;
        }
        got
    }

    /// A many-partition IR through the incremental path matches the
    /// definition (a direct convolution computed here), delayed by exactly
    /// `latency()`, for call lengths that divide the partition, ones that do
    /// not, ones that straddle the boundary and ones longer than a partition.
    ///
    /// The tolerance is float rounding of a 2000-tap sum of terms below 1.
    ///
    /// Mutation (run): `accumulate_to` starting its loop at
    /// `self.accumulated + 2` skips a partition per call → fails. Mutation (run):
    /// dropping `self.accumulated = 0` or `self.begun = false` in
    /// `process` leaves the next cycle with a stale or doubled sum → fails.
    #[test]
    fn incremental_path_matches_direct_convolution_at_any_call_length() {
        let ir = long_ir(2000);
        let x: Vec<f32> = (0..3000)
            .map(|i| ((i as f32) * 0.173).sin() * 0.8 + if i % 211 == 0 { 0.5 } else { 0.0 })
            .collect();
        let want = direct_convolve(&x, &ir);
        let patterns: [&[usize]; 6] = [&[1], &[7], &[64], &[33, 5, 64, 9, 200], &[100], &[300, 17]];
        for part in [32usize, 64, 128] {
            for lens in patterns {
                let mut c = Convolver::new(&ir, part);
                let lat = c.latency();
                assert_eq!(lat, part);
                let mut padded = x.clone();
                padded.extend(std::iter::repeat_n(0.0, lat));
                let got = run_blocks(&mut c, &padded, lens);
                for i in 0..x.len() {
                    assert!(
                        (got[i + lat] - want[i]).abs() < 2e-3,
                        "partition {part}, calls {lens:?}: sample {i} is {} want {}",
                        got[i + lat],
                        want[i]
                    );
                }
                assert!(got[..lat].iter().all(|&s| s == 0.0), "fill-up is silent");
            }
        }
    }

    /// Linearity: `conv(a·x + b·y) = a·conv(x) + b·conv(y)` through the
    /// incremental path, within float tolerance.
    ///
    /// Covers linearity only. A dropped partition or a stale sum is still
    /// linear in the input history, so this does not catch them: the
    /// direct-convolution and bit-identity tests do (checked: dropping the
    /// `accumulated`/`begun` reset in `process` passes this test).
    #[test]
    fn incremental_path_is_linear() {
        let ir = long_ir(700);
        let n = 1500;
        let x: Vec<f32> = (0..n).map(|i| ((i as f32) * 0.21).sin()).collect();
        let y: Vec<f32> = (0..n).map(|i| ((i as f32) * 0.05).cos() * 0.5).collect();
        let (a, b) = (0.75f32, -1.25f32);
        let mix: Vec<f32> = x.iter().zip(&y).map(|(&p, &q)| a * p + b * q).collect();
        let lens = [13usize, 64, 5];
        let cx = run_blocks(&mut Convolver::new(&ir, 64), &x, &lens);
        let cy = run_blocks(&mut Convolver::new(&ir, 64), &y, &lens);
        let cm = run_blocks(&mut Convolver::new(&ir, 64), &mix, &lens);
        for i in 0..n {
            assert!((cm[i] - (a * cx[i] + b * cy[i])).abs() < 2e-3, "sample {i}");
        }
    }

    /// However the partitions are split across calls the result is the same bits:
    /// the block path (sum spread over the cycle) and the per-sample path
    /// (the whole sum at the boundary) agree for a many-partition IR. This
    /// pins the split and a loop reversal; an order both paths share is
    /// pinned by `the_spread_sum_is_the_ascending_sum`.
    ///
    /// Mutation (run): `accumulate_to` starting its loop at
    /// `self.accumulated + 2` leaves a partition out on the block path only →
    /// fails.
    #[test]
    fn incremental_path_is_bit_identical_to_the_boundary_sum() {
        let ir = long_ir(1500);
        let x: Vec<f32> = (0..2500)
            .map(|i| ((i as f32) * 0.31).sin() * 0.7 + if i % 53 == 0 { 0.3 } else { 0.0 })
            .collect();
        for part in [16usize, 64, 256] {
            let mut per_sample = Convolver::new(&ir, part);
            let want: Vec<f32> = x.iter().map(|&s| per_sample.process_sample(s)).collect();
            let got = run_blocks(&mut Convolver::new(&ir, part), &x, &[9, 64, 1, 31, 128, 70]);
            assert!(
                got.iter()
                    .zip(&want)
                    .all(|(g, w)| g.to_bits() == w.to_bits()),
                "partition {part}"
            );
        }
    }

    /// The sum is spread, not deferred: with 8-frame calls into a 64-frame
    /// partition no call advances more than its share (plus rounding), and
    /// every partition is in before the call that crosses the boundary.
    ///
    /// Mutation (run): scheduling `accumulate_to(older)` at the first call of
    /// a cycle, or not at all (everything at the boundary), fails the
    /// per-call bound or the done-before-boundary check respectively.
    #[test]
    fn accumulation_is_spread_across_the_calls_of_a_partition() {
        let part = 64;
        let mut c = Convolver::new(&long_ir(1000), part);
        let older = c.fft.spectra.segments.len() - 1;
        let x = vec![0.5f32; 8];
        let mut out = vec![0.0f32; 8];
        let share = older * 8 / (part - 8) + 1;
        for cycle in 0..4 {
            let mut last = 0;
            for call in 0..8 {
                c.process_block(&x, &mut out);
                if call < 7 {
                    let now = c.fft.accumulated;
                    assert!(
                        now - last <= share,
                        "cycle {cycle} call {call}: {last}->{now}"
                    );
                    last = now;
                    if call >= 6 {
                        assert_eq!(now, older, "done before the boundary call");
                    }
                } else {
                    assert_eq!(c.fft.accumulated, 0, "the boundary call drained");
                }
            }
        }
    }

    /// The sum the boundary call receives is the ascending one: partitions
    /// `1..count` added to zero in order, with the code's own `re`/`im`
    /// formulas. With 4-frame calls into a 16-frame partition the sum is
    /// complete after the third call of a cycle; it is recomputed here from
    /// the spectra and the history and compared bit for bit. It is same-run
    /// data, so it does not depend on libm.
    ///
    /// Mutation (run): a descending walk shared by both paths (`let i =
    /// count - k` for `k` in the loop range of `accumulate_to`) fails.
    /// A literal `.rev()` of that loop fails this and
    /// `incremental_path_is_bit_identical_to_the_boundary_sum`.
    #[test]
    fn the_spread_sum_is_the_ascending_sum() {
        let part = 16;
        let mut c = Convolver::new(&long_ir(300), part);
        let count = c.fft.spectra.segments.len();
        assert!(count > 8);
        let x: Vec<f32> = (0..part * 12)
            .map(|i| ((i as f32) * 0.29).sin() + if i % 5 == 0 { 0.4 } else { 0.0 })
            .collect();
        let mut out = [0.0f32; 4];
        let mut checked = 0;
        for (n, call) in x.chunks(4).enumerate() {
            c.process_block(call, &mut out);
            if n % 4 != 2 {
                continue;
            }
            assert_eq!(c.fft.accumulated, count - 1, "sum complete, call {n}");
            let mut want = vec![Complex32::new(0.0f32, 0.0); part + 1];
            for i in 1..count {
                let seg = &c.fft.spectra.segments[i];
                let hist = &c.fft.history[(c.fft.current + i) % count];
                for ((w, a), b) in want.iter_mut().zip(seg).zip(hist) {
                    w.re += a.re * b.re - a.im * b.im;
                    w.im += a.re * b.im + a.im * b.re;
                }
            }
            for (bin, (g, w)) in c.fft.pre_multiplied.iter().zip(&want).enumerate() {
                assert!(
                    g.re.to_bits() == w.re.to_bits() && g.im.to_bits() == w.im.to_bits(),
                    "call {n} bin {bin}: {g:?} want {w:?}"
                );
            }
            checked += 1;
        }
        assert_eq!(checked, 12);
    }

    /// A call that crosses a boundary leaves the next cycle's sum untouched
    /// (`accumulated` 0, not begun), so it never costs more than the
    /// boundary work: the transforms and the newest partition. With calls
    /// 20, 20, 20, 50 into a 64-frame partition the 50 crosses with 46
    /// frames left over.
    ///
    /// Mutation (run): dropping the `crossed` guard in `process_block` leaves
    /// `accumulated` above 0 after the crossing call → fails.
    #[test]
    fn a_call_that_crosses_a_boundary_leaves_the_next_cycle_unstarted() {
        let part = 64;
        let mut c = Convolver::new(&long_ir(1000), part);
        let lens = [20usize, 20, 20, 50];
        let mut out = [0.0f32; 50];
        let (mut fill, mut crossings) = (0usize, 0);
        for k in 0..40 {
            let n = lens[k % 4];
            let x = vec![0.25f32; n];
            c.process_block(&x, &mut out[..n]);
            if fill + n > part {
                crossings += 1;
                assert_eq!(c.fft.accumulated, 0, "call {k} crossed");
                assert!(!c.fft.begun, "call {k} crossed");
            }
            fill = (fill + n) % part;
        }
        assert!(crossings >= 5);
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
