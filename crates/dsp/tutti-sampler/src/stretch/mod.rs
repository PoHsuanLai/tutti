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
//! use tutti_core::AudioUnit;
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
//! Every buffer is allocated in [`Unit::with_fft_size_and_channels`], and a
//! clone allocates its own (see "Owned, not shared" on [`Unit`]). Neither
//! `tick`, `process` nor the slot's block read allocates or blocks, and
//! neither does the vocoder beneath them. Dropping a `Unit` frees its
//! vocoders, so it is *not* RT-safe: the pool retires removed filters to the
//! control thread (`VoicePool::retired`).

/// Per-channel RT scratch capacity, in samples.
///
/// **Deliberately not `tutti_core::MAX_BUFFER_SIZE`**, which is 64 (fundsp's
/// per-block cap). This is the *scratch* the vocoder pre-reserves so `process`
/// never reallocates, and it is sized for the FFT window rather than the block:
/// at 8192 it covers the largest `FftSize` with headroom. Importing the core
/// constant here instead would silently cut the reservation 128x — the tests
/// still pass, because a `Vec` that reallocates is correct, just not RT-safe.
const MAX_BUFFER_SIZE: usize = 8192;

use tutti_core::{SampleRate, Samples, Seconds};

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
/// - **A power of two between 4 and 32768**, because `real_fft` (the private
///   `fft` module) dispatches on exactly those lengths and panics otherwise.
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
mod fft;
mod unit;
mod vocoder;

pub use unit::Unit;

#[cfg(test)]
mod tests;
