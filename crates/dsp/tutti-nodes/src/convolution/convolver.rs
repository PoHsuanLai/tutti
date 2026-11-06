//! Mono FFT convolver primitive: a partitioned FFT convolution whose
//! transformed impulse response is **stored once**, in an [`IrSpectra`]
//! behind an `Arc`, and shared by every convolver built from it.
//!
//! Each level of the convolution is the uniformly partitioned overlap-add
//! convolution `fft-convolver` 0.3 runs (its `FFTConvolver`), with the same
//! arithmetic in the same order, over [`RealFft`], the engine's
//! CPU-independent real FFT (rustfft's scalar planner, where
//! `fft-convolver`'s `realfft` picks SIMD at run time). An output is
//! therefore bit-identical across CPUs on one OS and libm. It is written here
//! rather than depended on because `fft-convolver` keeps the IR's spectra
//! inside each convolver with no way to share them, so every channel of a
//! shared-IR node and every fork of a node would hold its own copy:
//! megabytes for a long reverb. Here a channel, a clone and a fork hold one
//! `Arc` to the spectra, which nothing writes after they are built; each
//! keeps only its own running state (the input spectra history, the overlap,
//! the FFT scratch).
//!
//! # Non-uniform partitions
//!
//! The latency is the head partition `B` (the `block_size` given to
//! [`Convolver::new`]): the output at frame `n` is the convolution at frame
//! `n - B`. A long IR is cut into **levels**, each a uniform partitioned
//! convolver with its own partition `P` (a power-of-two multiple of `B`) and
//! FFT size `2P`: the head level has `P = B`, and the tail levels have larger
//! ones, up to four times `B` and 4096 frames, so the IR's late part is
//! covered by far fewer, longer partitions. A partition costs one complex
//! multiply-accumulate per bin per `P` frames, so with `L` taps a level costs
//! `L / P` per frame: a tail four times the partition is four times cheaper,
//! and the transforms it adds are a small fixed overhead per level.
//!
//! A level covers the IR taps `start..start + count * P`. It is a uniform
//! convolver of latency `P` followed by a delay of `start + B - P` frames,
//! which is never negative because a tail level starts at `start = P` (one of
//! its own partitions in): the total is `B` on every level, so the node's
//! declared latency is the same as the uniform convolver's. That delay is
//! **slack**: once a block of `P` input frames is complete the level's output
//! is not due for `start + B - P` frames (`B` for a tail level), and the
//! level's two transforms run in **steps** at fixed offsets inside it, at
//! offsets no other level's steps use, so no host block of up to one slot of
//! `B` carries more than one step of a tail level's transforms. A tail
//! transform whose inner FFT has more than [`MAX_SUB_FFT`] points
//! ([`StagedRealFft`]) is cut by decimation in time into sub-FFTs of at most
//! that many points and radix-2 combine passes, one step each, so the biggest
//! unit of work a host block is handed is a 1024-point complex FFT whatever
//! the partition; the cut costs a few percent of the transform's total and
//! changes its rounding but not its result. Every level's output is summed
//! into one ring at the frame it is due, in the order of the steps' frames,
//! which does not depend on how the host splits its calls.
//!
//! A cost model picks the levels (`plan_levels`): the layout with the lowest
//! modelled cost per frame wins, the single uniform level included, and only
//! if it costs at most 0.7 of the uniform one. A short IR (where an extra
//! level's transforms cost about what they save) is the uniform convolver it
//! always was, as is any IR when `B` is 4096 or more.
//!
//! **Each level's partition sum is spread across the blocks of its
//! partition.** Only the newest partition's multiply, the inverse transform
//! and the overlap-add need the input that completes a block; the products of
//! the older partitions with the older history are fixed as soon as the
//! previous block's transform is done. [`Convolver::process_block`] therefore
//! sums them a few partitions per call, in proportion to the frames consumed,
//! finishing a call's length before the next block's transform. Each bin adds
//! its partitions in ascending order from zero, so any split of the work into
//! calls gives the same bits.
//!
//! No allocation happens after construction, and the render is bit-identical
//! for any split of the input into calls, including one frame at a time.

use std::sync::Arc;

use tutti_dsp::{Complex32, StagedRealFft};

/// Default FFT block size when the caller doesn't pick one. A power of two,
/// as every partition size here is.
pub(crate) const DEFAULT_BLOCK_SIZE: usize = 512;

/// A tail level's partition is at most this many times the head's, and at
/// most [`MAX_PARTITION`] frames: the larger it is, the more steps its
/// transforms take, and the fewer slots of the head's slack are left.
const MAX_TAIL_RATIO: usize = 4;
const MAX_PARTITION: usize = 4096;

/// A tail transform's inner complex FFT is cut into sub-FFTs of at most this
/// many points, run on different host blocks ([`StagedRealFft`]): a partition
/// `P` has a `P`-point inner FFT, so a level of partition 2048 runs its
/// transforms as two steps of 1024 points and a combine. A head never splits
/// (its transforms have no slack).
const MAX_SUB_FFT: usize = 1024;

/// A non-uniform layout replaces the uniform one only when its modelled cost
/// is at most this fraction of it: the model leaves out each level's
/// bookkeeping, and an IR short enough for the two to be close measures
/// slightly worse split than whole.
const MIN_GAIN: f64 = 0.7;

/// The cost model's figures, in nanoseconds, measured on a desktop core: one
/// complex multiply-accumulate of a bin (spectra read from memory), and one
/// real transform per `n * log2(n)`. They decide between layouts and nothing
/// else, so only their ratio matters.
const MAC_NS: f64 = 0.7;
const FFT_NS: f64 = 0.29;

/// One level of a layout: `count` partitions of `part` frames covering the
/// IR taps from `start`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Plan {
    part: usize,
    start: usize,
    count: usize,
}

/// The modelled cost per frame of `plan`, in nanoseconds: the multiply-
/// accumulates of every partition and the two transforms of every block.
fn plan_cost(plan: &[Plan]) -> f64 {
    plan.iter()
        .map(|l| {
            let p = l.part as f64;
            let n = 2 * l.part;
            // Every partition is a power of two, so the integer log is exact.
            let n_log_n = n as f64 * f64::from(n.ilog2());
            l.count as f64 * (p + 1.0) * MAC_NS / p + 2.0 * FFT_NS * n_log_n / p
        })
        .sum()
}

/// The levels for partitions `parts` (ascending, `parts[0]` the head) over an
/// IR of `ir_len` taps. A level starts at its own partition size, which is
/// what leaves a tail level `B` frames of slack: the head takes `parts[1] / B`
/// partitions, and each later level the number that reaches the next one.
fn build_plan(ir_len: usize, parts: &[usize]) -> Vec<Plan> {
    let mut plan = Vec::new();
    let mut start = 0;
    for (j, &part) in parts.iter().enumerate() {
        if start >= ir_len {
            break;
        }
        let want = match parts.get(j + 1) {
            Some(&next) if j == 0 => next / part,
            Some(&next) => next / part - 1,
            None => usize::MAX,
        };
        let count = want.min((ir_len - start).div_ceil(part));
        plan.push(Plan { part, start, count });
        start += count * part;
    }
    plan
}

