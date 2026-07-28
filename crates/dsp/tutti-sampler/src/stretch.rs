//! Time-stretching and pitch-shifting via phase vocoder.
//!
//! Changes duration WITHOUT changing pitch, or pitch without duration — the
//! operation varispeed cannot express, since resampling couples the two. See
//! [`PlaybackRate`](tutti_core::PlaybackRate) for the coupled kind.
//!
//! [`Unit`] is a pure frame-in → frame-out filter: it owns no source, so the
//! caller ticks its own source and feeds each frame in. That is why this is a
//! peer of `playback` rather than part of it — nothing here knows what a voice
//! is.
//!
//! # Example
//!
//! ```ignore
//! use tutti_sampler::stretch;
//! use tutti_core::{Cents, StretchFactor};
//!
//! // The stretcher is a pure filter: the caller ticks its own source and feeds
//! // each frame in (it owns no source of its own).
//! let stretched = stretch::Unit::new(44100.0);
//!
//! stretched.set_stretch_factor(StretchFactor::new(2.0)); // half speed
//! stretched.set_pitch_cents(Cents::new(1200.0));         // up an octave
//! ```
//!
//! # Algorithm
//!
//! 1. **Analysis** — window the input with a periodic Hann, forward FFT.
//! 2. **Phase unwrapping** — instantaneous frequency from phase differences.
//! 3. **Synthesis** — scale phases for pitch, inverse FFT, overlap-add.
//!
//! # RT-safety
//!
//! Every buffer is allocated in [`Unit::with_fft_size_and_channels`]. Neither
//! `tick` nor `process` allocates, and neither does the vocoder beneath them.

use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

use tutti_analysis::{window::hann, StftGeometry};
use tutti_core::{
    inverse_fft, real_fft, AtomicF32, AudioThreadCell, AudioUnit, BufferMut, BufferRef, Cents,
    Complex32, Ordering, Radians, ReadRate, RtScratch, SampleRate, Samples, Seconds, SignalFrame,
    StretchFactor,
};

/// The vocoder bank, shared by refcount across graph generations.
///
/// `Net::commit` clones every node per graph edit, and a deep copy of this is
/// ~96 KB per channel allocated and the previous generation's freed — measured
/// at 201.8 MB per commit over 640 stereo nodes, with allocation and free
/// exactly balanced. Sharing makes the commit clone a refcount bump and removes
/// both halves: 81.5 MB at stereo, 243.8 MB at six channels.
///
/// # The invariant, and why it is enforced rather than documented
///
/// **At most one live handle may tick a given bank.** Sharing is sound only
/// because generations are ticked one at a time: `commit_inner` hands the
/// backend a new net and takes the old one back for deallocation, so the two
/// never run together.
///
/// Break that and the failure is quiet. Two handles ticking one bank do not
/// race or panic — they *interleave*, each consuming samples the other expected,
/// producing audio that is plausible and wrong. That is the same failure shape
/// as the 60 dB gain bug this module already carries a warning about, and it is
/// exactly what no test catches by asserting output is non-zero.
///
/// `AudioThreadCell` cannot catch it: its debug flag detects *concurrent*
/// borrows, and interleaved ticking is sequential. So the bank carries its own
/// [`Bank::ticker`] token — the id of the handle allowed to tick it. A handle
/// claims the bank on its first tick, and a second handle claiming an
/// already-claimed bank trips a `debug_assert` naming both.
///
/// The one genuinely concurrent case is the offline region render, which
/// `clone_isolated`s the live net and ticks it on a worker pool **while the
/// audio thread plays the original**. That is severed in
/// [`AudioUnit::isolate`], which the render's isolation pass already calls on
/// every node of the clone before it reaches the worker. Share on the hot path,
/// deep-copy on the rare one.
struct Bank {
    channels: AudioThreadCell<Vec<Vocoder>>,

    /// Per-block working buffers, one pair per channel.
    ///
    /// Here rather than on [`Unit`] because they follow the same rule the
    /// vocoders do: only the one handle holding the bank's claim may touch them,
    /// and they carry nothing between blocks. Leaving them on the handle meant a
    /// fresh 64 KB per channel per generation — after the bank was shared, that
    /// was **98% of a commit's remaining traffic at both widths** (240 MB of
    /// 243.8 at six channels). Deferring their allocation to
    /// [`AudioUnit::allocate`] had only moved when it was paid, not whether.
    scratch_in: AudioThreadCell<Vec<RtScratch<f32>>>,
    scratch_out: AudioThreadCell<Vec<RtScratch<f32>>>,
    /// Which [`Unit`] may tick this bank; `UNCLAIMED` until the first tick.
    ///
    /// Not a borrow flag — a claim. It outlives any single call, which is what
    /// makes it able to see the interleaving a per-call guard cannot.
    ticker: AtomicUsize,
}

/// No handle has ticked this bank yet.
const UNCLAIMED: usize = 0;

/// Source of [`Unit::id`]. Starts at 1 so no handle can collide with
/// [`UNCLAIMED`].
static NEXT_HANDLE_ID: AtomicUsize = AtomicUsize::new(1);

/// A fresh handle identity.
fn next_handle_id() -> usize {
    NEXT_HANDLE_ID.fetch_add(1, Ordering::Relaxed)
}

impl Bank {
    fn new(channels: Vec<Vocoder>) -> Arc<Self> {
        // Size the scratch here, not lazily in `allocate`. A directly
        // constructed unit must be usable without the graph's help — the RT
        // guard `time_stretch_process_is_allocation_free` builds one and ticks
        // it straight away, and leaving it unsized made `process` allocate in
        // the callback. Construction is not on the commit path (a clone inherits
        // an already-sized bank), so this costs nothing per graph edit.
        let width = channels.len();
        Arc::new(Self {
            channels: AudioThreadCell::new(channels),
            scratch_in: AudioThreadCell::new(
                (0..width)
                    .map(|_| RtScratch::new(MAX_BUFFER_SIZE))
                    .collect(),
            ),
            scratch_out: AudioThreadCell::new(
                (0..width)
                    .map(|_| RtScratch::new(MAX_BUFFER_SIZE))
                    .collect(),
            ),
            ticker: AtomicUsize::new(UNCLAIMED),
        })
    }

    /// Whether the block scratch is sized for `width` channels.
    fn scratch_is_ready(&self, width: usize) -> bool {
        let scratch = self.scratch_in.borrow();
        scratch.len() == width && scratch.iter().all(|s| s.capacity() >= MAX_BUFFER_SIZE)
    }

    /// Size the block scratch. Idempotent; control thread only.
    fn allocate_scratch(&self, width: usize) {
        if self.scratch_is_ready(width) {
            return;
        }
        *self.scratch_in.borrow_mut() = (0..width)
            .map(|_| RtScratch::new(MAX_BUFFER_SIZE))
            .collect();
        *self.scratch_out.borrow_mut() = (0..width)
            .map(|_| RtScratch::new(MAX_BUFFER_SIZE))
            .collect();
    }

    /// Assert `who` is allowed to tick, claiming the bank if it is unclaimed.
    ///
    /// Debug-only, and deliberately so: in release this compiles away, leaving
    /// the sharing at full speed. The claim is what a test can assert against —
    /// see `two_live_handles_ticking_one_bank_is_caught`.
    #[inline]
    fn claim(&self, who: usize) {
        #[cfg(debug_assertions)]
        {
            let prev = self
                .ticker
                .compare_exchange(UNCLAIMED, who, Ordering::AcqRel, Ordering::Acquire)
                .unwrap_or_else(|actual| actual);
            debug_assert!(
                prev == UNCLAIMED || prev == who,
                "BUG: two live stretch::Unit handles are ticking one shared vocoder \
                 bank (owner {prev:#x}, caller {who:#x}). They will interleave and \
                 render plausible-but-wrong audio. A clone that is ticked \
                 independently must call `AudioUnit::isolate` first."
            );
        }
        #[cfg(not(debug_assertions))]
        let _ = who;
    }

    /// Give up `who`'s claim, if it holds one.
    ///
    /// Called when a handle is dropped. Without this a retired generation keeps
    /// its claim forever and its legitimate successor looks like an interleave —
    /// which is exactly what the first version of
    /// `a_successor_generation_continues_the_stream` hit.
    ///
    /// Conditional, not an unconditional store: a handle that never ticked, or
    /// one whose bank has already been taken over, must not clear someone else's
    /// claim.
    #[inline]
    fn release(&self, who: usize) {
        let _ = self
            .ticker
            .compare_exchange(who, UNCLAIMED, Ordering::AcqRel, Ordering::Acquire);
    }

    /// Hand ticking rights to `who`, forgetting any previous claim.
    ///
    /// Used where a handle legitimately succeeds another on the same bank: a
    /// committed generation replaces the one it was cloned from, and `reset`
    /// starts the stream over.
    #[inline]
    fn reclaim(&self, who: usize) {
        self.ticker.store(who, Ordering::Release);
    }
}

/// An analysis window length, in samples.
///
/// A newtype over the sample count rather than a `Small`/`Medium`/`Large` enum:
/// the quantity that matters is the number itself — it sets frequency
/// resolution and latency — and a t-shirt size names neither. The constants
/// below are the useful presets; [`new`](Self::new) admits any other valid
/// length.
///
/// Two invariants hold for every instance, and both are enforced in `new`
/// rather than argued about at the use site:
///
/// - **A power of two between 2 and 32768**, because [`real_fft`] dispatches on
///   exactly those lengths and panics otherwise.
/// - **Divisible by 4**, so [`hop`](Self::hop) is `size / 4` exactly — 75%
///   overlap, the minimum Hann² constant-overlap-add requires. Any power of two
///   ≥ 4 satisfies this; it is stated because the hop, not the window, is what
///   COLA constrains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FftSize(usize);

impl FftSize {
    /// 1024 samples — ~23 ms at 44.1 kHz. Live performance.
    pub const N1024: Self = Self(1024);
    /// 2048 samples — ~46 ms at 44.1 kHz. The default.
    pub const N2048: Self = Self(2048);
    /// 4096 samples — ~93 ms at 44.1 kHz. Mixing.
    pub const N4096: Self = Self(4096);
    /// 8192 samples — ~186 ms at 44.1 kHz. Extreme stretching (Paulstretch).
    pub const N8192: Self = Self(8192);

    /// Smallest window `real_fft` can transform that still admits a `/4` hop.
    pub const MIN: Self = Self(4);
    /// Largest window `real_fft` can transform.
    pub const MAX: Self = Self(32768);

    /// Every preset, for tests and for enumerating a UI.
    pub const PRESETS: [Self; 4] = [Self::N1024, Self::N2048, Self::N4096, Self::N8192];

    /// A window length, or `None` if it is not a power of two in
    /// [`MIN`](Self::MIN)..=[`MAX`](Self::MAX).
    ///
    /// Fallible rather than clamping: silently rounding 1000 up to 1024 would
    /// hand back a unit whose latency is not the one the caller asked for, and
    /// `real_fft` panics on the un-rounded value — so the caller has to know.
    pub const fn new(size: usize) -> Option<Self> {
        if size.is_power_of_two() && size >= Self::MIN.0 && size <= Self::MAX.0 {
            Some(Self(size))
        } else {
            None
        }
    }

    /// The window length in samples.
    pub const fn size(self) -> Samples {
        Samples(self.0)
    }

    /// One quarter of the window: 75% overlap, the minimum Hann² COLA needs.
    pub const fn hop(self) -> Samples {
        Samples(self.0 / 4)
    }

