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
//! ```
//! use tutti_core::dsp::AudioUnit;
//! use tutti_core::{Cents, StretchFactor};
//! use tutti_sampler::stretch;
//!
//! // A pure filter: it owns no source, so the caller ticks its own and feeds
//! // each frame in.
//! let mut stretched = stretch::Unit::new(44_100.0);
//!
//! // The factor is clamped to `MIN..=MAX` at construction — an out-of-range
//! // request saturates rather than being rejected.
//! stretched.set_stretch_factor(StretchFactor::new_clamped(2.0)); // half speed
//! stretched.set_pitch_cents(Cents(1200.0));                      // up an octave
//!
//! // One frame in, one frame out; the vocoder's latency means early frames
//! // are the window filling rather than stretched audio.
//! let mut out = [0.0f32; 2];
//! stretched.tick(&[0.25, 0.25], &mut out);
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
//! Every buffer is allocated in [`Unit::with_fft_size_and_channels`], or — for
//! a unit produced by `clone`, which leaves its per-block scratch empty — in
//! `AudioUnit::allocate`, which the graph calls before running the node.
//! Neither `tick` nor `process` allocates or blocks, and neither does the
//! vocoder beneath them. Dropping a `Unit` is *not* RT-safe; see its `Drop`.

/// Per-channel RT scratch capacity, in samples.
///
/// **Deliberately not `tutti_core::dsp::MAX_BUFFER_SIZE`**, which is 64 (fundsp's
/// per-block cap). This is the *scratch* the vocoder pre-reserves so `process`
/// never reallocates, and it is sized for the FFT window rather than the block:
/// at 8192 it covers the largest `FftSize` with headroom. Importing the core
/// constant here instead would silently cut the reservation 128x — the tests
/// still pass, because a `Vec` that reallocates is correct, just not RT-safe.
const MAX_BUFFER_SIZE: usize = 8192;

use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

use tutti_core::{AudioThreadCell, AudioUnit, Ordering, RtScratch, SampleRate, Samples, Seconds};

/// The vocoder bank, shared by refcount across graph generations.
///
/// `Net::commit` clones every node per graph edit, and a deep copy of this is
/// ~96 KB per channel allocated with the previous generation's freed — 201.8 MB
/// per commit over 640 stereo nodes, against a 2 ms budget. Sharing makes the
/// commit clone a refcount bump and removes both halves at once. Full figures:
/// `examples/profile_stretch_clone.rs`.
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
    /// and they carry nothing between blocks. On the handle instead they cost a
    /// fresh 64 KB per channel per generation — **98% of a shared-bank commit's
    /// remaining traffic at both widths**. Deferring them to
    /// [`AudioUnit::allocate`] moves only *when* that is paid, not whether: the
    /// graph calls `allocate` on every generation.
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
/// - **A power of two between 2 and 32768**, because
///   [`real_fft`](tutti_core::real_fft) dispatches on exactly those lengths and
///   panics otherwise.
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

// The pieces, innermost first: `buffers` is plain sample plumbing, `vocoder` is
// the phase-vocoder DSP over it, and `unit` is the public filter that owns one
// vocoder per channel.
mod buffers;
mod unit;
mod vocoder;

use vocoder::Vocoder;

pub use unit::Unit;

#[cfg(test)]
mod tests;