/// The layout with the lowest modelled cost: the uniform one (a single
/// head level), or the head and tail levels whose partitions are powers of
/// two up to [`MAX_TAIL_RATIO`] times the head's, if that costs at most
/// [`MIN_GAIN`] of the uniform. Empty for an empty IR.
fn plan_levels(ir_len: usize, head: usize) -> Vec<Plan> {
    if ir_len == 0 {
        return Vec::new();
    }
    let uniform = vec![Plan {
        part: head,
        start: 0,
        count: ir_len.div_ceil(head),
    }];
    let uniform_cost = plan_cost(&uniform);
    let mut best = (uniform_cost * MIN_GAIN, None);
    let reach = MAX_PARTITION.min(head.saturating_mul(MAX_TAIL_RATIO));
    let sizes: Vec<usize> = std::iter::successors(head.checked_mul(2), |p| p.checked_mul(2))
        .take_while(|&p| p <= reach)
        .collect();
    for mask in 1u32..(1 << sizes.len()) {
        let parts: Vec<usize> = std::iter::once(head)
            .chain(
                sizes
                    .iter()
                    .enumerate()
                    .filter(|(k, _)| mask >> k & 1 == 1)
                    .map(|(_, &p)| p),
            )
            .collect();
        let cand = build_plan(ir_len, &parts);
        let cost = plan_cost(&cand);
        if cost < best.0 {
            best = (cost, Some(cand));
        }
    }
    best.1.unwrap_or(uniform)
}

/// Zeroed complex spectrum of `bins` bins.
fn zeros(bins: usize) -> Vec<Complex32> {
    vec![Complex32::new(0.0, 0.0); bins]
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

/// One level of an impulse response, transformed: `segments.len()` spectra of
/// `part` frames each, and the two transforms of `2 × part` points that
/// produced them and that every convolver over it runs.
struct Level {
    part: usize,
    /// Frames from the level's uniform output (latency `part`) to its due
    /// time (latency `B`): `start + B - part`. Zero for the head.
    delay: usize,
    /// Frames after a block of `part` inputs is complete at which each step
    /// of its transforms runs: the forward transform's steps, then the
    /// multiply and the inverse transform's. Non-decreasing, ending at most
    /// `delay` and below `part`. `forward_steps` of them are the forward's.
    steps: Vec<usize>,
    forward_steps: usize,
    /// `part + 1` bins each.
    segments: Vec<Vec<Complex32>>,
    /// Both directions, `2 × part` points, in steps.
    fft: StagedRealFft,
}

/// An impulse response, transformed: its levels, built once and
/// read-only after that. Nothing on the audio thread (or anywhere) writes it,
/// so any number of convolvers (a node's channels, a clone, a fork) share one
/// behind an `Arc`.
pub struct IrSpectra {
    block_size: usize,
    ir_len: usize,
    levels: Vec<Level>,
    /// Power-of-two lengths of the input ring (the newest `part` frames plus
    /// the first step's offset, of every level) and the output ring (the
    /// furthest frame a level writes ahead of the read cursor).
    input_ring: usize,
    output_ring: usize,
}

impl std::fmt::Debug for IrSpectra {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IrSpectra")
            .field("block_size", &self.block_size)
            .field("ir_len", &self.ir_len)
            .field("input_ring", &self.input_ring)
            .field("output_ring", &self.output_ring)
            .finish_non_exhaustive()
    }
}

impl IrSpectra {
    /// Partition and transform `ir` at `block_size`, which must be a power
    /// of two. Allocates; control thread.
    fn new(ir: &[f32], block_size: usize) -> Self {
        debug_assert!(block_size.is_power_of_two());
        Self::from_plan(
            ir,
            block_size,
            &plan_levels(ir.len(), block_size),
            MAX_SUB_FFT,
        )
    }

    /// `ir` cut into the levels of `plan`, whose tail transforms are cut into
    /// sub-FFTs of at most `max_sub` points.
    fn from_plan(ir: &[f32], block_size: usize, plan: &[Plan], max_sub: usize) -> Self {
        // A tail level's transforms (`2 * part` real points, `part` complex)
        // run in `part / max_sub` sub-FFTs and the passes around them; a
        // head's run whole, at its start.
        let ffts: Vec<StagedRealFft> = plan
            .iter()
            .enumerate()
            .map(|(j, l)| {
                StagedRealFft::new(
                    2 * l.part,
                    if j == 0 { 1 } else { l.part.div_ceil(max_sub) },
                )
            })
            .collect();
        // The tail levels' steps in the head partition's cycle: the head's
        // transforms are at its start, and the steps of each tail level (its
        // forward steps, then its inverse's) at slots that no other step
        // uses, spaced evenly through the `block_size` frames of slack, so
        // that a host block no longer than one slot holds at most one of
        // them.
        let tail_steps: usize = ffts[1.min(ffts.len())..]
            .iter()
            .map(|f| f.forward_steps() + f.inverse_steps())
            .sum();
        let slot = (block_size / (tail_steps + 1)).max(1);
        let mut taken = 0;
        let levels: Vec<Level> = plan
            .iter()
            .zip(ffts)
            .enumerate()
            .map(|(j, (l, fft))| {
                let mut scratch = fft.new_scratch();
                let mut buffer = vec![0.0f32; 2 * l.part];
                let segments = (0..l.count)
                    .map(|i| {
                        let lo = l.start + i * l.part;
                        let hi = ir.len().min(lo + l.part);
                        copy_and_pad(&mut buffer, &ir[lo..hi]);
                        let mut spectrum = zeros(l.part + 1);
                        for step in 0..fft.forward_steps() {
                            fft.forward_step(step, &buffer, &mut spectrum, &mut scratch);
                        }
                        spectrum
                    })
                    .collect();
                let delay = l.start + block_size - l.part;
                let (forward_steps, steps) = (
                    fft.forward_steps(),
                    fft.forward_steps() + fft.inverse_steps(),
                );
                let steps = (0..steps)
                    .map(|_| {
                        if j == 0 {
                            return 0;
                        }
                        taken += 1;
                        (taken * slot).min(delay)
                    })
                    .collect();
                Level {
                    part: l.part,
                    delay,
                    steps,
                    forward_steps,
                    segments,
                    fft,
                }
            })
            .collect();
        let input_ring = levels
            .iter()
            .map(|l| l.part + l.steps[0])
            .max()
            .unwrap_or(1)
            .next_power_of_two();
        let output_ring =
            (levels.iter().map(|l| l.delay + l.part).max().unwrap_or(0) + 1).next_power_of_two();
        Self {
            block_size,
            ir_len: ir.len(),
            levels,
            input_ring,
            output_ring,
        }
    }

    /// The partition size and partition count of each level, head first.
    #[cfg(test)]
    pub(super) fn shape(&self) -> Vec<(usize, usize)> {
        self.levels
            .iter()
            .map(|l| (l.part, l.segments.len()))
            .collect()
    }
}

/// One convolver's running state for one [`Level`]: everything here is this
/// convolver's own; the spectra are only read.
#[derive(Clone)]
struct LevelState {
    /// The input's spectra, one per partition: a ring whose write slot is
    /// `current`; the block before is at `current + 1`.
    history: Vec<Vec<Complex32>>,
    current: usize,
    /// `2 × part`: the padded input going in, the product coming out.
    buffer: Vec<f32>,
    /// Both transforms' scratch, which holds a transform between its steps.
    scratch: Vec<Complex32>,
    /// The older partitions' products for the *next* block, summed a few
    /// partitions at a time ([`accumulate_to`](Self::accumulate_to)):
    /// partitions `1..=accumulated` are in, and `begun` says the sum was
    /// zeroed for this block.
    building: Vec<Complex32>,
    accumulated: usize,
    begun: bool,
    /// The finished sum for the block whose multiply is pending.
    ready: Vec<Complex32>,
    conv: Vec<Complex32>,
    /// The second half of the last block's product, added to the next.
    overlap: Vec<f32>,
    /// The frame (frames pushed so far) at which the block being transformed
    /// was complete, and the next of the level's steps to run for it
    /// (`Level::steps`, which are offsets from `base`).
    base: u64,
    step: usize,
    /// The frame of the last forward step, once the block's forward
    /// transform is done and until the next one starts: the spread of the sum
    /// is measured from it.
    spread_from: Option<u64>,
    /// A forward transform has finished, so a sum for the next block exists.
    seeded: bool,
    /// A step of a forward transform ran in the current call.
    crossed: bool,
}