    /// The delay this window imposes: one whole window must arrive before the
    /// first frame can be analyzed.
    pub fn latency(self, sample_rate: impl Into<SampleRate>) -> Seconds {
        Seconds(self.0 as f32 / sample_rate.into().get() as f32)
    }
}

impl Default for FftSize {
    fn default() -> Self {
        Self::N2048
    }
}

impl std::fmt::Display for FftSize {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The wrap arithmetic both rings below share.
///
/// A power-of-two capacity plus two monotonic cursors. The cursors never wrap,
/// so "how much is unconsumed" is a plain subtraction and cannot be confused
/// with the empty case; only the *index* wraps, via [`mask`](Self::mask) —
/// which is why the capacity is forced to a power of two rather than merely
/// being one in practice.
///
/// Deliberately not a public container. It is the shared half of two rings with
/// genuinely different interfaces ([`SampleFifo`], [`OverlapAdd`]), and exposing
/// the union of their operations is what let the original code index one ring
/// with the other's cursor.
struct Ring {
    data: Vec<f32>,
    write: usize,
    read: usize,
}

impl Ring {
    /// `capacity` is rounded up to a power of two so wrapping is a mask.
    fn new(capacity: usize) -> Self {
        Self {
            data: vec![0.0; capacity.next_power_of_two()],
            write: 0,
            read: 0,
        }
    }

    #[inline]
    fn mask(&self, i: usize) -> usize {
        i & (self.data.len() - 1)
    }

    /// Samples written but not yet consumed, **capped at the capacity**.
    ///
    /// The cap is the load-bearing part. The cursors are monotonic so the raw
    /// subtraction keeps counting past the ring end, and a reader that trusted it
    /// would hand back slots overwritten laps ago as though they were fresh — a
    /// plausible-sounding wrong answer rather than a detectable failure. That is
    /// exactly how the vocoder's input-rate bug stayed hidden: `available()`
    /// reported 79,231 pending in a 4,096-sample ring and every caller believed
    /// it.
    ///
    /// Capping does not *fix* an overrun — the data is already gone. It bounds
    /// the damage to "the oldest samples were dropped" instead of "the stream is
    /// silently interleaved with stale laps", and it makes
    /// [`Ring::overrun`](Self::overrun) meaningful.
    #[inline]
    fn available(&self) -> usize {
        self.write.saturating_sub(self.read).min(self.data.len())
    }

    /// Whether more has been written than the ring can hold — i.e. unread
    /// samples were overwritten. Always a bug in the *caller's* rate matching,
    /// never something the ring can recover from, so it is exposed for tests to
    /// assert against rather than handled here.
    #[cfg(test)]
    #[inline]
    fn overrun(&self) -> bool {
        self.write.saturating_sub(self.read) > self.data.len()
    }

    fn reset(&mut self) {
        self.data.fill(0.0);
        self.write = 0;
        self.read = 0;
    }
}

/// The vocoder's input side: a plain FIFO.
///
/// Samples arrive in blocks of whatever size the host hands down, and are
/// consumed a window at a time with the read cursor advancing by one *hop* —
/// so a frame is read four times over at 75% overlap. That is why this reads
/// through [`peek`](Self::peek) and advances separately, rather than popping:
/// the data outlives any single read.
struct SampleFifo(Ring);

impl SampleFifo {
    fn new(capacity: usize) -> Self {
        Self(Ring::new(capacity))
    }

    #[inline]
    fn available(&self) -> usize {
        self.0.available()
    }

    #[inline]
    fn push(&mut self, samples: &[f32]) {
        for &s in samples {
            let i = self.0.mask(self.0.write);
            self.0.data[i] = s;
            self.0.write += 1;
        }
    }

    /// Sample at `offset` past the read cursor, without consuming it.
    #[inline]
    fn peek(&self, offset: usize) -> f32 {
        self.0.data[self.0.mask(self.0.read + offset)]
    }

    /// Consume `count` samples. Separate from [`peek`](Self::peek) because one
    /// frame is read `window / hop` times before being retired.
    #[inline]
    fn consume(&mut self, count: usize) {
        self.0.read += count;
    }

    fn reset(&mut self) {
        self.0.reset();
    }
}

/// The vocoder's output side: an overlap-add accumulator.
///
/// Not a FIFO, which is why it is its own type. Synthesis *sums into* a whole
/// window starting at the write cursor — overlapping the tails of the previous
/// three frames — and only then advances by one synthesis hop. So writes land
/// **ahead** of the cursor and are revisited by later frames, while reads drain
/// behind it. A FIFO's `push` cannot express that.
struct OverlapAdd(Ring);

impl OverlapAdd {
    fn new(capacity: usize) -> Self {
        Self(Ring::new(capacity))
    }

    #[inline]
    fn available(&self) -> usize {
        self.0.available()
    }

    /// See [`Ring::overrun`]. On this side an overrun means the caller fed the
    /// vocoder input slower than it is publishing output.
    #[cfg(test)]
    #[inline]
    fn overrun(&self) -> bool {
        self.0.overrun()
    }

    /// Sum `value` into the slot `offset` past the write cursor.
    ///
    /// Flushes subnormals: this accumulator is IIR-like, so on a silent tail it
    /// decays into the subnormal range where x86 FPUs trap into microcode and
    /// spike. Snapping to zero never changes audible output — subnormals are
    /// below ~1.2e-38, far under the noise floor of 32-bit audio.
    #[inline]
    fn add_at(&mut self, offset: usize, value: f32) {
        let i = self.0.mask(self.0.write + offset);
        let sum = self.0.data[i] + value;
        self.0.data[i] = if sum.is_subnormal() { 0.0 } else { sum };
    }

    /// Zero the slot `offset` past the write cursor, before a later frame sums
    /// into it. Without this the ring would accumulate stale audio from a
    /// previous lap.
    #[inline]
    fn clear_at(&mut self, offset: usize) {
        let i = self.0.mask(self.0.write + offset);
        self.0.data[i] = 0.0;
    }

    /// Advance the write cursor by one synthesis hop, publishing that many
    /// finished samples to the reader.
    #[inline]
    fn advance(&mut self, hop: usize) {
        self.0.write += hop;
    }

    /// Drain up to `out.len()` finished samples. Returns how many were
    /// available; the rest of `out` is left untouched.
    #[inline]
    fn drain(&mut self, out: &mut [f32]) -> usize {
        let count = out.len().min(self.available());
        for (i, slot) in out.iter_mut().take(count).enumerate() {
            *slot = self.0.data[self.0.mask(self.0.read + i)];
        }
        self.0.read += count;
        count
    }

    fn reset(&mut self) {
        self.0.reset();
    }
}

/// One channel of phase-vocoder state.
///
/// Sized once, at construction; `process` allocates nothing.
struct Vocoder {
    geometry: StftGeometry,
    /// The Hann analysis/synthesis window.
    ///
    /// `Arc` because it is immutable for the vocoder's lifetime and identical for
    /// every channel and every clone — and because building it costs `size`
    /// `cos()` calls, which `Net::commit`'s deep clone was paying per channel per
    /// node. Sharing turns that into a refcount bump.
    window: Arc<Vec<f32>>,

    /// Real scratch handed to [`real_fft`], which transforms it in place.
    fft_buffer: Vec<f32>,
    /// Full spectrum: `DC..=Nyquist` written by analysis, the conjugate half
    /// rebuilt before the inverse transform.
    spectrum: Vec<Complex32>,
    /// Per-bin synthesis phase, accumulated across frames.
    phase_accumulator: Vec<Radians>,
    /// Per-bin analysis phase from the previous frame.
    last_phase: Vec<Radians>,
    /// Per-bin phase advance produced by **one sample** of analysis hop.
    /// Multiplied by the frame's analysis hop, which varies with stretch.
    ///
    /// `Arc` for the same reason as `window`: a function of the geometry alone,
    /// immutable for the vocoder's lifetime, and identical across every clone.
    phase_per_sample: Arc<Vec<Radians>>,

    input: SampleFifo,
    output: OverlapAdd,
}

impl Vocoder {
    fn new(geometry: StftGeometry) -> Self {
        let size = geometry.window().get();
        let bins = geometry.bins_per_frame().get();
        // 2π·k/size — the phase bin `k` advances **per sample** of analysis hop.
        // No sample-rate term: it is a ratio of sample counts, which is why
        // changing the rate does not invalidate it.
        //
        // Stored per-sample rather than per-hop because the analysis hop is no
        // longer fixed — see `process_frame`. Multiplying by the frame's actual
        // hop is one multiply on a table read that already happens.
        let phase_per_sample = (0..bins)
            .map(|k| Radians(Radians::TAU.get() * k as f32 / size as f32))
            .collect();

        Self {
            geometry,
            window: Arc::new(hann(size)),
            fft_buffer: vec![0.0; size],
            spectrum: vec![Complex32::new(0.0, 0.0); size],
            phase_accumulator: vec![Radians(0.0); bins],
            last_phase: vec![Radians(0.0); bins],
            phase_per_sample: Arc::new(phase_per_sample),
            // 4x the window: three frames of overlap-add tail plus the frame
            // being written.
            input: SampleFifo::new(size * 4),
            output: OverlapAdd::new(size * 4),
        }
    }

    /// A fresh vocoder on the same grid, sharing everything immutable.
    ///
    /// A clone starts with clean phase history (see [`Unit::clone`]), so no state
    /// is copied — only the *shapes* carry. The Hann window and the per-bin phase
    /// table are both functions of the geometry alone, so they are shared rather
    /// than rebuilt; rebuilding cost `size` `cos()` calls per vocoder per commit.
    ///
    /// # What this costs on a graph commit
    ///
    /// This runs from `Net::commit`, once per channel per node, and the clone is
    /// **kept** — `commit_inner` clones the net, `core::mem::swap`s the vertex
    /// vectors so the ORIGINALS ship to the backend ("necessary if the nodes
    /// contain any backends, which cannot be cloned effectively"), and the
    /// freshly-built clones stay on the frontend as the next generation's source.
    /// So the allocation is not waste; it is the price of double-buffering, paid
    /// once per commit per vocoder: ~100 KB of mutable state, 64% of it the two
    /// `size * 4` rings.
    ///
    /// Profiled (`examples/profile_stretch_clone.rs`, run under `samply`), the
    /// cost splits **~42% allocator, ~37% `memset`** — allocating the buffers and
    /// zeroing them, in nearly equal measure. Kernel time is 1.3%, so this is
    /// real work rather than the paging artifact an earlier wall-clock benchmark
    /// suggested. That benchmark's figures (18.5 ms / 628 ms, quoted in earlier
    /// revisions of this comment) also measured two live generations at once,
    /// which is 5-14x more expensive than the one-at-a-time shape `commit_inner`
    /// actually produces — so they overstated a commit by about an order of
    /// magnitude.
    ///
    /// **That 37% is why a buffer pool was built here and then removed.** A pool
    /// recycles the allocation but a recycled buffer still has to be cleared, and
    /// the clear is the same `memset` as a fresh `vec![0.0; n]` — so pooling can
    /// only address the allocator's 42%, and only when the pool is non-empty.
    /// Here it never is: `commit_inner` clones *before* it retires the previous
    /// generation, so nothing has been returned at the moment the clone asks.
    /// Measured, `Buffers::new` and a pooled hit came out identical within noise.
    ///
    /// **What did work: not cloning what carries nothing.** The block scratch
    /// (`scratch_in`/`scratch_out`, 64 KB per channel — 40% of a unit's bytes)
    /// is overwritten every block before it is read, so a clone leaves it empty
    /// and [`AudioUnit::allocate`] sizes it. That removes both halves of the
    /// cost for those bytes, because a buffer never allocated is also never
    /// zeroed. Re-profiled, the clone phase fell 461 → 215 samples (-53%) while
    /// `fresh_construction`, which still allocates eagerly, held at 180 → 179 —
    /// the control that says the drop is this change and not the machine.
    ///
    /// # This is no longer on the commit path
    ///
    /// `Unit::clone` shares the vocoder bank by refcount (see [`Bank`]), so a
    /// graph commit does not reach this function at all. It runs only from
    /// [`AudioUnit::isolate`], where an offline render needs private state.
    ///
    /// The history is worth keeping, because it is what the design was measured
    /// against. When a commit *did* deep-clone: 201.8 MB per commit at stereo and
    /// 604.6 MB at six channels, median 70-135 ms and 393-488 ms against a 2 ms
    /// budget. Sharing the bank took that to 81.5 / 243.8 MB, and moving the
    /// block scratch onto the bank as well took it to **1.3 / 3.3 MB** — a ~180x
    /// reduction, with both widths committing in ~0.2 ms. What remains is
    /// fundsp's own per-`Vertex` bookkeeping, not this state.
    fn clone_fresh(&self) -> Self {
        let size = self.geometry.window().get();
        let bins = self.geometry.bins_per_frame().get();
        Self {
            geometry: self.geometry,
            window: Arc::clone(&self.window),
            fft_buffer: vec![0.0; size],
            spectrum: vec![Complex32::new(0.0, 0.0); size],
            phase_accumulator: vec![Radians(0.0); bins],
            last_phase: vec![Radians(0.0); bins],
            phase_per_sample: Arc::clone(&self.phase_per_sample),
            input: SampleFifo::new(size * 4),
            output: OverlapAdd::new(size * 4),
        }
    }