impl std::fmt::Debug for LevelState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LevelState")
            .field("current", &self.current)
            .field("accumulated", &self.accumulated)
            .field("begun", &self.begun)
            .field("base", &self.base)
            .field("step", &self.step)
            .field("spread_from", &self.spread_from)
            .field("seeded", &self.seeded)
            .field("crossed", &self.crossed)
            .finish_non_exhaustive()
    }
}

impl LevelState {
    /// A level's state in its freshly built state. Allocates.
    fn new(lv: &Level) -> Self {
        let zero = Complex32::new(0.0, 0.0);
        Self {
            history: vec![vec![zero; lv.part + 1]; lv.segments.len()],
            current: 0,
            buffer: vec![0.0; 2 * lv.part],
            scratch: lv.fft.new_scratch(),
            building: zeros(lv.part + 1),
            accumulated: 0,
            begun: false,
            ready: zeros(lv.part + 1),
            conv: zeros(lv.part + 1),
            overlap: vec![0.0; lv.part],
            base: lv.part as u64,
            step: 0,
            spread_from: None,
            seeded: false,
            crossed: false,
        }
    }

    /// The frame at which the next step runs.
    fn next_frame(&self, lv: &Level) -> u64 {
        self.base + lv.steps[self.step] as u64
    }

    /// The step due: a step of the block's forward transform, then of its
    /// multiply and inverse transform, then the next block's.
    fn run_step(&mut self, lv: &Level, input: &[f32], output: &mut [f32]) {
        let at = self.next_frame(lv);
        if self.step < lv.forward_steps {
            self.forward(lv, input, at);
        } else {
            self.inverse(lv, self.step - lv.forward_steps, output);
        }
        self.step += 1;
        if self.step == lv.steps.len() {
            self.step = 0;
            self.base += lv.part as u64;
        }
    }

    /// Partitions `1..=upto` of the older products summed into `building`.
    /// Everything they read (the spectra, and the history slots other than
    /// `current`, which the next forward transform overwrites) is fixed
    /// between two blocks, and each bin sums its partitions in ascending
    /// order from zero, as one whole-block pass would, so any split of the
    /// partitions into calls gives the same bits.
    fn accumulate_to(&mut self, lv: &Level, upto: usize) {
        let count = lv.segments.len();
        if !self.begun {
            self.building.fill(Complex32::new(0.0, 0.0));
            self.begun = true;
        }
        let upto = upto.min(count - 1);
        for i in self.accumulated + 1..=upto {
            let slot = (self.current + i) % count;
            complex_multiply_accumulate(&mut self.building, &lv.segments[i], &self.history[slot]);
        }
        self.accumulated = self.accumulated.max(upto);
    }

    /// Forward step, at frame `at`. The first finishes the sum for the block,
    /// takes the block of `part` frames that ended `steps[0]` frames ago from
    /// the input ring, and starts its transform; the last writes the spectrum
    /// into the write slot.
    fn forward(&mut self, lv: &Level, input: &[f32], at: u64) {
        if self.step == 0 {
            if self.seeded {
                self.accumulate_to(lv, usize::MAX);
                std::mem::swap(&mut self.building, &mut self.ready);
            }
            self.accumulated = 0;
            self.begun = false;
            self.spread_from = None;
            let mask = input.len() - 1;
            let first = (self.base as usize).wrapping_sub(lv.part);
            for (r, b) in self.buffer[..lv.part].iter_mut().enumerate() {
                *b = input[first.wrapping_add(r) & mask];
            }
            self.buffer[lv.part..].fill(0.0);
        }
        lv.fft.forward_step(
            self.step,
            &self.buffer,
            &mut self.history[self.current],
            &mut self.scratch,
        );
        if self.step + 1 == lv.forward_steps {
            self.current = if self.current > 0 {
                self.current - 1
            } else {
                lv.segments.len() - 1
            };
            self.spread_from = Some(at);
            self.seeded = true;
        }
    }

    /// Inverse step `k`. The first is the block's product: the older
    /// partitions' sum plus the newest partition. The last
    /// normalises the result, adds it to the overlap, and sums it into the
    /// output ring where the level's output is due.
    fn inverse(&mut self, lv: &Level, k: usize, output: &mut [f32]) {
        if k == 0 {
            let count = lv.segments.len();
            let newest = (self.current + 1) % count;
            self.conv.copy_from_slice(&self.ready);
            complex_multiply_accumulate(&mut self.conv, &self.history[newest], &lv.segments[0]);
        }
        // A non-zero imaginary part at DC or Nyquist, which the spectrum of a
        // real input has only when the input held NaN or ±∞, is ignored by
        // the inverse and the transform completes, so the block is used as
        // computed: a non-finite input propagates as through any linear
        // filter, the same in debug and release, and leaves with the
        // partition that carried it.
        lv.fft
            .inverse_step(k, &mut self.conv, &mut self.buffer, &mut self.scratch);
        if k + 1 < lv.steps.len() - lv.forward_steps {
            return;
        }
        // The inverse is unnormalised.
        let len = self.buffer.len() as f32;
        for x in &mut self.buffer {
            *x /= len;
        }
        let mask = output.len() - 1;
        let due = (self.base as usize).wrapping_add(lv.delay);
        for (r, (&y, &v)) in self.buffer[..lv.part].iter().zip(&self.overlap).enumerate() {
            output[due.wrapping_add(r) & mask] += y + v;
        }
        self.overlap.copy_from_slice(&self.buffer[lv.part..]);
    }

    /// Clear the running state; the spectra stay.
    fn reset(&mut self, lv: &Level) {
        let zero = Complex32::new(0.0, 0.0);
        self.buffer.fill(0.0);
        for h in &mut self.history {
            h.fill(zero);
        }
        self.building.fill(zero);
        self.ready.fill(zero);
        self.conv.fill(zero);
        self.accumulated = 0;
        self.begun = false;
        self.overlap.fill(0.0);
        self.current = 0;
        self.base = lv.part as u64;
        self.step = 0;
        self.spread_from = None;
        self.seeded = false;
        self.crossed = false;
    }
}

/// Real-time-safe mono convolver.
///
/// Composes the levels of the IR's [`IrSpectra`], which it shares (a `Clone`
/// shares them, and so does [`fresh`](Self::fresh)), with its own running
/// state, all sized at construction. The hot path (`process_sample`,
/// `process_block`) never allocates.
#[derive(Clone)]
pub struct Convolver {
    spectra: Arc<IrSpectra>,
    levels: Vec<LevelState>,
    /// The newest input frames, a ring of `spectra.input_ring`.
    input: Vec<f32>,
    /// The output not yet read: every level's blocks summed at the frames they
    /// are due, a ring of `spectra.output_ring`, zeroed as it is read.
    output: Vec<f32>,
    /// Frames consumed since construction or the last reset.
    pushed: u64,
}

impl std::fmt::Debug for Convolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Convolver")
            .field("spectra", &self.spectra)
            .field("levels", &self.levels)
            .field("pushed", &self.pushed)
            .finish_non_exhaustive()
    }
}

impl Convolver {
    /// Creates a new convolver from an impulse response.
    ///
    /// `block_size` is rounded up to the next power of two; it is the head
    /// partition and the latency.
    #[must_use]
    pub fn new(ir: &[f32], block_size: usize) -> Self {
        let block_size = block_size.next_power_of_two().max(2);
        Self::over(Arc::new(IrSpectra::new(ir, block_size)))
    }