    fn reset(&mut self) {
        self.fft_buffer.fill(0.0);
        self.spectrum.fill(Complex32::new(0.0, 0.0));
        self.phase_accumulator.fill(Radians(0.0));
        self.last_phase.fill(Radians(0.0));
        self.input.reset();
        self.output.reset();
    }

    /// Drain every whole frame the input holds.
    ///
    /// `analysis_hop` is how far through the SOURCE each frame steps;
    /// `synthesis_hop` is how much finished output each frame publishes. Their
    /// ratio is the time-scaling, and which one varies depends on who drives the
    /// rate — see [`Unit::tick`].
    fn process(&mut self, analysis_hop: usize, synthesis_hop: usize, pitch_ratio: f32) {
        while self.input.available() >= self.geometry.window().get() {
            self.process_frame(analysis_hop, synthesis_hop, pitch_ratio);
        }
    }

    fn process_frame(&mut self, analysis_hop: usize, synthesis_hop: usize, pitch_ratio: f32) {
        let size = self.geometry.window().get();
        let bins = self.geometry.bins_per_frame().get();

        // 1. Window the frame into the FFT scratch.
        for i in 0..size {
            self.fft_buffer[i] = self.input.peek(i) * self.window[i];
        }
        self.input.consume(analysis_hop);

        // 2. Forward transform. `real_fft` returns size/2 bins and packs
        //    Nyquist into DC's imaginary part, so unpack both before treating
        //    any bin as a magnitude/phase pair — reading bin 0 as-is mixes two
        //    unrelated frequencies into one bogus polar value.
        let packed = real_fft(&mut self.fft_buffer);
        self.spectrum[..packed.len()].copy_from_slice(packed);
        let dc = self.spectrum[0].re;
        let nyquist = self.spectrum[0].im;
        self.spectrum[0] = Complex32::new(dc, 0.0);
        self.spectrum[bins - 1] = Complex32::new(nyquist, 0.0);

        // 3. Per-bin phase advance, scaled for pitch and re-accumulated.
        let hop_ratio = synthesis_hop as f32 / analysis_hop as f32;
        for k in 0..bins {
            let magnitude = self.spectrum[k].norm();
            let phase = Radians(self.spectrum[k].arg());

            // Deviation of the observed advance from the expected one, wrapped
            // into (-π, π] — the unwrapping step that recovers the bin's true
            // instantaneous frequency rather than its aliased one.
            let expected = Radians(self.phase_per_sample[k].get() * analysis_hop as f32);
            let deviation = wrap_phase(phase - self.last_phase[k] - expected);
            let true_advance = expected + deviation;

            self.phase_accumulator[k] =
                wrap_phase(self.phase_accumulator[k] + true_advance * pitch_ratio * hop_ratio);
            self.last_phase[k] = phase;

            self.spectrum[k] = Complex32::from_polar(magnitude, self.phase_accumulator[k].get());
        }

        // 4. Rebuild the conjugate half so the inverse transform is real.
        for i in 1..bins - 1 {
            self.spectrum[size - i] = self.spectrum[i].conj();
        }

        // 5. Inverse transform, then window and overlap-add.
        //
        // No `1 / size` here: microfft's inverse already normalizes, so a
        // forward-then-inverse pair is the identity (pinned by
        // `fft_roundtrip_is_the_identity`). The original code divided anyway,
        // attenuating the stretched signal by the FFT size — 60 dB at 1024, 66
        // at 2048. It read as "stretching mutes the voice" rather than as a
        // gain bug, which is why it survived: every test asserted only that
        // output was non-zero, and 0.0004 is non-zero.
        inverse_fft(&mut self.spectrum);
        for i in 0..size {
            self.output
                .add_at(i, self.spectrum[i].re * self.window[i] * COLA_GAIN);
        }

        // Zero the span the next frame will accumulate into, which this one has
        // already scrolled past.
        for i in 0..synthesis_hop {
            self.output.clear_at(size + i);
        }
        self.output.advance(synthesis_hop);
    }
}

/// Wrap a phase into (-π, π].
///
/// Arithmetic, not a `while` loop: the loop it replaces ran once per 2π of
/// input, so a large accumulated phase cost unbounded iterations on the audio
/// thread.
#[inline]
fn wrap_phase(phase: Radians) -> Radians {
    let tau = Radians::TAU.get();
    let p = phase.get();
    Radians(p - tau * ((p + std::f32::consts::PI) / tau).floor())
}

/// Overlap-add normalization for a Hann analysis/synthesis pair at 75% overlap.
///
/// Windowing twice — once on analysis, once on synthesis — means the overlapped
/// frames sum to `Σ hann²` per sample rather than to unity. At a hop of
/// `window / 4` that sum is exactly `4 × mean(hann²) = 4 × 3/8 = 1.5`, so
/// synthesis divides it back out. Without this the vocoder is 3.5 dB hot.
///
/// Only correct at 75% overlap, which is why [`FftSize::hop`] is fixed at
/// `size / 4` rather than configurable.
const COLA_GAIN: f32 = 1.0 / 1.5;

/// Maximum block size pre-allocated per channel (covers any common interface).
const MAX_BUFFER_SIZE: usize = 8192;

/// Real-time time-stretching and pitch-shifting unit.
///
/// A pure frame-in → frame-out **filter**: it owns NO source. The caller ticks
/// the real audio source itself and feeds the resulting frame in as this unit's
/// `input`; what lives here is the latent phase-vocoder state (one [`Vocoder`]
/// per channel, the scratch buffers, the atomics).
///
/// # Channels
///
/// One [`Vocoder`] per channel, plus one in/out [`RtScratch`] pair each. The
/// vocoders are **independent** — there is no phase locking between them, so a
/// correlated source can drift channel-to-channel. That was already true of the
/// original stereo pair; widening does not make it worse, and fixing it is a
/// separate question from width.
pub struct Unit {
    /// One per channel; the bank's length **is** the unit's width, so the
    /// scratch vectors are always the same length.
    ///
    /// Shared across graph generations — see [`Bank`]. `width` mirrors the
    /// length so `inputs()`/`outputs()` need no borrow: fundsp calls them during
    /// graph planning, where taking a borrow would collide with a live one.
    channels: Arc<Bank>,
    width: usize,

    /// This handle's identity for [`Bank::claim`], unique among live handles.
    ///
    /// A counter, **not** `self as *const Self`. The address is not an identity:
    /// `Net::push(Box::new(unit))` moves the value, so a handle that claimed the
    /// bank before the move could never release its own claim afterwards — the
    /// guard would then fire on the legitimate successor. Found the hard way, by
    /// this exact bug in `a_successor_generation_continues_the_stream`.
    id: usize,
    stretch_factor: Arc<AtomicF32>,
    pitch_cents: Arc<AtomicF32>,
    enabled: bool,
    /// Fractional debt in the source-intake resampler: how much of the next
    /// source sample the unit still owes itself before it may consume one.
    ///
    /// A time-stretcher emits `stretch` samples per source sample, but
    /// [`AudioUnit::tick`] hands over exactly one and takes exactly one back. The
    /// only way to satisfy both is for the unit to consume the source at
    /// `1 / stretch` internally — dropping input above unity, repeating it below —
    /// which is what this accumulator paces. See [`Unit::hops`].
    ///
    /// One accumulator for all channels: they share a stretch factor, so
    /// per-channel debts would always be equal and could only drift through a
    /// bug that skewed the channels against each other.
    intake_debt: f64,
}

// Hand-rolled: holds non-`Debug` vocoders and `RtScratch` buffers. Print the
// live stretch/pitch atomics and the enabled flag.
impl std::fmt::Debug for Unit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Unit")
            .field("channels", &self.width)
            .field("enabled", &self.enabled)
            .field("stretch_factor", &self.stretch_factor())
            .field("pitch_cents", &self.pitch_cents())
            .finish_non_exhaustive()
    }
}

impl Unit {
    /// Stereo, at the default FFT size.
    pub fn new(sample_rate: impl Into<SampleRate>) -> Self {
        Self::with_fft_size_and_channels(sample_rate, FftSize::default(), 2)
    }

    /// Stereo, at a custom FFT size.
    pub fn with_fft_size(sample_rate: impl Into<SampleRate>, fft_size: FftSize) -> Self {
        Self::with_fft_size_and_channels(sample_rate, fft_size, 2)
    }

    /// `channels` wide, at the default FFT size.
    pub fn with_channels(sample_rate: impl Into<SampleRate>, channels: usize) -> Self {
        Self::with_fft_size_and_channels(sample_rate, FftSize::default(), channels)
    }

    /// Full constructor: custom FFT size at a custom width.
    ///
    /// Width is fixed here because it sizes one vocoder and two scratch buffers
    /// per channel, all allocated on this path so the RT path never does.
    /// Callers must pass the width of the source they will feed in: a narrower
    /// stretcher silently truncates the frames handed to `tick`.
    pub fn with_fft_size_and_channels(
        sample_rate: impl Into<SampleRate>,
        fft_size: FftSize,
        channels: usize,
    ) -> Self {
        let geometry = Self::geometry(sample_rate, fft_size);
        // A zero-wide filter has nothing to process and would make `inputs()` /
        // `outputs()` lie to the graph.
        let channels = channels.max(1);
        Self {
            channels: Bank::new((0..channels).map(|_| Vocoder::new(geometry)).collect()),
            width: channels,
            id: next_handle_id(),
            stretch_factor: Arc::new(AtomicF32::new(StretchFactor::UNITY.get())),
            pitch_cents: Arc::new(AtomicF32::new(0.0)),
            enabled: true,
            intake_debt: 0.0,
        }
    }

    /// The analysis grid, which is COLA-valid for every [`FftSize`].
    ///
    /// `cola` is fallible in general — a hop that does not divide its window,
    /// or overlaps under 75%, cannot reconstruct. Neither is reachable here:
    /// `FftSize::hop` is exactly `size / 4`. The rate is clamped because a
    /// non-positive one is the remaining rejectable input, and a stretcher that
    /// panicked on a device reporting 0 Hz would be worse than one that runs at
    /// a nominal rate.
    fn geometry(sample_rate: impl Into<SampleRate>, fft_size: FftSize) -> StftGeometry {
        let rate = sample_rate.into().get().max(1.0);
        StftGeometry::cola(rate, fft_size.size(), fft_size.hop())
            .expect("BUG: FftSize hop is size/4, which is COLA-valid at a positive rate")
    }

    /// Channel width — the number of vocoders, and this unit's in/out arity.
    pub fn channels(&self) -> usize {
        self.width
    }

    /// Clamped into [`StretchFactor::MIN`]..=[`StretchFactor::MAX`].
    pub fn set_stretch_factor(&self, factor: StretchFactor) {
        self.stretch_factor.store(
            StretchFactor::new_clamped(factor.get()).get(),
            Ordering::Release,
        );
    }

    pub fn stretch_factor(&self) -> StretchFactor {
        StretchFactor::new(self.stretch_factor.load(Ordering::Acquire))
    }

    /// Arc for lock-free external control.
    pub fn stretch_factor_arc(&self) -> Arc<AtomicF32> {
        Arc::clone(&self.stretch_factor)
    }

    /// Clamped to ±2 octaves.
    pub fn set_pitch_cents(&self, cents: Cents) {
        self.pitch_cents.store(
            cents.get().clamp(MIN_PITCH_CENTS, MAX_PITCH_CENTS),
            Ordering::Release,
        );
    }

    pub fn pitch_cents(&self) -> Cents {
        Cents::new(self.pitch_cents.load(Ordering::Acquire))
    }

    /// Arc for lock-free external control.
    pub fn pitch_cents_arc(&self) -> Arc<AtomicF32> {
        Arc::clone(&self.pitch_cents)
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Whether the unit is doing anything but passing audio through.
    pub fn is_processing(&self) -> bool {
        self.enabled
            && ((self.stretch_factor().get() - StretchFactor::UNITY.get()).abs() > STRETCH_EPSILON
                || self.pitch_cents().get().abs() > PITCH_EPSILON_CENTS)
    }

    /// Whether these two units share one vocoder bank.
    ///
    /// Exposed so callers that must sever sharing before running a clone on
    /// another thread can *assert* they did — the alternative is trusting that
    /// [`AudioUnit::isolate`] was reached, which is exactly the assumption that
    /// shipped a data race in `VoiceNode`. Sharing is otherwise invisible from
    /// outside this module: it changes no output until two threads race, and by
    /// then nothing is observable in a test.
    pub fn shares_bank_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.channels, &other.channels)
    }

    /// Processing latency, in samples — **zero while bypassing**.
    ///
    /// One whole window must arrive before the first frame can be analysed, so a
    /// processing unit delays by exactly that. A bypassing one does not: `tick`
    /// and `process` copy input to output directly at unity stretch and pitch, or
    /// when disabled.
    ///
    /// The bypass case is what `route` reports to fundsp's PDC, and reporting a
    /// window there while the audio passes straight through makes every other
    /// branch of the graph get delayed to compensate for a delay that does not
    /// exist — 46 ms at the default 2048 window. It is reachable through ordinary
    /// use, not only at construction: `VoiceSlot::set_stretch` keeps the resident
    /// filter and writes its atomics, so returning a stretched voice to 1.0 leaves
    /// a filter sitting at unity.
    ///
    /// Every channel reports the same value (a function of the shared FFT size),
    /// so channel 0 speaks for all.
    pub fn latency_samples(&self) -> usize {
        if !self.is_processing() {
            return 0;
        }
        self.channels
            .channels
            .borrow()
            .first()
            .map_or(0, |v| v.geometry.window().get())
    }

    /// The (analysis, synthesis) hop pair for the current stretch factor.
    ///
    /// The **synthesis** hop is pinned to the grid's natural `size / 4`, and the
    /// **analysis** hop is `synthesis / stretch`. Their ratio is still exactly
    /// `stretch`, which is what the phase accumulator needs to hold pitch fixed
    /// while duration changes.
    ///
    /// Pinning synthesis is a COLA requirement, not a preference. Overlap-add
    /// reconstruction needs the synthesis frames to overlap by at least 75% for a
    /// Hann-squared pair to sum to a constant; the synthesis hop is what sets that
    /// overlap, and the window is fixed. Scaling synthesis *up* with the stretch
    /// factor — the textbook offline formulation — walks the overlap down as the
    /// factor rises: 62% at 1.5x, 50% at 2x, and at 4x the hop equals the whole
    /// window, so consecutive frames abut with NO overlap at all. The Hann²
    /// envelopes then ripple instead of summing flat, and the output amplitude
    /// modulates at the frame rate. Measured as 16 of 256 blocks dipping under a
    /// tenth of full level at 4x, on a perfectly steady input.
    ///
    /// Scaling analysis down instead keeps every factor at the same 75% overlap
    /// the grid was built for, and `Unit::geometry` asserts that grid is COLA-valid.
    ///
    /// Both hops are clamped to at least 1: a zero analysis hop would re-read the
    /// same frame forever, and a zero synthesis hop would advance the output ring
    /// nowhere and spin `process` in an infinite loop.
    #[inline]
    fn hops(&self) -> (usize, usize) {
        let synthesis = self
            .channels
            .channels
            .borrow()
            .first()
            .map_or(1, |v| v.geometry.hop().get());
        let analysis = ((synthesis as f32 / self.stretch_factor().get()).round() as usize).max(1);
        (analysis, synthesis.max(1))
    }

    /// Source samples this unit consumes per output sample: `1 / stretch`.
    ///
    /// A time-stretcher is a rate changer, and `AudioUnit::tick` is one-in /
    /// one-out — so the *caller* has to supply the difference. A placed voice
    /// reads by derived position, so it can simply scale that position: at
    /// `stretch = 2.0` the source advances half a sample per output sample, and
    /// the vocoder spreads it back over the full duration at unchanged pitch.
    ///
    /// This is NOT varispeed, though it looks identical in isolation. Varispeed is
    /// slow-reading *alone*, which drops pitch by the same factor. Here the
    /// vocoder's `synthesis / analysis` ratio undoes exactly that shift — the two
    /// halves only work together, which is why this rate belongs to the unit that
    /// knows the stretch factor rather than to the call site.
    ///
    /// `1.0` when the unit is bypassing, so a caller can apply it unconditionally.
    #[inline]
    pub fn input_rate(&self) -> ReadRate {
        if !self.is_processing() {
            return ReadRate::UNITY;
        }
        let (analysis, synthesis) = self.hops();
        ReadRate(analysis as f64 / synthesis as f64)
    }
}

/// Below this, a stretch factor is indistinguishable from unity and the unit
/// bypasses rather than paying for an FFT.
const STRETCH_EPSILON: f32 = 0.001;
/// Half a cent — beneath the threshold of hearing.
const PITCH_EPSILON_CENTS: f32 = 0.5;
/// Two octaves down.
const MIN_PITCH_CENTS: f32 = -2400.0;
/// Two octaves up.
const MAX_PITCH_CENTS: f32 = 2400.0;

impl Clone for Unit {
    fn clone(&self) -> Self {
        // Fresh vocoder state rather than cloned: the phase accumulators and
        // overlap-add rings are mid-frame history, and a clone is a new voice
        // rather than a continuation of this one. Only the parameters carry.
        //
        // The block scratch is left **empty**, not cloned. It carries nothing
        // across blocks — `process` overwrites `scratch_in` from its input and
        // `fill(0.0)`s `scratch_out` before draining into it — so copying 64 KB
        // per channel only to overwrite it was 40% of a clone's bytes buying
        // nothing. `allocate` sizes it, which is exactly the hook fundsp
        // documents for "buffers for block processing" and which `Net::commit`
        // calls on the graph it is about to run.
        let mut cloned = Self {
            // The whole point: a refcount bump, not ~96 KB per channel.
            channels: Arc::clone(&self.channels),
            width: self.width,
            // A distinct identity: the clone is a different live handle, and the
            // guard exists precisely to tell it apart from its predecessor.
            id: next_handle_id(),
            stretch_factor: Arc::new(AtomicF32::new(self.stretch_factor.load(Ordering::Acquire))),
            pitch_cents: Arc::new(AtomicF32::new(self.pitch_cents.load(Ordering::Acquire))),
            enabled: self.enabled,
            intake_debt: 0.0,
        };
        cloned.enabled = self.enabled;
        cloned
    }
}

impl Drop for Unit {
    /// Release this handle's claim on the shared bank.
    ///
    /// A commit retires the previous generation, and its successor must be able
    /// to tick the bank it inherited. Without this the claim outlives the handle
    /// and every post-commit tick trips the guard.
    ///
    /// The body is release-only, but **dropping a `Unit` is not free**: after it
    /// runs, `channels: Arc<Bank>` is dropped too, and when that is the last
    /// reference the vocoders and block scratch (~192 KB at six channels) are
    /// deallocated right there. This comment used to claim the opposite, which
    /// was true of the body and false of the type.
    ///
    /// That matters because `VoiceCommand::Remove` retires a slot inside
    /// `drain_commands`, which runs from the audio callback. `VoicePool` now
    /// hands removed slots to a retirement channel so the free happens on the
    /// control thread — see `VoicePool::retired`. Any *other* caller dropping a
    /// `Unit` on the audio thread has the same hazard and needs the same
    /// treatment.
    fn drop(&mut self) {
        self.channels.release(self.id);
    }
}

impl AudioUnit for Unit {
    fn inputs(&self) -> usize {
        // A filter: it consumes the frame the caller feeds in (already tick'd
        // from the real audio source), one channel per vocoder.
        self.channels()
    }

    fn outputs(&self) -> usize {
        self.channels()
    }

    fn reset(&mut self) {
        // A reset restarts the stream, so it also transfers ticking rights: this
        // is the legitimate way a successor generation takes over a bank without
        // tripping the claim.
        self.channels.reclaim(self.id);
        for v in self.channels.channels.borrow_mut().iter_mut() {
            v.reset();
        }
        self.intake_debt = 0.0;
    }

    fn set_sample_rate(&mut self, sample_rate: SampleRate) {
        // The grid's window and hop are sample counts and its phase table is
        // their ratio, so none of the vocoder state depends on the rate. Only
        // the rate the geometry reports back does — rebuild it, and leave the
        // running phase history alone.
        for v in self.channels.channels.borrow_mut().iter_mut() {
            v.geometry = Self::geometry(sample_rate, FftSize::default());
        }
    }

    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        // `input` is the source frame the caller already produced (in-memory index
        // or streaming ring pop). This unit does not own or pull a source.
        //
        // A short `input` fans channel 0 to the rest: a mono feed into a wider
        // stretcher stays audible on every channel rather than going silent
        // past the first.
        let n = self.channels().min(output.len());
        let src0 = input.first().copied().unwrap_or(0.0);
        let src = |c: usize| input.get(c).copied().unwrap_or(src0);

        if !self.is_processing() {
            for (c, o) in output.iter_mut().enumerate().take(n) {
                *o = src(c);
            }
            return;
        }

        let (analysis_hop, synthesis_hop) = self.hops();
        let pitch_ratio = self.pitch_cents().to_pitch_ratio();