    /// A convolver over `ir` at `block_size` (a power of two) cut by `parts`
    /// (the partition of each level, ascending, the head first) rather than
    /// by the cost model.
    #[cfg(test)]
    fn with_parts(ir: &[f32], block_size: usize, parts: &[usize]) -> Self {
        Self::with_parts_split(ir, block_size, parts, MAX_SUB_FFT)
    }

    /// [`with_parts`](Self::with_parts), with the tail transforms cut into
    /// sub-FFTs of at most `max_sub` points.
    #[cfg(test)]
    fn with_parts_split(ir: &[f32], block_size: usize, parts: &[usize], max_sub: usize) -> Self {
        assert_eq!(parts[0], block_size);
        let plan = build_plan(ir.len(), parts);
        Self::over(Arc::new(IrSpectra::from_plan(
            ir, block_size, &plan, max_sub,
        )))
    }

    /// A convolver over `spectra`, in its freshly built state.
    fn over(spectra: Arc<IrSpectra>) -> Self {
        Self {
            levels: spectra.levels.iter().map(LevelState::new).collect(),
            input: vec![0.0; spectra.input_ring],
            output: vec![0.0; spectra.output_ring],
            pushed: 0,
            spectra,
        }
    }

    /// A convolver over this one's IR, sharing its spectra, in the state of
    /// a freshly built one: nothing of this one's running state is copied.
    /// Allocates the new state (the spectra are not copied).
    #[must_use]
    pub fn fresh(&self) -> Self {
        Self::over(Arc::clone(&self.spectra))
    }

    /// The IR's spectra this convolver reads, shared.
    pub fn spectra(&self) -> &Arc<IrSpectra> {
        &self.spectra
    }

    /// Creates a convolver with the default block size (`512`).
    #[must_use]
    pub fn with_ir(ir: &[f32]) -> Self {
        Self::new(ir, DEFAULT_BLOCK_SIZE)
    }

    /// IR length in samples.
    pub fn ir_length(&self) -> usize {
        self.spectra.ir_len
    }

    /// Latency introduced by the partitioned FFT (== block size).
    pub fn latency(&self) -> usize {
        self.spectra.block_size
    }

    /// Produces one convolved sample. Returns `0.0` for the first
    /// `block_size` samples (fill-up latency).
    #[inline]
    pub fn process_sample(&mut self, input: f32) -> f32 {
        let mut out = [0.0f32];
        self.process_block(&[input], &mut out);
        out[0]
    }

    /// Convolve a block: `output[i]` is what `process_sample(input[i])` would
    /// have returned, for every `i`, bit for bit, and the same for any split
    /// of the input into calls. `input` and `output` must be the same length,
    /// which may be anything.
    ///
    /// The call is cut into runs at the frames where a level has a transform
    /// to run, which it does as soon as its frame is reached, so each runs
    /// before the output frame that needs it. A call costs the transforms
    /// whose frames it contains, the share of every level's older partitions
    /// its length is due, and the copy.
    pub fn process_block(&mut self, input: &[f32], output: &mut [f32]) {
        debug_assert_eq!(input.len(), output.len());
        let Self {
            spectra,
            levels,
            input: ring,
            output: out_ring,
            pushed,
        } = self;
        let (in_mask, out_mask) = (ring.len() - 1, out_ring.len() - 1);
        let mut done = 0;
        while done < input.len() {
            // The frames to the next transform, or the end of the call.
            let next = levels
                .iter()
                .zip(&spectra.levels)
                .map(|(st, lv)| st.next_frame(lv))
                .min()
                .unwrap_or(u64::MAX);
            let run = (next - *pushed).min((input.len() - done) as u64) as usize;
            let at = *pushed as usize;
            for (r, o) in output[done..done + run].iter_mut().enumerate() {
                let slot = &mut out_ring[at.wrapping_add(r) & out_mask];
                *o = *slot;
                *slot = 0.0;
            }
            for (r, &x) in input[done..done + run].iter().enumerate() {
                ring[at.wrapping_add(r) & in_mask] = x;
            }
            *pushed += run as u64;
            done += run;
            for (lv, st) in spectra.levels.iter().zip(levels.iter_mut()) {
                // What is due at this frame, in the order of the frames and,
                // at one frame, a level's forward transform before its
                // inverse and the levels in order: the sums into the output
                // ring add in an order that does not depend on the calls.
                while st.next_frame(lv) <= *pushed {
                    st.crossed |= st.step < lv.forward_steps;
                    st.run_step(lv, ring, out_ring);
                }
                // Spread the older partitions' sum over the block: by the
                // time `horizon` frames have passed since the level's forward
                // transform finished every partition is in. The horizon stops
                // short of the next transform's first step by this call's
                // length (at most half the span), so the call that holds it
                // pays for the transforms and the newest partition only. A
                // call that has run a step does no spread work for the next
                // block: a later call or the next transform does it.
                let older = lv.segments.len() - 1;
                if let (Some(last), true) = (st.spread_from, older > 0 && !st.crossed) {
                    let span = lv.part - (lv.steps[lv.forward_steps - 1] - lv.steps[0]);
                    let horizon = (span - input.len().min(span / 2)).max(1) as u64;
                    let target = older as u64 * (*pushed - last) / horizon;
                    st.accumulate_to(lv, target.min(older as u64) as usize);
                }
            }
        }
        for st in levels.iter_mut() {
            st.crossed = false;
        }
    }

    /// Zeros the working buffers without discarding the IR. Safe to
    /// call on a stream discontinuity.
    ///
    /// **Everything** that holds signal has to be cleared: the input ring,
    /// the output ring, and every level's history, sums and overlap. Clearing
    /// only the input leaves the convolution's own partition buffers holding
    /// the previous signal's tail, which then bleeds into whatever plays
    /// next: with an 8-tap IR the first sample after a reset comes back at
    /// 3.5 instead of silence. `reset` is called precisely at stream
    /// discontinuities (a seek, a region change), so that leak lands exactly
    /// where a ghost of the previous audio is most audible.
    pub fn reset(&mut self) {
        self.input.fill(0.0);
        self.output.fill(0.0);
        self.pushed = 0;
        for (lv, st) in self.spectra.levels.iter().zip(&mut self.levels) {
            st.reset(lv);
        }
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
                let mut c = Convolver::with_parts(&ir, part, &[part]);
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
            let mut per_sample = Convolver::with_parts(&ir, part, &[part]);
            let want: Vec<f32> = x.iter().map(|&s| per_sample.process_sample(s)).collect();
            let got = run_blocks(
                &mut Convolver::with_parts(&ir, part, &[part]),
                &x,
                &[9, 64, 1, 31, 128, 70],
            );
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
        let mut c = Convolver::with_parts(&long_ir(1000), part, &[part]);
        let older = c.spectra.levels[0].segments.len() - 1;
        let x = vec![0.5f32; 8];
        let mut out = vec![0.0f32; 8];
        let share = older * 8 / (part - 8) + 1;
        // Nothing is summed before the first block's transform: the history
        // is zero, so the first cycle is not measured.
        for _ in 0..8 {
            c.process_block(&x, &mut out);
        }
        for cycle in 0..4 {
            let mut last = 0;
            for call in 0..8 {
                c.process_block(&x, &mut out);
                if call < 7 {
                    let now = c.levels[0].accumulated;
                    assert!(
                        now - last <= share,
                        "cycle {cycle} call {call}: {last}->{now}"
                    );
                    last = now;
                    if call >= 6 {
                        assert_eq!(now, older, "done before the boundary call");
                    }
                } else {
                    assert_eq!(c.levels[0].accumulated, 0, "the boundary call drained");
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
        let mut c = Convolver::with_parts(&long_ir(300), part, &[part]);
        let count = c.spectra.levels[0].segments.len();
        assert!(count > 8);
        let x: Vec<f32> = (0..part * 12)
            .map(|i| ((i as f32) * 0.29).sin() + if i % 5 == 0 { 0.4 } else { 0.0 })
            .collect();
        let mut out = [0.0f32; 4];
        let mut checked = 0;
        for (n, call) in x.chunks(4).enumerate() {
            c.process_block(call, &mut out);
            // Nothing is summed before the first block's transform.
            if n % 4 != 2 || n < 4 {
                continue;
            }
            assert_eq!(c.levels[0].accumulated, count - 1, "sum complete, call {n}");
            let mut want = vec![Complex32::new(0.0f32, 0.0); part + 1];
            for i in 1..count {
                let seg = &c.spectra.levels[0].segments[i];
                let hist = &c.levels[0].history[(c.levels[0].current + i) % count];
                for ((w, a), b) in want.iter_mut().zip(seg).zip(hist) {
                    w.re += a.re * b.re - a.im * b.im;
                    w.im += a.re * b.im + a.im * b.re;
                }
            }
            for (bin, (g, w)) in c.levels[0].building.iter().zip(&want).enumerate() {
                assert!(
                    g.re.to_bits() == w.re.to_bits() && g.im.to_bits() == w.im.to_bits(),
                    "call {n} bin {bin}: {g:?} want {w:?}"
                );
            }
            checked += 1;
        }
        assert_eq!(checked, 11);
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
        let mut c = Convolver::with_parts(&long_ir(1000), part, &[part]);
        let lens = [20usize, 20, 20, 50];
        let mut out = [0.0f32; 50];
        let (mut fill, mut crossings) = (0usize, 0);
        for k in 0..40 {
            let n = lens[k % 4];
            let x = vec![0.25f32; n];
            c.process_block(&x, &mut out[..n]);
            if fill + n > part {
                crossings += 1;
                assert_eq!(c.levels[0].accumulated, 0, "call {k} crossed");
                assert!(!c.levels[0].begun, "call {k} crossed");
            }
            fill = (fill + n) % part;
        }
        assert!(crossings >= 5);
    }

    /// Only a forward step suppresses the call's spread work: a call that
    /// runs a tail level's inverse step still advances its sum of older
    /// partitions. This shapes load only (any split of the work into calls
    /// renders the same bits), so the observable is `accumulated`. Calls of
    /// one frame into a cut 256-frame level: some call that ran an inverse
    /// step and no forward one leaves `accumulated` higher than before.
    ///
    /// Mutation (run): `crossed |= true` in `process_block` (any step) leaves
    /// no such call → fails.
    #[test]
    fn an_inverse_step_does_not_suppress_the_spread() {
        let mut c = Convolver::with_parts_split(&long_ir(8000), 64, &[64, 256], 16);
        let tail = 1;
        let (forward, all) = (
            c.spectra.levels[tail].forward_steps,
            c.spectra.levels[tail].steps.len(),
        );
        assert!(forward > 1 && all > forward);
        let mut out = [0.0f32; 1];
        let mut advanced = 0;
        for _ in 0..40 * 256 {
            let (step, acc) = (c.levels[tail].step, c.levels[tail].accumulated);
            c.process_block(&[0.25], &mut out);
            let st = &c.levels[tail];
            let inverse_only = step >= forward && st.step > step;
            if inverse_only && st.accumulated > acc {
                advanced += 1;
            }
        }
        assert!(advanced > 0);
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

    // ---- Non-uniform layouts ------------------------------------------------

    /// A noise-like signal, not periodic in any block length used here.
    fn noise(len: usize) -> Vec<f32> {
        let mut state = 0x2545_f491u32;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                (state >> 8) as f32 / (1 << 23) as f32 - 1.0
            })
            .collect()
    }

    /// An IR of `len` taps that does not decay, so a late partition carries
    /// as much as an early one and dropping it cannot hide in the tolerance.
    fn flat_ir(len: usize) -> Vec<f32> {
        noise(len + 5)[5..].iter().map(|v| v * 0.5).collect()
    }

    /// The round-off tolerance of an FFT convolution of `x` with `ir` through
    /// transforms of at most `n` points.
    ///
    /// One output is a sum of products, computed as the inverse of a product
    /// of transforms. A transform's rounding error is `~ eps * log2(n)` of its
    /// input's norm, so an output differs from the exact sum by a small
    /// multiple of `eps * log2(n) * |h|_2 * |x|_2` (the norms of the whole IR
    /// and the whole input, which bound every partition's and every block's;
    /// the partitions' roundings are independent and add in quadrature, which
    /// the bound ignores). The constant 2 is about nine times the worst error
    /// measured over the layouts below, and `the_tolerance_is_tight_enough`
    /// pins that ratio so the bound cannot drift loose: a wrong or missing
    /// partition is an error of order `|h_k| |x|`, thousands of times more.
    fn tolerance(ir: &[f32], x: &[f32], n: usize) -> f32 {
        let norm = |v: &[f32]| v.iter().map(|a| a * a).sum::<f32>().sqrt();
        2.0 * f32::EPSILON * (n as f32).log2() * norm(ir) * norm(x)
    }

    /// The layouts the tests force: head partition, then the partitions of the
    /// tail levels.
    const LAYOUTS: [(usize, &[usize]); 5] = [
        (16, &[16, 64]),
        (16, &[16, 32, 128]),
        (8, &[8, 16, 32, 64]),
        (32, &[32, 128]),
        (4, &[4, 16]),
    ];

    /// The IR lengths that sit on and around every level's first and last tap.
    fn boundary_lengths(head: usize, parts: &[usize]) -> Vec<usize> {
        let plan = build_plan(1 << 20, parts);
        let mut lens = vec![1, head - 1, head, head + 1];
        for l in &plan[1..] {
            lens.extend([l.start - 1, l.start, l.start + 1]);
            lens.extend([l.start + l.part - 1, l.start + l.part, l.start + l.part + 1]);
        }
        let last = plan.last().unwrap();
        lens.push(last.start + 2 * last.part + 7);
        lens.retain(|&l| l >= 1);
        lens.sort_unstable();
        lens.dedup();
        lens
    }

    /// The convolver's output for `x` then enough silence for the whole tail,
    /// fed in `lens`-cycled calls, with the latency dropped.
    fn render(c: &mut Convolver, x: &[f32], lens: &[usize]) -> Vec<f32> {
        let lat = c.latency();
        let mut padded = x.to_vec();
        padded.extend(std::iter::repeat_n(0.0, lat + c.ir_length()));
        let mut got = run_blocks(c, &padded, lens);
        assert!(
            got[..lat].iter().all(|&s| s == 0.0),
            "the fill-up is exact silence"
        );
        got.drain(..lat);
        got
    }

    /// Every layout, at IR lengths that end exactly on, one short of and one
    /// past each level's first and last tap, equals the direct convolution
    /// (the definition) within round-off, through the whole tail, for call
    /// lengths that divide a partition, ones that do not, ones that straddle
    /// the transforms' frames and ones longer than the longest partition.
    ///
    /// The tolerance is [`tolerance`]'s. Mutations (run): `inverse` summing
    /// into the output ring with `delay` left out, or with `delay + 1`, puts
    /// a tail level's block at the wrong frame → fails at every length that
    /// reaches a tail level; `build_plan` giving the last level one partition
    /// fewer fails the lengths that end in it.
    #[test]
    fn non_uniform_output_equals_direct_convolution() {
        let x = noise(900);
        let patterns: [&[usize]; 5] = [&[1], &[7, 13], &[61], &[64, 3, 127], &[5000]];
        for (head, parts) in LAYOUTS {
            for len in boundary_lengths(head, parts) {
                let ir = flat_ir(len);
                let want = direct_convolve(&x, &ir);
                let tol = tolerance(&ir, &x, 2 * parts[parts.len() - 1]);
                for lens in patterns {
                    let mut c = Convolver::with_parts(&ir, head, parts);
                    let got = render(&mut c, &x, lens);
                    for (i, w) in want.iter().enumerate() {
                        assert!(
                            (got[i] - w).abs() <= tol,
                            "parts {parts:?}, {len} taps, calls {lens:?}: frame {i} is {} want {w} (tol {tol})",
                            got[i]
                        );
                    }
                    let rest = got[want.len()..]
                        .iter()
                        .fold(0.0f32, |a, &s| a.max(s.abs()));
                    assert!(
                        rest <= tol,
                        "parts {parts:?}, {len} taps: ringing after the tail {rest}"
                    );
                }
            }
        }
    }

    /// The tolerance is within a small factor of the error the layouts
    /// actually make, so it cannot be so loose that a wrong partition passes.
    /// Checked at IR lengths of 700, 3000 and 9000 taps (the longer ones
    /// exercise transforms of up to 128 points with many partitions per
    /// level), over every layout.
    ///
    /// Mutation (run): `tolerance`'s constant 2 raised to 4096 fails.
    #[test]
    fn the_tolerance_is_tight_enough() {
        let x = noise(900);
        let mut worst_ratio = 0.0f32;
        for len in [700, 3000, 9000] {
            for (head, parts) in LAYOUTS {
                let ir = flat_ir(len);
                let want = direct_convolve(&x, &ir);
                let mut c = Convolver::with_parts(&ir, head, parts);
                let got = render(&mut c, &x, &[61]);
                let err = want
                    .iter()
                    .zip(&got)
                    .fold(0.0f32, |a, (w, g)| a.max((w - g).abs()));
                worst_ratio = worst_ratio.max(err / tolerance(&ir, &x, 2 * parts[parts.len() - 1]));
            }
        }
        assert!(
            (0.02..0.5).contains(&worst_ratio),
            "the measured error is {worst_ratio} of the tolerance"
        );
    }

    /// An impulse in gives the IR out, at exactly the declared latency: exact
    /// silence for the `latency` frames, then the IR's taps in order from the
    /// impulse's frame.
    ///
    /// Mutation (run): `delay` computed as `start + block_size - part + 1`
    /// shifts every tail tap by one frame → fails at the first tap of the
    /// first tail level.
    #[test]
    fn an_impulse_gives_the_ir_at_the_declared_latency() {
        for (head, parts) in LAYOUTS {
            let last = parts[parts.len() - 1];
            let ir = flat_ir(boundary_lengths(head, parts).into_iter().max().unwrap());
            let mut x = vec![0.0f32; 11];
            x[3] = 1.0;
            let mut c = Convolver::with_parts(&ir, head, parts);
            assert_eq!(c.latency(), head);
            let lat = c.latency();
            let mut padded = x.clone();
            padded.extend(std::iter::repeat_n(0.0, lat + ir.len()));
            let got = run_blocks(&mut c, &padded, &[9, 1, 40]);
            assert!(
                got[..lat].iter().all(|&s| s == 0.0),
                "parts {parts:?}: the fill-up is exact silence"
            );
            let tol = tolerance(&ir, &x, 2 * last);
            // Before the impulse (inside its block, where the transforms'
            // rounding is) the output is within round-off of silence.
            assert!(
                got[lat..lat + 3].iter().all(|&s| s.abs() <= tol),
                "parts {parts:?}: {:?}",
                &got[lat..lat + 3]
            );
            for (k, &h) in ir.iter().enumerate() {
                assert!(
                    (got[lat + 3 + k] - h).abs() <= tol,
                    "parts {parts:?}: tap {k} is {} want {h}",
                    got[lat + 3 + k]
                );
            }
        }
    }

    /// The IR's last tap reaches the output, in the last level, whether the
    /// tap is the level's first or last or the IR ends part-way through a
    /// partition: a unit impulse through an IR of zeros but for its last tap
    /// comes out as a single unit at `latency + len - 1`.
    ///
    /// Mutations (run): `build_plan` leaving the last level's last partition
    /// out, or `inverse` summing its block at the wrong frame (no `delay`),
    /// fails: the unit never comes out.
    #[test]
    fn the_tail_reaches_the_output() {
        for (head, parts) in LAYOUTS {
            let plan = build_plan(1 << 20, parts);
            let last = plan[plan.len() - 1];
            assert!(parts.len() > 1);
            for len in [
                last.start + 1,
                last.start + last.part / 2,
                last.start + last.part,
                last.start + last.part + 1,
            ] {
                let mut ir = vec![0.0f32; len];
                ir[len - 1] = 1.0;
                let mut c = Convolver::with_parts(&ir, head, parts);
                let spectra = Arc::clone(c.spectra());
                assert_eq!(spectra.levels.len(), parts.len(), "the tail level exists");
                let mut x = vec![0.0f32; 1];
                x[0] = 1.0;
                let got = render(&mut c, &x, &[3, 50]);
                for (n, &s) in got.iter().enumerate() {
                    let want = if n == len - 1 { 1.0 } else { 0.0 };
                    assert!(
                        (s - want).abs() < 1e-4,
                        "parts {parts:?}, {len} taps: frame {n} is {s} want {want}"
                    );
                }
            }
        }
    }

    /// The render does not depend on how the host cuts its calls: odd
    /// lengths, one frame, a call longer than the longest partition, and the
    /// per-sample path all give the same bits, through every level.
    ///
    /// Mutation (run): `accumulate_to` ignoring `accumulated` (re-adding the
    /// partitions already in, as often as there are calls) fails the bitwise
    /// comparison.
    #[test]
    fn the_render_is_independent_of_the_call_lengths() {
        let x = noise(2500);
        for (head, parts) in LAYOUTS {
            let ir = flat_ir(700);
            let mut per_sample = Convolver::with_parts(&ir, head, parts);
            let want: Vec<f32> = x.iter().map(|&s| per_sample.process_sample(s)).collect();
            let patterns: [&[usize]; 7] = [
                &[3],
                &[7, 13, 1],
                &[head - 1, head + 1],
                &[61, 64, 127],
                &[509],
                &[1000, 1, 2],
                &[5000],
            ];
            for lens in patterns {
                let got = run_blocks(&mut Convolver::with_parts(&ir, head, parts), &x, lens);
                assert!(
                    got.iter()
                        .zip(&want)
                        .all(|(g, w)| g.to_bits() == w.to_bits()),
                    "parts {parts:?}, calls {lens:?}"
                );
            }
        }
        // The layout the cost model picks for a long IR, at odd calls.
        let ir = flat_ir(6000);
        let mut per_sample = Convolver::new(&ir, 32);
        assert!(per_sample.spectra().levels.len() > 1);
        let want: Vec<f32> = x.iter().map(|&s| per_sample.process_sample(s)).collect();
        let got = run_blocks(&mut Convolver::new(&ir, 32), &x, &[33, 5, 100, 1, 31]);
        assert!(
            got.iter()
                .zip(&want)
                .all(|(g, w)| g.to_bits() == w.to_bits())
        );
    }

    /// The layout of every length and head is well formed: levels are
    /// contiguous from tap 0 and cover the IR with no empty last partition;
    /// a tail level starts at its own partition size, which is the slack that
    /// makes its output due `block_size` frames after its uniform latency;
    /// the offsets of its two transforms fall in that slack, in order, inside
    /// one partition; and the rings hold what the levels read and write.
    ///
    /// Mutations (run): `next / part` for `next / part - 1` in `build_plan`
    /// starts the third level of a forced layout past its own partition
    /// size → fails the start check; `output_ring` sized from the largest
    /// `part` alone (no `delay`) fails the ring check and the
    /// direct-convolution test, whose tail blocks then wrap onto frames not
    /// yet read.
    #[test]
    fn layouts_are_contiguous_and_schedule_inside_their_slack() {
        // Forced layouts of three and four levels follow the same rule.
        for (head, parts) in LAYOUTS {
            let plan = build_plan(1 << 20, parts);
            for (j, p) in plan.iter().enumerate().skip(1) {
                assert_eq!(p.start, p.part, "{parts:?} level {j}");
                assert_eq!(
                    p.start,
                    plan[j - 1].start + plan[j - 1].count * plan[j - 1].part
                );
            }
            assert_eq!(plan[0].count * head, parts.get(1).copied().unwrap_or(head));
        }
        for head in [2usize, 4, 16, 64, 512, 2048, 8192] {
            for len in [1, 2, 5, 100, 1000, 3000, 24_000, 96_000, 240_000] {
                let ir = vec![0.25f32; len];
                let s = IrSpectra::new(&ir, head);
                let plan = plan_levels(len, head);
                assert_eq!(plan.len(), s.levels.len());
                let mut next = 0;
                for (j, (p, l)) in plan.iter().zip(&s.levels).enumerate() {
                    assert_eq!(p.start, next, "head {head}, {len} taps: contiguous");
                    next += p.count * p.part;
                    assert!(p.start < len, "no level starts past the IR");
                    assert!(p.part <= MAX_PARTITION.max(head));
                    assert!(p.part == head || p.part <= MAX_TAIL_RATIO * head);
                    assert_eq!(l.delay, p.start + head - p.part);
                    if j > 0 {
                        assert_eq!(p.start, p.part, "a tail level starts at its own size");
                        assert!(l.delay >= head, "slack of a head partition");
                    } else {
                        assert_eq!((l.delay, &l.steps[..]), (0, &[0usize, 0][..]));
                    }
                    assert!(l.steps.windows(2).all(|w| w[0] <= w[1]));
                    assert!(l.steps.iter().all(|&o| o <= l.delay && o < l.part));
                    assert!(0 < l.forward_steps && l.forward_steps < l.steps.len());
                    assert!(s.input_ring >= l.part + l.steps[0]);
                    assert!(s.output_ring > l.delay + l.part);
                    assert!(s.input_ring.is_power_of_two() && s.output_ring.is_power_of_two());
                }
                let last = plan.last().unwrap();
                assert!(
                    next >= len && next - last.part < len,
                    "no empty last partition"
                );
            }
        }
    }

    /// The cost model keeps the uniform layout where an extra level does not
    /// pay, and cuts a long IR into a head and larger tail partitions at most
    /// [`MAX_TAIL_RATIO`] times the head; a head of [`MAX_PARTITION`] or more
    /// has no larger partition to offer. A chosen non-uniform layout costs at
    /// most [`MIN_GAIN`] of the uniform one.
    ///
    /// Mutations (run): `MIN_GAIN` raised to 2.0 sends the 0.5 s IR (modelled
    /// at 0.81 of the uniform cost, which measures no better) to a split
    /// layout → fails; lowered to 0.0 keeps every layout uniform → the
    /// long-IR assertion fails.
    #[test]
    fn the_cost_model_picks_uniform_for_short_irs_and_levels_for_long_ones() {
        let shape =
            |len: usize, head: usize| Convolver::new(&vec![0.1; len], head).spectra().shape();
        assert_eq!(shape(0, 512), vec![]);
        assert_eq!(shape(100, 512), vec![(512, 1)]);
        assert_eq!(shape(3 * 512, 512), vec![(512, 3)]);
        assert_eq!(shape(24_000, 512), vec![(512, 47)]);
        assert_eq!(shape(240_000, 8192), vec![(8192, 30)]);
        let long = shape(240_000, 512);
        assert!(long.len() >= 2, "{long:?}");
        assert_eq!(long[0].0, 512);
        assert!(long[1..].iter().all(|&(p, _)| p > 512 && p <= 4 * 512));
        let covered: usize = long.iter().map(|&(p, c)| p * c).sum();
        assert!(covered >= 240_000);
        let plan = plan_levels(240_000, 512);
        let uniform = [Plan {
            part: 512,
            start: 0,
            count: 240_000usize.div_ceil(512),
        }];
        assert!(plan_cost(&plan) <= MIN_GAIN * plan_cost(&uniform));
    }

    /// No host block carries two steps of the tail transforms, and a split
    /// transform's steps land in different blocks. With calls of one slot
    /// (the head partition over the tail levels' steps plus one), no call
    /// holds two steps of one level or steps of two levels, and the head's
    /// two (it has no slack) share a call with nothing else. The last two
    /// layouts split their tail transforms into up to eight and sixteen
    /// sub-FFTs, so each transform's steps are spread over that many calls.
    ///
    /// Mutations (run): every tail step given the offset of the first (all at
    /// `slot`) puts several steps in one call → fails; `max_sub` ignored
    /// (`ways = 1`) leaves the split layout with one step per transform → the
    /// `forward_steps` check fails.
    #[test]
    fn transforms_land_in_different_host_blocks() {
        let steps_run = |st: &LevelState, lv: &Level| {
            ((st.base - lv.part as u64) / lv.part as u64) as usize * lv.steps.len() + st.step
        };
        for (head, parts, max_sub) in [
            (64usize, &[64usize, 128, 256][..], 1024usize),
            (64, &[64, 256], 1024),
            (32, &[32, 64, 128], 1024),
            (512, &[512, 1024, 2048], 256),
            (128, &[128, 256, 512], 32),
        ] {
            let ir = flat_ir(3000);
            let mut c = Convolver::with_parts_split(&ir, head, parts, max_sub);
            let split = c.spectra.levels.iter().any(|l| l.forward_steps > 1);
            assert_eq!(split, max_sub < 1024, "{parts:?}");
            let tail: usize = c.spectra.levels[1..].iter().map(|l| l.steps.len()).sum();
            let call = head / (tail + 1);
            let x = vec![0.1f32; call];
            let mut out = vec![0.0f32; call];
            let mut busy_calls = 0;
            for k in 0..20 * 4096 / call {
                let before: Vec<usize> = c
                    .levels
                    .iter()
                    .zip(&c.spectra.levels)
                    .map(|(st, lv)| steps_run(st, lv))
                    .collect();
                c.process_block(&x, &mut out);
                let events: Vec<usize> = c
                    .levels
                    .iter()
                    .zip(&c.spectra.levels)
                    .zip(&before)
                    .map(|((st, lv), &b)| steps_run(st, lv) - b)
                    .collect();
                let total: usize = events.iter().sum();
                if total > 0 {
                    busy_calls += 1;
                }
                if events[0] > 0 {
                    assert_eq!(total, 2, "call {k}: the head's pair is alone: {events:?}");
                } else {
                    assert!(total <= 1, "call {k}: {events:?}");
                }
            }
            assert!(busy_calls > 20);
        }
    }

    /// Layouts whose tail transforms are cut into sub-FFTs: head, partitions
    /// and the largest sub-FFT. The last cuts a 64-frame partition into
    /// sixteen, more steps than it has frames of slack, so its steps share
    /// frames.
    const SPLIT_LAYOUTS: [(usize, &[usize], usize); 4] = [
        (64, &[64, 256], 64),
        (32, &[32, 64, 128], 16),
        (16, &[16, 64], 8),
        (16, &[16, 64], 4),
    ];

    /// A split transform is a convolution: every split layout, at IR lengths
    /// on and around each level's first and last tap, equals the direct
    /// convolution within [`tolerance`] (the splitting adds a combine pass,
    /// not a transform stage, so the bound is the unsplit one's), and equals
    /// the same layout unsplit within twice that, being off from the exact
    /// sum by at most the tolerance each. Call lengths divide a partition,
    /// straddle the steps' frames, and exceed the longest partition.
    ///
    /// Mutations (run): the spectra built with the last forward step left
    /// out (`0..fft.forward_steps() - 1`) fails every length that reaches a
    /// tail level; `inverse` leaving out the newest partition's multiply
    /// fails the same.
    #[test]
    fn split_output_equals_direct_convolution() {
        let x = noise(900);
        let patterns: [&[usize]; 4] = [&[1], &[7, 13], &[64, 3, 127], &[5000]];
        for (head, parts, max_sub) in SPLIT_LAYOUTS {
            for len in boundary_lengths(head, parts) {
                let ir = flat_ir(len);
                let want = direct_convolve(&x, &ir);
                let tol = tolerance(&ir, &x, 2 * parts[parts.len() - 1]);
                for lens in patterns {
                    let mut c = Convolver::with_parts_split(&ir, head, parts, max_sub);
                    assert!(
                        len <= parts[1] || c.spectra.levels.iter().any(|l| l.forward_steps > 1)
                    );
                    let got = render(&mut c, &x, lens);
                    let unsplit = render(&mut Convolver::with_parts(&ir, head, parts), &x, lens);
                    for (i, w) in want.iter().enumerate() {
                        assert!(
                            (got[i] - w).abs() <= tol && (got[i] - unsplit[i]).abs() <= 2.0 * tol,
                            "parts {parts:?}/{max_sub}, {len} taps, calls {lens:?}: frame {i} is {} \
                             want {w} (unsplit {}, tol {tol})",
                            got[i],
                            unsplit[i]
                        );
                    }
                    let rest = got[want.len()..]
                        .iter()
                        .fold(0.0f32, |a, &s| a.max(s.abs()));
                    assert!(
                        rest <= tol,
                        "parts {parts:?}/{max_sub}, {len} taps: ringing after the tail {rest}"
                    );
                }
            }
        }
    }

    /// The render of a split layout does not depend on how the host cuts its
    /// calls: one frame at a time, odd lengths, and calls that hold several
    /// steps or none give the same bits, though the steps of a transform
    /// land in different calls.
    ///
    /// Mutation (run): `process_block` running the whole call as one run
    /// (not cut at the frames of the steps) runs the steps after later frames
    /// are written, and fails the bitwise comparison.
    #[test]
    fn a_split_render_is_independent_of_the_call_lengths() {
        let x = noise(2500);
        for (head, parts, max_sub) in SPLIT_LAYOUTS {
            let ir = flat_ir(700);
            let mut per_sample = Convolver::with_parts_split(&ir, head, parts, max_sub);
            let want: Vec<f32> = x.iter().map(|&s| per_sample.process_sample(s)).collect();
            let patterns: [&[usize]; 5] = [
                &[3],
                &[7, 13, 1],
                &[head - 1, head + 1],
                &[61, 64, 127],
                &[5000],
            ];
            for lens in patterns {
                let mut c = Convolver::with_parts_split(&ir, head, parts, max_sub);
                let got = run_blocks(&mut c, &x, lens);
                assert!(
                    got.iter()
                        .zip(&want)
                        .all(|(g, w)| g.to_bits() == w.to_bits()),
                    "parts {parts:?}/{max_sub}, calls {lens:?}"
                );
            }
        }
    }

    /// An empty IR is silence at the declared latency, with no panic.
    ///
    /// Mutation (run): `unwrap_or(1)` for the next transform frame of a
    /// convolver with no level never advances past that frame: the call
    /// hangs (checked under a timeout).
    #[test]
    fn an_empty_ir_is_silence() {
        let mut c = Convolver::new(&[], 64);
        let x = noise(300);
        let got = run_blocks(&mut c, &x, &[7, 100]);
        assert!(got.iter().all(|&s| s == 0.0));
        assert_eq!(c.latency(), 64);
    }

    /// `reset` clears every level, not just the head: after a reset, silence
    /// in is silence out, with a tail level mid-flight.
    ///
    /// Mutation (run): `reset` calling `st.reset` for the first level only
    /// → the tail level's overlap and history ring out → fails.
    #[test]
    fn reset_clears_every_level() {
        let ir = flat_ir(400);
        let mut c = Convolver::with_parts(&ir, 16, &[16, 64]);
        let x = noise(500);
        run_blocks(&mut c, &x, &[13]);
        c.reset();
        let silence = vec![0.0f32; 1000];
        let got = run_blocks(&mut c, &silence, &[29]);
        assert!(got.iter().all(|&s| s == 0.0));
    }

    /// After `reset`, a convolver renders a signal bit for bit as a fresh one
    /// does, whatever junk it had taken in before: the reset restores the
    /// whole running state (every level's schedule, sums, history and
    /// overlap, and both rings), not just the silence. Junk lengths cross
    /// slots, level partitions and ring wraps, with call lengths that
    /// straddle them; layouts are non-uniform, uniform with several
    /// partitions, and the cost model's own pick for a long IR.
    ///
    /// Mutations (run), each fails: `LevelState::reset` without
    /// `self.step = 0` (a split transform resumes part-way); without
    /// `self.base = ...` (the steps run on the old schedule);
    /// `Convolver::reset` without `self.output.fill` (the old output ring
    /// plays); `self.pushed = 0` left out (the rings' positions and the
    /// schedule disagree).
    #[test]
    fn a_reset_convolver_renders_as_a_fresh_one() {
        let x = noise(1500);
        let ir = flat_ir(1200);
        let cases: Vec<(&str, Box<dyn Fn() -> Convolver>)> = vec![
            (
                "non-uniform [16, 64]",
                Box::new(|| Convolver::with_parts(&ir, 16, &[16, 64])),
            ),
            (
                "non-uniform [8, 16, 32, 64]",
                Box::new(|| Convolver::with_parts(&ir, 8, &[8, 16, 32, 64])),
            ),
            (
                "uniform [16]",
                Box::new(|| Convolver::with_parts(&ir, 16, &[16])),
            ),
            (
                "uniform [64]",
                Box::new(|| Convolver::with_parts(&ir, 64, &[64])),
            ),
            (
                "split [16, 64] in eight",
                Box::new(|| Convolver::with_parts_split(&ir, 16, &[16, 64], 8)),
            ),
            (
                "cost model, 512",
                Box::new(|| Convolver::new(&flat_ir(100_000), 512)),
            ),
        ];
        for (name, make) in &cases {
            let want = render(&mut make(), &x, &[61]);
            for n in [1, 37, 700, 2049, 8191] {
                for lens in [&[1usize][..], &[13, 100], &[5000]] {
                    let mut c = make();
                    run_blocks(&mut c, &noise(n + 3)[3..], lens);
                    c.reset();
                    let got = render(&mut c, &x, &[61]);
                    assert!(
                        got.iter()
                            .zip(&want)
                            .all(|(a, b)| a.to_bits() == b.to_bits()),
                        "{name}: after {n} junk frames in calls {lens:?} the render differs from a fresh one's"
                    );
                }
            }
        }
    }
}