        // Pace the source intake at `1 / stretch` — see `intake_debt`. Above
        // unity this drops input samples; below it, feeds the same one twice.
        self.intake_debt += self.input_rate().get();
        // One borrow for the whole call: the cell's contract is one borrow at a
        // time, and re-borrowing per sample would also cost a debug atomic each.
        self.channels.claim(self.id);
        let mut bank = self.channels.channels.borrow_mut();
        while self.intake_debt >= 1.0 {
            self.intake_debt -= 1.0;
            for (c, v) in bank.iter_mut().enumerate() {
                v.input.push(&[src(c)]);
                v.process(analysis_hop, synthesis_hop, pitch_ratio);
            }
        }

        let mut one = [0.0f32];
        for (c, o) in output.iter_mut().enumerate().take(n) {
            one[0] = 0.0;
            bank[c].output.drain(&mut one);
            *o = one[0];
        }
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        // `size` past MAX_BUFFER_SIZE is clamped by `RtScratch::active`; the
        // fixed capacity makes a per-block reallocation impossible.
        //
        // One scratch pair per channel (not one flat strided buffer): the
        // vocoder API is per-channel `push`/`pop` over a contiguous run of
        // samples, so channel-major is what it wants. A frame-major buffer
        // would need a de-interleave here and a re-interleave after, for no
        // gain.
        let channels = self.channels();
        let in_ch = input.channels();

        // A clone shares the bank but leaves its scratch for `allocate` to size.
        // If that never ran, `RtScratch::active` clamps to a zero-length slice
        // and every loop below iterates zero times — the unit would emit silence
        // and look like a gain bug, the same failure shape that hid a 60 dB
        // error here before. Size it here instead: this is the control thread's
        // job, but a late allocation beats silent silence, and the debug assert
        // names the real fault. Every RT call on an allocated unit skips it.
        if !self.channels.scratch_is_ready(self.width) {
            debug_assert!(
                false,
                "BUG: stretch::Unit::process before allocate(); the graph must \
                 call allocate() on a cloned unit before running it"
            );
            self.channels.allocate_scratch(self.width);
        }

        // One claim and one borrow-set for the whole call. The scratch lives on
        // the bank now, so it is covered by the same claim that protects the
        // vocoders — a second live handle reaching this would be caught rather
        // than silently sharing working buffers.
        self.channels.claim(self.id);
        let mut scratch_in = self.channels.scratch_in.borrow_mut();

        for (c, s) in scratch_in.iter_mut().enumerate() {
            let buf = s.active(size);
            // Fewer input channels than vocoders: mirror `tick`'s
            // fan-from-channel-0 rather than emitting silence.
            let src_ch = if c < in_ch { c } else { 0 };
            for (i, b) in buf.iter_mut().enumerate().take(size) {
                *b = input.at_f32(src_ch, i);
            }
        }

        let out_ch = output.channels().min(channels);

        if !self.is_processing() {
            for c in 0..out_ch {
                let buf = scratch_in[c].active_ref(size);
                for (i, &s) in buf.iter().enumerate().take(size) {
                    output.set_f32(c, i, s);
                }
            }
            return;
        }

        let (analysis_hop, synthesis_hop) = self.hops();
        let pitch_ratio = self.pitch_cents().to_pitch_ratio();
        let rate = self.input_rate();

        // Same intake pacing as `tick`, applied per sample of the block so the
        // two entry points consume the source identically. Walking the block
        // rather than pushing it whole is what keeps `process` and `tick`
        // producing the same audio — they drifted apart once before by writing
        // the two paths separately.
        let mut bank = self.channels.channels.borrow_mut();
        for i in 0..size {
            self.intake_debt += rate.get();
            while self.intake_debt >= 1.0 {
                self.intake_debt -= 1.0;
                for (c, v) in bank.iter_mut().enumerate() {
                    let sample = scratch_in[c].active_ref(size)[i];
                    v.input.push(&[sample]);
                    v.process(analysis_hop, synthesis_hop, pitch_ratio);
                }
            }
        }

        let mut scratch_out = self.channels.scratch_out.borrow_mut();
        for c in 0..channels {
            let out = scratch_out[c].active(size);
            out.fill(0.0);
            let count = bank[c].output.drain(out);
            if c >= out_ch {
                continue;
            }
            for (i, &s) in out.iter().enumerate().take(size) {
                output.set_f32(c, i, if i < count { s } else { 0.0 });
            }
        }
    }

    audio_unit_boilerplate!(id = crate::node_id::TIME_STRETCH_ID);

    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        // As a filter, the incoming `input` frame IS the source signal. Width
        // must track `outputs()` or fundsp mis-plans this node's latency.
        let channels = self.channels();
        let mut out = SignalFrame::new(channels);
        let latency = self.latency_samples() as f64;
        let first = input.at(0).delay(latency);
        for c in 0..channels {
            let sig = if c < input.len() {
                input.at(c).delay(latency)
            } else {
                first
            };
            out.set(c, sig);
        }
        out
    }

    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>()
    }

    /// Sever the shared vocoder bank, giving this unit private state.
    ///
    /// **This is what makes sharing sound.** `Unit::clone` hands out a refcount
    /// bump, which is safe only while generations are ticked one at a time. The
    /// offline region render breaks that: it `clone_isolated`s the live net and
    /// ticks it on a worker pool while the audio thread plays the original — two
    /// generations, two threads, concurrently. Sharing the FIFOs and phase
    /// accumulators there would corrupt both the render and playback.
    ///
    /// The render's isolation pass already calls this on every node of the clone
    /// before it reaches the worker, so the deep copy lands exactly where
    /// concurrency begins and nowhere else. Cost is the ~96 KB per channel that
    /// the commit path no longer pays, on a path that is already
    /// admission-capped for being expensive.
    ///
    /// Fresh state rather than a copy of the running one, matching what
    /// `Unit::clone` used to produce: an isolated render starts its filter clean
    /// rather than mid-frame on audio it will not emit.
    fn isolate(&mut self) {
        let geometry = self
            .channels
            .channels
            .borrow()
            .first()
            .map(|v| v.geometry)
            .unwrap_or_else(|| Self::geometry(SampleRate(44_100.0), FftSize::default()));
        let fresh: Vec<Vocoder> = self
            .channels
            .channels
            .borrow()
            .iter()
            .map(Vocoder::clone_fresh)
            .collect();
        let _ = geometry;
        self.channels = Bank::new(fresh);
        self.channels.reclaim(self.id);
        self.intake_debt = 0.0;
    }

    /// Size the per-block scratch. Idempotent, and never called from the audio
    /// thread.
    ///
    /// This is what makes [`Unit::clone`] cheap: the clone leaves the scratch
    /// empty, and the graph calls this before running the unit
    /// (`Net::commit_inner` → `Net::allocate` → `Vertex::allocate`, and
    /// `Net::set_unit` for a hot swap). Re-allocating an already-sized unit
    /// would be a needless 64 KB per channel, so a ready unit returns early.
    fn allocate(&mut self) {
        self.channels.allocate_scratch(self.width);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;
    use tutti_core::BufferVec;

    fn sine(freq: f32, sample_rate: f32, len: usize) -> Vec<f32> {
        (0..len)
            .map(|i| (Radians::TAU.get() * freq * i as f32 / sample_rate).sin() * 0.5)
            .collect()
    }

    #[test]
    fn fft_sizes_are_powers_of_two_with_quarter_hops() {
        for size in FftSize::PRESETS {
            assert!(size.size().get().is_power_of_two());
            assert_eq!(size.hop().get(), size.size().get() / 4);
        }
        assert_eq!(FftSize::N2048.size(), Samples(2048));
        assert_eq!(FftSize::N2048.hop(), Samples(512));
    }

    /// Every `FftSize` must satisfy COLA, because `geometry` `expect`s it.
    #[test]
    fn every_fft_size_yields_a_cola_grid() {
        for size in FftSize::PRESETS {
            let g = Unit::geometry(44_100.0, size);
            assert!(g.is_cola(), "{size:?} is not COLA-compliant");
            assert_eq!(g.window(), size.size());
        }
    }

    /// A zero or negative rate must not reach `cola`, which rejects it.
    #[test]
    fn non_positive_sample_rate_does_not_panic() {
        for rate in [0.0, -44_100.0] {
            let u = Unit::new(rate);
            assert_eq!(u.channels(), 2);
        }
    }

    #[test]
    fn wrap_phase_maps_into_a_single_turn() {
        let tau = Radians::TAU.get();
        for &p in &[0.0, PI, -PI, 3.0 * PI, -3.0 * PI, 100.0 * tau + 1.0] {
            let w = wrap_phase(Radians(p)).get();
            assert!(w > -PI - 1e-4 && w <= PI + 1e-4, "{p} wrapped to {w}");
            // Wrapping differs from the input by a whole number of turns.
            let turns = (p - w) / tau;
            assert!((turns - turns.round()).abs() < 1e-3, "{p} -> {w}");
        }
    }

    /// The `while` loop this replaced iterated once per 2π, so a large phase
    /// cost unbounded time on the audio thread. Arithmetic wrapping is O(1).
    #[test]
    fn wrap_phase_handles_a_large_accumulated_phase() {
        let w = wrap_phase(Radians(1.0e6)).get();
        assert!(w > -PI - 1e-2 && w <= PI + 1e-2, "wrapped to {w}");
    }

    /// The fact `COLA_GAIN` depends on: microfft's inverse transform already
    /// normalizes, so a forward/inverse pair is the identity and synthesis must
    /// NOT divide by the FFT size again.
    ///
    /// The original code did divide, attenuating stretched audio by 60–78 dB
    /// depending on window. If a future FFT backend returns an unnormalized
    /// inverse, this fails loudly here rather than showing up as a quiet
    /// stretcher.
    #[test]
    fn fft_roundtrip_is_the_identity() {
        let n = 1024usize;
        let orig: Vec<f32> = (0..n)
            .map(|i| (Radians::TAU.get() * 5.0 * i as f32 / n as f32).sin() * 0.5)
            .collect();

        let mut buf = orig.clone();
        let packed = real_fft(&mut buf);
        let bins = n / 2 + 1;
        let mut spectrum = vec![Complex32::new(0.0, 0.0); n];
        spectrum[..packed.len()].copy_from_slice(packed);
        let (dc, nyquist) = (spectrum[0].re, spectrum[0].im);
        spectrum[0] = Complex32::new(dc, 0.0);
        spectrum[bins - 1] = Complex32::new(nyquist, 0.0);
        for i in 1..bins - 1 {
            spectrum[n - i] = spectrum[i].conj();
        }
        inverse_fft(&mut spectrum);

        for (i, &want) in orig.iter().enumerate() {
            let got = spectrum[i].re;
            assert!(
                (got - want).abs() < 1e-4,
                "sample {i}: {got} != {want} — inverse_fft normalization changed"
            );
        }
    }

    #[test]
    fn fifo_peeks_without_consuming() {
        let mut f = SampleFifo::new(8);
        f.push(&[1.0, 2.0, 3.0]);
        assert_eq!(f.available(), 3);

        // The defining property of the input side: reading does not consume, so
        // the same frame can be read once per hop.
        assert_eq!(f.peek(0), 1.0);
        assert_eq!(f.peek(0), 1.0);
        assert_eq!(f.peek(2), 3.0);
        assert_eq!(f.available(), 3);

        f.consume(2);
        assert_eq!(f.available(), 1);
        assert_eq!(f.peek(0), 3.0);
    }

    /// The index wraps but the cursors do not, so a write past the ring end
    /// keeps `available` exact rather than folding it to zero.
    #[test]
    fn fifo_wraps_the_index_not_the_cursors() {
        let mut f = SampleFifo::new(8);
        f.push(&[1.0, 2.0, 3.0]);
        f.consume(2);
        f.push(&[4.0; 7]);
        assert_eq!(f.available(), 8);
        // Read cursor is at 2, so the oldest live sample is still 3.0.
        assert_eq!(f.peek(0), 3.0);
    }

    #[test]
    fn overlap_add_accumulates_ahead_and_drains_behind() {
        let mut o = OverlapAdd::new(8);

        // Two frames summing into overlapping spans — the operation a FIFO
        // cannot express.
        o.add_at(0, 0.5);
        o.add_at(1, 0.5);
        o.advance(1);
        o.add_at(0, 0.25); // lands on the slot the previous frame's offset 1 hit

        let mut out = [0.0; 2];
        assert_eq!(o.drain(&mut out), 1, "only one hop has been published");
        assert_eq!(out[0], 0.5);

        o.advance(1);
        assert_eq!(o.drain(&mut out), 1);
        assert_eq!(out[0], 0.75, "0.5 + 0.25 accumulated in one slot");
    }

    #[test]
    fn overlap_add_drain_reports_short_reads() {
        let mut o = OverlapAdd::new(8);
        o.add_at(0, 1.0);
        o.advance(1);

        let mut out = [0.0; 4];
        assert_eq!(o.drain(&mut out), 1);
        assert_eq!(out, [1.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn overlap_add_clear_at_zeroes_a_future_slot() {
        let mut o = OverlapAdd::new(8);
        o.add_at(3, 1.0);
        o.clear_at(3);
        o.advance(4);

        let mut out = [0.0; 4];
        assert_eq!(o.drain(&mut out), 4);
        assert_eq!(out[3], 0.0, "cleared slot must not carry stale audio");
    }

    /// `available()` must not exceed the capacity, and an overrun must be
    /// reportable.
    ///
    /// The cursors are monotonic, so before the cap this returned 12 for a
    /// 4-slot ring and `drain` cheerfully served eight slots that had been
    /// overwritten twice. A caller cannot distinguish that from real audio — it
    /// is the mechanism that hid the vocoder's input-rate bug for the life of the
    /// file.
    #[test]
    fn ring_available_saturates_at_capacity_and_reports_the_overrun() {
        let mut o = OverlapAdd::new(4);
        assert!(!o.overrun());

        for i in 0..12 {
            o.add_at(0, i as f32);
            o.advance(1);
        }

        assert!(o.overrun(), "12 written into 4 slots is an overrun");
        assert_eq!(o.available(), 4, "must not claim more than the ring holds");

        let mut out = [0.0f32; 8];
        assert_eq!(o.drain(&mut out), 4, "drain is bounded by available()");
    }

    /// **The rate contract.** A one-in/one-out feed keeps both rings bounded and
    /// the output audible, at every stretch factor.
    ///
    /// This is the invariant the design rests on, and it asserts both halves:
    /// bounded rings alone would be satisfied by a unit that emitted silence.
    ///
    /// It holds because the unit paces its own source intake at
    /// [`input_rate`](Unit::input_rate) internally. A stretcher emits `stretch`
    /// samples per source sample, but `AudioUnit::tick` hands over exactly one and
    /// takes one back — so the rate change has to happen on the source side, where
    /// the unit can drop or repeat, rather than on the output side, where it
    /// cannot.
    ///
    /// The old formulation consumed one source sample per tick and published
    /// `hop * stretch` per `hop` consumed, leaving a surplus of
    /// `hop * (stretch - 1)` output samples per frame with nowhere to go: measured
    /// 79,231 pending in a 4,096-sample ring at `stretch = 2.0` — nineteen laps —
    /// which made `drain` serve overwritten audio and dropped ~32 of every 256
    /// blocks to silence.
    #[test]
    fn a_one_to_one_feed_stays_bounded_and_audible_at_every_stretch() {
        for factor in [0.25f32, 0.5, 1.5, 2.0, 4.0] {
            let mut u = Unit::with_fft_size_and_channels(44_100.0, FftSize::N1024, 1);
            u.set_stretch_factor(StretchFactor::new(factor));
            assert!(u.is_processing(), "factor {factor} should not bypass");

            let mut out = [0.0f32; 1];
            let mut n = 0usize;
            let mut feed = |u: &mut Unit, count: usize, n: &mut usize| {
                let mut peak = 0.0f32;
                for _ in 0..count {
                    let t = *n as f32 / 44_100.0;
                    let sample = 0.5 * (Radians::TAU.get() * 3000.0 * t).sin();
                    *n += 1;
                    u.tick(&[sample], &mut out);
                    peak = peak.max(out[0].abs());
                }
                peak
            };

            // Prime: the opening blocks are legitimately quiet while the FIFOs and
            // the overlap-add tail fill. At 4x the intake is a quarter rate, so
            // this has to be generous.
            feed(&mut u, 60_000, &mut n);

            let mut quiet = 0usize;
            for _ in 0..256 {
                if feed(&mut u, 64, &mut n) < 0.01 {
                    quiet += 1;
                }
                assert!(
                    !u.channels.channels.borrow()[0].output.overrun(),
                    "stretch {factor}: output ring overran ({} pending)",
                    u.channels.channels.borrow()[0].output.available()
                );
                assert!(
                    !u.channels.channels.borrow()[0].input.0.overrun(),
                    "stretch {factor}: input ring overran"
                );
            }
            assert_eq!(
                quiet, 0,
                "stretch {factor}: {quiet}/256 blocks fell silent under a steady feed"
            );
        }
    }

    /// The intake loop is **bounded**, which is an RT-safety property rather than
    /// a performance one: it runs inside the audio callback, and an unbounded
    /// `while` there is a dropout waiting for the right parameter value.
    ///
    /// `input_rate` is `1 / stretch` and [`StretchFactor::MIN`] is 0.25, so the
    /// rate can never exceed 4.0 and the loop can never run more than four times
    /// per tick. The bound comes from `set_stretch_factor` clamping on store — a
    /// raw `StretchFactor::new(0.001)` would otherwise ask for a thousand
    /// iterations.
    #[test]
    fn the_intake_loop_is_bounded_by_the_stretch_clamp() {
        let u = Unit::with_channels(44_100.0, 1);

        // Well past the clamp, in the direction that increases intake.
        u.set_stretch_factor(StretchFactor::new(0.001));
        assert_eq!(u.stretch_factor(), StretchFactor::MIN);
        assert!(
            u.input_rate().get() <= 4.0 + 1e-6,
            "intake rate {} would run the per-tick loop more than 4 times",
            u.input_rate()
        );

        // And the other end cannot drive it to zero, which would starve the FIFO.
        u.set_stretch_factor(StretchFactor::new(100.0));
        assert_eq!(u.stretch_factor(), StretchFactor::MAX);
        assert!(u.input_rate().get() > 0.0);
    }

    /// `input_rate` is `1.0` while bypassing, so a caller can apply it
    /// unconditionally without branching on `is_processing`.
    /// A clone must not rebuild the immutable tables — the measurable half of
    /// commit cost,
    /// and the only half this crate can fix without a design change.
    ///
    /// `Net::commit` deep-clones every node, once per channel. Rebuilding the Hann
    /// window there costs `size` `cos()` calls per vocoder; sharing it is a
    /// refcount bump. Asserted structurally (pointer identity) rather than by
    /// timing, because a wall-clock threshold in a test suite is a flake generator.
    ///
    /// # Commit cost is still over budget — do not proceed to per-voice nodes
    ///
    /// Measured on this machine, release, 640 stretch nodes (32 tracks x 20
    /// voices), against the 2 ms budget a graph edit has before it risks an audio
    /// dropout:
    ///
    /// | width  | before sharing | after  | budget |
    /// |--------|----------------|--------|--------|
    /// | stereo | 18.5 ms        | 12.9 ms | 2 ms  |
    /// | 6ch    | 628 ms         | 448 ms  | 2 ms  |
    ///
    /// Still 6x over at stereo and 224x at six channels. The window was never the
    /// dominant term: each `Vocoder` allocates and zeroes ~108 KB of state, so 640
    /// six-channel nodes touch ~405 MB per commit. No amount of sharing immutable
    /// data fixes that — the buffers must either be pooled (so a clone claims
    /// rather than allocates) or not cloned at all.
    ///
    /// **This is the measurement gating the container dissolve.** It says the
    /// per-voice-node design cannot land as written: 640 nodes is a realistic
    /// project, and a commit at that scale would stall the main thread long enough
    /// to underrun the callback.
    #[test]
    fn cloning_shares_the_bank_and_isolate_severs_it() {
        let u = Unit::with_channels(44_100.0, 6);
        let c = u.clone();

        // The commit path: a refcount bump, not ~96 KB per channel.
        assert!(
            Arc::ptr_eq(&u.channels, &c.channels),
            "the clone deep-copied the vocoder bank instead of sharing it"
        );
        assert_eq!(
            c.width, 6,
            "width must mirror the bank without borrowing it"
        );

        // The safety boundary. An offline render clones the live net and ticks
        // it on a worker pool WHILE the audio thread plays the original, so a
        // shared bank there would have two threads writing one set of FIFOs.
        // `isolate` is called on every node of that clone before it reaches the
        // worker, and it must hand back private state.
        let mut isolated = u.clone();
        assert!(Arc::ptr_eq(&u.channels, &isolated.channels));
        isolated.isolate();
        assert!(
            !Arc::ptr_eq(&u.channels, &isolated.channels),
            "isolate() left the render sharing the live graph's vocoder state"
        );
        assert_eq!(isolated.width, 6, "isolate must preserve the unit's width");

        // Isolated state is clean, matching what a clone used to produce: a
        // render starts its filter fresh rather than mid-frame on audio it will
        // never emit.
        assert_eq!(isolated.channels.channels.borrow()[0].input.available(), 0);
        assert_eq!(isolated.channels.channels.borrow()[0].output.available(), 0);

        // The immutable tables still ride by `Arc` through an isolate, so
        // severing does not pay to rebuild the window or the phase table.
        assert!(Arc::ptr_eq(
            &u.channels.channels.borrow()[0].window,
            &isolated.channels.channels.borrow()[0].window
        ));
        assert!(Arc::ptr_eq(
            &u.channels.channels.borrow()[0].phase_per_sample,
            &isolated.channels.channels.borrow()[0].phase_per_sample
        ));
    }

    /// The invariant has teeth: two live handles ticking one bank is caught.
    ///
    /// This is the test the whole hardening exists for. Interleaving is silent —
    /// no race, no panic, just plausible-and-wrong audio — so without a guard
    /// the only symptom is a subtly damaged render that every `!= 0.0` assertion
    /// in this file would pass.
    ///
    /// `AudioThreadCell`'s debug flag cannot catch this: it detects *concurrent*
    /// borrows, and interleaved ticking is sequential. Hence [`Bank::claim`].
    #[test]
    #[should_panic(expected = "two live stretch::Unit handles")]
    #[cfg(debug_assertions)]
    fn two_live_handles_ticking_one_bank_is_caught() {
        let mut a = Unit::with_channels(44_100.0, 2);
        a.set_stretch_factor(StretchFactor::new(2.0));
        a.allocate();

        // A committed generation, sharing `a`'s bank.
        let mut b = a.clone();
        b.allocate();

        let mut frame = [0.0f32; 2];
        // `a` claims the bank...
        a.tick(&[0.25, 0.25], &mut frame);
        // ...and `b` ticking it too is the bug. Both handles are still alive, so
        // this is the interleave case and not a legitimate succession.
        b.tick(&[0.25, 0.25], &mut frame);
    }

    /// Succession is legitimate and must NOT trip the claim.
    ///
    /// A committed generation replaces the one it was cloned from, and the live
    /// path reaches that through `reset` / `isolate`. If either tripped the
    /// guard, the guard would be unusable — so pin both directions, not just the
    /// failing one.
    #[test]
    fn succession_and_isolation_do_not_trip_the_claim() {
        let mut a = Unit::with_channels(44_100.0, 2);
        a.set_stretch_factor(StretchFactor::new(2.0));
        a.allocate();
        let mut frame = [0.0f32; 2];
        a.tick(&[0.25, 0.25], &mut frame);

        // Reset transfers ticking rights to the successor.
        let mut b = a.clone();
        b.allocate();
        b.reset();
        b.tick(&[0.25, 0.25], &mut frame);

        // Isolation gives a private bank, so the render path is free regardless.
        let mut c = b.clone();
        c.isolate();
        c.allocate();
        c.tick(&[0.25, 0.25], &mut frame);

        // And the isolated handle owns state nobody else can reach.
        assert!(!Arc::ptr_eq(&b.channels, &c.channels));
    }

    /// A successor generation continues the stream rather than restarting it.
    ///
    /// This is the payoff of sharing: a graph commit hands the next generation
    /// the same bank, so playback continues seamlessly across a graph edit
    /// instead of re-filling the vocoder and dropping ~46 ms of audio.
    ///
    /// Written second. The first attempt ticked the original and the clone while
    /// **both were alive**, which is precisely the interleave bug — and
    /// [`Bank::claim`] caught it, which is the guard earning its place on a test
    /// its author got wrong. Succession means the predecessor stops.
    #[test]
    fn a_successor_generation_continues_the_stream() {
        let mut original = Unit::with_channels(44_100.0, 2);
        original.set_stretch_factor(StretchFactor::new(2.0));
        original.allocate();

        let size = 64;
        let mut input = BufferVec::new(2);
        for i in 0..size {
            let s = (i as f32 * 0.05).sin() * 0.5;
            input.buffer_mut().set_f32(0, i, s);
            input.buffer_mut().set_f32(1, i, s);
        }
        let mut out = BufferVec::new(2);

        // Warm past the fill-up so the bank holds real history.
        for _ in 0..96 {
            original.process(size, &input.buffer_ref(), &mut out.buffer_mut());
        }
        let history = original.channels.channels.borrow()[0].input.available();
        assert!(history > 0, "the bank should hold history to inherit");

        // The commit: the successor takes the bank, the predecessor retires.
        let mut successor = original.clone();
        successor.allocate();
        assert!(
            Arc::ptr_eq(&original.channels, &successor.channels),
            "the successor should share the bank, not copy it"
        );
        drop(original);

        // The inherited state is the predecessor's, not a fresh filter's.
        assert_eq!(
            successor.channels.channels.borrow()[0].input.available(),
            history,
            "the successor restarted the stream instead of continuing it"
        );

        // And it emits immediately — no second fill-up latency after the edit.
        successor.process(size, &input.buffer_ref(), &mut out.buffer_mut());
        let heard = (0..size).any(|i| out.buffer_ref().at_f32(0, i).abs() > 1e-6);
        assert!(heard, "the successor went silent across the commit");
    }

    /// The block scratch rides the shared bank, so a commit reallocates nothing.
    ///
    /// Rewritten twice, and the history is the point. First the clone copied
    /// 64 KB per channel outright. Then it deferred that to `allocate` — which
    /// changed *when* the cost was paid, not *whether*: `Net::commit` calls
    /// `allocate` on every generation, so after the vocoder bank was shared this
    /// scratch was **98% of a commit's remaining traffic at both widths**
    /// (240 MB of 243.8 at six channels).
    ///
    /// Now it lives on the bank, under the same claim and the same isolate
    /// boundary as the vocoders, and a successor generation inherits it sized.
    #[test]
    fn the_block_scratch_rides_the_shared_bank() {
        let mut u = Unit::with_channels(44_100.0, 6);
        u.allocate();
        assert!(u.channels.scratch_is_ready(6));

        // A commit's clone shares the bank, so it inherits sized scratch and
        // `allocate` has nothing left to do.
        let c = u.clone();
        assert!(
            c.channels.scratch_is_ready(6),
            "the successor should inherit sized scratch, not reallocate it"
        );
        assert!(Arc::ptr_eq(&u.channels, &c.channels));

        // Idempotent: the graph allocates every generation, and re-sizing would
        // throw away 64 KB per channel per commit — the exact cost this removes.
        let ptr_before = u.channels.scratch_in.borrow()[0].capacity();
        let mut c2 = u.clone();
        c2.allocate();
        assert_eq!(u.channels.scratch_in.borrow()[0].capacity(), ptr_before);

        // Isolation severs it with the rest of the bank. The fresh bank is
        // sized at construction, so a render's isolated node is immediately
        // usable — the same guarantee a directly built unit has, and the one
        // `time_stretch_process_is_allocation_free` depends on.
        let mut iso = u.clone();
        iso.isolate();
        assert!(!Arc::ptr_eq(&u.channels, &iso.channels));
        assert!(
            iso.channels.scratch_is_ready(6),
            "a severed bank must arrive usable, not needing a later allocate"
        );
        iso.allocate();
        assert_eq!(iso.channels.scratch_out.borrow().len(), 6);
    }

    /// An isolated clone renders exactly what the original renders.
    ///
    /// Rewritten when the vocoder bank became shared. The previous version ticked
    /// the original and a plain clone alternately and asserted they matched
    /// sample-for-sample — which a shared bank makes meaningless, because the two
    /// handles now feed ONE FIFO and interleave rather than run in parallel. That
    /// is the design working, not a regression, but it means the property has to
    /// be asserted on an `isolate`d clone, which is the only clone that genuinely
    /// owns its state.
    ///
    /// Asserted on sample values rather than liveness — a quieter or truncated
    /// block is the failure mode, and every `!= 0.0` assertion here would pass it.
    #[test]
    fn an_isolated_clone_renders_identically() {
        let mut original = Unit::with_channels(44_100.0, 2);
        original.set_stretch_factor(StretchFactor::new(2.0));
        original.allocate();

        // The offline-render shape: clone, isolate, allocate. Isolation gives it
        // private state, so it must now track the original exactly.
        let mut clone = original.clone();
        clone.isolate();
        clone.allocate();

        // fundsp's `Buffer` is fixed at 64 samples per channel; a larger `size`
        // reads past it rather than being clamped.
        let size = 64;
        let mut input_vec = BufferVec::new(2);
        for i in 0..size {
            let s = (i as f32 * 0.05).sin() * 0.5;
            input_vec.buffer_mut().set_f32(0, i, s);
            input_vec.buffer_mut().set_f32(1, i, s);
        }
        let mut out_a = BufferVec::new(2);
        let mut out_b = BufferVec::new(2);

        // Enough blocks to clear the fill-up latency: at 2x stretch the vocoder
        // emits nothing until its FIFO holds a whole 2048-sample window, which
        // is 64 blocks of source at this size — so a handful would compare
        // silence to silence and prove nothing.
        let mut heard_signal = false;
        for block in 0..128 {
            original.process(size, &input_vec.buffer_ref(), &mut out_a.buffer_mut());
            clone.process(size, &input_vec.buffer_ref(), &mut out_b.buffer_mut());

            for ch in 0..2 {
                for i in 0..size {
                    let (x, y) = (
                        out_a.buffer_ref().at_f32(ch, i),
                        out_b.buffer_ref().at_f32(ch, i),
                    );
                    assert_eq!(
                        x, y,
                        "block {block}, channel {ch}, sample {i}: the isolated \
                         clone diverged from the original"
                    );
                    heard_signal |= x.abs() > 1e-6;
                }
            }
        }

        assert!(
            heard_signal,
            "both rendered silence — the comparison proved nothing"
        );
    }

    /// PDC must not compensate for a delay that is not happening.
    ///
    /// `route` reports `latency_samples` to fundsp, which delays every parallel
    /// branch to match. A bypassing unit copies input to output, so reporting a
    /// window there desynchronises the whole graph by 46 ms at the default 2048.
    #[test]
    fn latency_is_zero_while_bypassing_and_a_window_while_processing() {
        let mut u = Unit::with_channels(44_100.0, 2);
        let window = u.channels.channels.borrow()[0].geometry.window().get();

        assert!(!u.is_processing());
        assert_eq!(u.latency_samples(), 0, "bypassing unit claimed latency");

        u.set_stretch_factor(StretchFactor::new(2.0));
        assert_eq!(u.latency_samples(), window);

        // Reachable through ordinary use: `VoiceSlot::set_stretch` keeps the
        // resident filter and writes its atomics, so a voice returned to 1.0 is a
        // built filter sitting at unity — it must stop claiming latency.
        u.set_stretch_factor(StretchFactor::UNITY);
        assert_eq!(
            u.latency_samples(),
            0,
            "unity stretch still claimed latency"
        );

        // Pitch alone is enough to make it real processing.
        u.set_pitch_cents(Cents::new(100.0));
        assert_eq!(u.latency_samples(), window);

        // Disabling bypasses regardless of the atomics.
        u.set_enabled(false);
        assert_eq!(u.latency_samples(), 0, "disabled unit claimed latency");
    }

    #[test]
    fn input_rate_is_unity_when_bypassing() {
        let u = Unit::with_channels(44_100.0, 1);
        assert!(!u.is_processing());
        assert_eq!(u.input_rate(), ReadRate::UNITY);

        // Disabled counts as bypassing too.
        let mut u = Unit::with_channels(44_100.0, 1);
        u.set_stretch_factor(StretchFactor::new(2.0));
        u.set_enabled(false);
        assert_eq!(u.input_rate(), ReadRate::UNITY);
    }

    #[test]
    fn overlap_add_flushes_subnormals() {
        let mut o = OverlapAdd::new(4);
        o.add_at(0, f32::MIN_POSITIVE / 4.0);
        o.advance(1);

        let mut out = [0.0; 1];
        o.drain(&mut out);
        assert_eq!(out[0], 0.0);
    }

    #[test]
    fn creation_and_width() {
        let unit = Unit::new(44100.0);
        assert_eq!(unit.channels(), 2);
        assert_eq!(unit.inputs(), 2);
        assert_eq!(unit.outputs(), 2);

        assert_eq!(Unit::with_channels(44_100.0, 6).channels(), 6);
    }

    /// A zero-wide filter would make `inputs()`/`outputs()` lie to the graph.
    #[test]
    fn zero_width_is_clamped_to_one() {
        assert_eq!(Unit::with_channels(44_100.0, 0).channels(), 1);
    }

    #[test]
    fn parameters_round_trip_and_clamp() {
        let unit = Unit::new(44100.0);

        unit.set_stretch_factor(StretchFactor::new(2.0));
        assert!((unit.stretch_factor().get() - 2.0).abs() < 0.001);
        unit.set_pitch_cents(Cents::new(-200.0));
        assert!((unit.pitch_cents().get() + 200.0).abs() < 0.001);

        // Clamped at the unit type's own bounds, not open-coded numbers.
        unit.set_stretch_factor(StretchFactor::new(10.0));
        assert_eq!(unit.stretch_factor().get(), StretchFactor::MAX.get());
        unit.set_stretch_factor(StretchFactor::new(0.1));
        assert_eq!(unit.stretch_factor().get(), StretchFactor::MIN.get());

        unit.set_pitch_cents(Cents::new(5000.0));
        assert_eq!(unit.pitch_cents().get(), MAX_PITCH_CENTS);
        unit.set_pitch_cents(Cents::new(-5000.0));
        assert_eq!(unit.pitch_cents().get(), MIN_PITCH_CENTS);
    }

    #[test]
    fn enabled_flag_gates_processing() {
        let mut unit = Unit::new(44100.0);
        unit.set_stretch_factor(StretchFactor::new(2.0));
        assert!(unit.is_processing());

        unit.set_enabled(false);
        assert!(!unit.is_processing());
        unit.set_enabled(true);
        assert!(unit.is_processing());
    }

    #[test]
    fn unity_parameters_bypass() {
        let mut unit = Unit::new(44100.0);
        assert!(!unit.is_processing());

        let mut output = [0.0f32; 2];
        unit.tick(&[0.5, 0.25], &mut output);
        assert_eq!(output, [0.5, 0.25]);
    }

    /// Bypass must pass every channel through untouched, not just the front
    /// pair.
    #[test]
    fn six_channel_bypass_passes_all_channels_through() {
        let mut u = Unit::with_channels(44_100.0, 6);
        assert!(!u.is_processing(), "unity stretch/pitch should bypass");

        let input: Vec<f32> = (0..6).map(|c| (c + 1) as f32).collect();
        let mut output = [0.0f32; 6];
        u.tick(&input, &mut output);

        for (c, &got) in output.iter().enumerate() {
            assert_eq!(got, (c + 1) as f32, "channel {c}: {output:?}");
        }
    }

    /// A clone carries the parameters and the width, and starts with clean
    /// phase state. Clones happen per graph commit and per voice slot.
    #[test]
    fn clone_carries_parameters_and_width() {
        let u = Unit::with_channels(44_100.0, 6);
        u.set_stretch_factor(StretchFactor::new(1.5));

        let c = u.clone();
        assert_eq!(c.channels(), 6);
        assert!((c.stretch_factor().get() - 1.5).abs() < 0.001);

        // The atomics are independent after the clone.
        u.set_stretch_factor(StretchFactor::new(2.0));
        assert!((c.stretch_factor().get() - 1.5).abs() < 0.001);
    }

    /// `route` must agree with `outputs()`. If it does not, fundsp mis-plans
    /// this node's latency — which corrupts PDC without crashing or obviously
    /// mis-routing audio, so nothing else in the suite would notice.
    #[test]
    fn route_width_tracks_outputs_at_every_width() {
        for w in [1usize, 2, 6, 8] {
            let mut u = Unit::with_channels(44_100.0, w);
            let out = u.route(&SignalFrame::new(w), 44_100.0);
            assert_eq!(out.len(), u.outputs(), "at channels={w}");
        }
    }

    #[test]
    fn reset_clears_buffered_audio() {
        let mut u = Unit::with_channels(44_100.0, 1);
        u.set_stretch_factor(StretchFactor::new(2.0));

        let input = sine(440.0, 44_100.0, 8192);
        let mut out = [0.0f32; 1];
        for &s in &input {
            u.tick(&[s], &mut out);
        }
        u.reset();

        assert_eq!(u.channels.channels.borrow()[0].input.available(), 0);
        assert_eq!(u.channels.channels.borrow()[0].output.available(), 0);
    }

    /// Stale audio survives a source discontinuity until something calls
    /// [`Unit::reset`] — and `reset` is sufficient to clear it.
    ///
    /// This is a **characterization** test: it passes today and documents the
    /// mechanism behind the seek bug rather than gating it. The gate lives one
    /// level up, where a transport can actually seek
    /// (`voice_pool::tests::a_transport_seek_flushes_stretch_state`).
    ///
    /// What it pins is the two halves of the fix:
    ///
    /// - the leak is **large** — the FIFOs hold up to `window * 4` samples and
    ///   the phase accumulators keep resynthesising from them, so the output
    ///   after the input goes silent is at signal level, not at noise level;
    /// - `reset()` clears it **exactly**, to zero, not merely to something
    ///   small.
    ///
    /// The second half is why this test earns its place: if a later change makes
    /// `Vocoder::reset` cheaper by clearing less, the fix built on top of it
    /// stops working and this fails here rather than in an ear.
    #[test]
    fn stale_audio_survives_a_discontinuity_until_reset() {
        let fill = |u: &mut Unit, level: f32, n: usize| {
            let mut out = [0.0f32; 1];
            let mut peak = 0.0f32;
            for _ in 0..n {
                u.tick(&[level], &mut out);
                peak = peak.max(out[0].abs());
            }
            peak
        };

        // Prime with DC so "is the output still carrying the old material" is a
        // question about level alone — no phase or frequency argument needed.
        let mut leaking = Unit::with_channels(44_100.0, 1);
        leaking.set_stretch_factor(StretchFactor::new(2.0));
        assert!(leaking.is_processing());
        fill(&mut leaking, 0.5, 8192);

        // The discontinuity: the source goes silent. A seek into a silent region
        // looks exactly like this from the filter's side.
        let leaked = fill(&mut leaking, 0.0, 4096);
        assert!(
            leaked > 0.25,
            "expected the pre-discontinuity signal to keep draining; peak {leaked}"
        );

        // Same run, with the flush the fix will perform.
        let mut flushed = Unit::with_channels(44_100.0, 1);
        flushed.set_stretch_factor(StretchFactor::new(2.0));
        fill(&mut flushed, 0.5, 8192);
        flushed.reset();

        let after_reset = fill(&mut flushed, 0.0, 4096);
        assert_eq!(
            after_reset, 0.0,
            "reset must clear the FIFOs and phase state exactly, not approximately"
        );
    }

    /// The gain invariant: at a synthesis hop equal to the analysis hop and
    /// unity pitch, the vocoder reconstructs its input.
    ///
    /// Driven at the [`Vocoder`] rather than through [`Unit`], deliberately.
    /// `Unit` bypasses at exactly unity, so the public API cannot express "run
    /// the FFT path at identity settings" — and nudging the stretch factor off
    /// unity to defeat the bypass makes synthesis_hop 257 against an analysis
    /// hop of 256, which resamples the output and drifts it against the input.
    /// That is a real property, but it is not this one.
    ///
    /// Pins two bugs the pre-existing tests could not see, because every one of
    /// them asserted only that output was non-zero:
    ///
    /// - the spurious `1 / size` in synthesis, which attenuated by 60 dB;
    /// - the missing [`COLA_GAIN`], which leaves the sum of Hann² frames 3.5 dB
    ///   hot.
    #[test]
    fn vocoder_reconstructs_its_input_at_unity() {
        let sample_rate = 44_100.0;
        let fft = FftSize::N1024;
        let size = fft.size().get();
        let hop = fft.hop().get();
        let mut v = Vocoder::new(Unit::geometry(sample_rate, fft));

        // Two tones plus a DC offset, so a corrupted DC or Nyquist bin cannot
        // hide behind the tones.
        let len = size * 8;
        let input: Vec<f32> = (0..len)
            .map(|i| {
                let t = i as f32 / sample_rate;
                0.4 * (Radians::TAU.get() * 440.0 * t).sin()
                    + 0.2 * (Radians::TAU.get() * 3000.0 * t).sin()
                    + 0.1
            })
            .collect();

        let mut out = vec![0.0f32; len];
        let mut written = 0usize;
        for chunk in input.chunks(hop) {
            v.input.push(chunk);
            // Synthesis hop == analysis hop: no time scaling, so output and
            // input advance together.
            v.process(hop, hop, 1.0);
            written += v.output.drain(&mut out[written..]);
        }

        // The drained stream is aligned with the input, NOT delayed by a window:
        // the first hop of output only becomes available once a whole window has
        // arrived, so the fill-up is absorbed by `available` rather than showing
        // up as a lag. (`Unit::latency_samples` reports one window because that
        // is the delay a *graph* sees before the first sample appears — a
        // different question from where the samples land once they do.)
        //
        // The opening hops still ramp in as the overlap-add sum reaches steady
        // state, so compare the interior.
        let start = size;
        let end = written;
        assert!(end > start, "not enough output: {written} samples");

        let (mut num, mut den) = (0.0f64, 0.0f64);
        for i in start..end {
            let want = input[i] as f64;
            let got = out[i] as f64;
            num += (got - want) * (got - want);
            den += want * want;
        }
        let error = (num / den).sqrt();
        assert!(
            error < 0.02,
            "vocoder should reconstruct at unity; relative error {error:.4}"
        );
    }

    /// With stretching active, every channel must reach the output — a
    /// 6-channel voice through a stretcher that only ran two vocoders would
    /// silently lose four channels, and no stereo test can see that.
    #[test]
    fn six_channel_stretch_reaches_every_channel() {
        let mut u = Unit::with_channels(44_100.0, 6);
        u.set_stretch_factor(StretchFactor::new(2.0));
        assert!(u.is_processing());

        let mut seen = [false; 6];
        let mut output = [0.0f32; 6];
        for n in 0..8192 {
            // Distinct per-channel tone so a cross-channel leak is not mistaken
            // for a correct read.
            let input: Vec<f32> = (0..6)
                .map(|c| ((n as f32) * 0.01 * (c + 1) as f32).sin())
                .collect();
            u.tick(&input, &mut output);
            for (c, &s) in output.iter().enumerate() {
                if s.abs() > 1e-6 {
                    seen[c] = true;
                }
            }
            if seen.iter().all(|&b| b) {
                break;
            }
        }
        assert!(
            seen.iter().all(|&b| b),
            "channels {:?} never produced output",
            seen.iter()
                .enumerate()
                .filter(|(_, &b)| !b)
                .map(|(c, _)| c)
                .collect::<Vec<_>>()
        );
    }

    /// Stretching changes how fast the unit walks its SOURCE, not how many output
    /// samples it emits.
    ///
    /// The distinction is the whole shape of this unit. It emits exactly one
    /// sample per `tick`, because that is `AudioUnit`'s contract; the time-scaling
    /// shows up as the source being consumed at `1 / stretch`. So over a fixed
    /// number of ticks a 2x stretch consumes half the source a 1x pass does, and a
    /// 0.5x stretch consumes twice as much.
    ///
    /// This replaces a test that asserted "2x queues up MORE output than 0.5x".
    /// That was true, but only because the surplus was piling into the output ring
    /// with nowhere to go — it measured the overrun bug rather than the feature.
    /// With the intake paced, both factors emit one sample per tick and the ring
    /// stays bounded, so that assertion is now false and the property it meant to
    /// check lives on the input side.
    #[test]
    fn stretch_factor_changes_the_source_consumption_rate() {
        const TICKS: usize = 16_384;

        let consumed = |factor: f32| {
            let mut u = Unit::with_fft_size_and_channels(44_100.0, FftSize::N1024, 1);
            u.set_stretch_factor(StretchFactor::new(factor));
            let input = sine(440.0, 44_100.0, TICKS);
            let mut frame = [0.0f32; 1];
            let mut emitted = 0usize;
            for &s in &input {
                u.tick(&[s], &mut frame);
                emitted += 1;
            }
            // Everything the FIFO has seen: what it still holds plus what the
            // frames have retired.
            let seen = u.channels.channels.borrow()[0].input.0.write;
            (seen, emitted)
        };

        let (fast_seen, fast_emitted) = consumed(2.0);
        let (slow_seen, slow_emitted) = consumed(0.5);

        // Output is one-per-tick regardless — that is the contract.
        assert_eq!(fast_emitted, TICKS);
        assert_eq!(slow_emitted, TICKS);

        // 2x walks the source at half rate, 0.5x at double.
        let ratio = slow_seen as f64 / fast_seen as f64;
        assert!(
            (ratio - 4.0).abs() < 0.05,
            "0.5x should consume 4x the source 2.0x does; \
             saw {slow_seen} vs {fast_seen} (ratio {ratio:.3})"
        );
    }
}
