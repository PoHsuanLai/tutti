//! [`Samples`] — a count of audio frames.
//!
//! A discrete count, distinct from the float-backed *measure* units (`Hz`,
//! `Seconds`, `Beat`, `SamplePosition`, …) in [`super::units`]. Those are
//! continuous quantities you interpolate and feed to [`Param`](super::Param);
//! this is an integer you compare and add.
//!
//! Deliberately **not** implementing the [`Unit`](super::Unit) marker trait — a
//! compensation delay is not an automatable DSP parameter and must not be
//! reachable through [`Param`](super::Param). The practical difference shows up
//! in the derives: `Eq + Ord + Hash`, which counts need for map keys and
//! `max()`, and which the float units cannot have.

// Serde `transparent`, which `unit!` gives every quantity: a `Samples` is `512`
// on the wire, not `[512]`. Load-bearing for the latency path: the
// plugin-metadata `latency_samples` field has the same wire form as a bare
// `usize`, so a host and a plugin subprocess built at different versions still
// agree.
unit! {
    /// A count of audio frames: reported latency, compensation delay, ring length,
    /// and every count crossing the [`AudioIn`](crate::io::AudioIn) /
    /// [`AudioOut`](crate::io::AudioOut) edge.
    ///
    /// "Samples" in the *sample-frame* sense — one tick of the clock, however many
    /// channels wide — which is how the plugin formats and `Seconds::to_samples`
    /// already use it. It is **never** a count of interleaved `f32`s; that quantity
    /// is a slice length, reached only through
    /// [`interleaved_len`](Self::interleaved_len), and brought back only through
    /// [`from_interleaved_len`](Self::from_interleaved_len).
    ///
    /// Integer by construction. Distinct from `SamplePosition`, which is an `f64`
    /// *position* within a wave that may be fractional so interpolating readers can
    /// address between two integer sample indices.
    pub struct Samples(pub usize): From, Display;
}

impl Samples {
    /// No frames at all.
    pub const ZERO: Samples = Samples(0);

    /// Wraps a raw count already denominated in **frames**.
    #[inline]
    #[must_use]
    pub const fn new(v: usize) -> Self {
        Self(v)
    }

    /// Returns the raw **frame** count. Multiply by the channel width to reach
    /// a sample index into an interleaved buffer.
    #[inline]
    #[must_use]
    pub const fn get(self) -> usize {
        self.0
    }

    /// Returns the delay needed so a signal arriving at `self` lines up with
    /// one arriving at `target`.
    ///
    /// Saturates: a signal already later than `target` needs no delay, and
    /// never a negative one.
    #[inline]
    #[must_use]
    pub const fn align_to(self, target: Samples) -> Samples {
        Samples(target.0.saturating_sub(self.0))
    }

    /// Returns the frames left after consuming `taken`.
    ///
    /// The mirror of [`align_to`](Self::align_to) — same subtraction, opposite
    /// argument order and opposite intent, which is why both are named rather
    /// than left to a bare `-`. Saturates at zero: a reader that consumed more
    /// than the buffer held has a bug upstream, and wrapping to ~1.8e19 would
    /// turn that bug into an out-of-bounds read.
    #[inline]
    #[must_use]
    pub const fn remaining_after(self, taken: Samples) -> Samples {
        Samples(self.0.saturating_sub(taken.0))
    }

    /// Returns `self - rhs`, or `None` when `rhs` is larger.
    ///
    /// For callers that must *branch* on underflow rather than absorb it — the
    /// difference between "clamp to zero and carry on" and "this ordering was
    /// supposed to be impossible".
    #[inline]
    #[must_use]
    pub const fn checked_sub(self, rhs: Samples) -> Option<Samples> {
        match self.0.checked_sub(rhs.0) {
            Some(v) => Some(Samples(v)),
            None => None,
        }
    }

    /// Returns whether this is an empty span (no frames at all).
    #[inline]
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    /// Returns how many **samples** these frames occupy in a flat interleaved
    /// buffer `layout` wide: `frames × channels`.
    ///
    /// The one sanctioned way from a frame count to a slice length.
    ///
    /// Returns a bare `usize` on purpose: the result indexes a `&[S]`, and a
    /// slice has no other unit. It is the *only* place on the
    /// [`AudioIn`](crate::io::AudioIn) / [`AudioOut`](crate::io::AudioOut) edge
    /// where a count leaves this type, so a stride mistake has one site to live
    /// in rather than one per caller.
    ///
    /// Saturating, like the `Mul` impl: an overflowing length can only come
    /// from a corrupt count, and slicing with `usize::MAX` panics at the slice
    /// rather than wrapping to a short, plausible-looking window.
    #[inline]
    #[must_use]
    pub const fn interleaved_len(self, layout: crate::ChannelLayout) -> usize {
        self.0.saturating_mul(layout.count() as usize)
    }

    /// Returns how many **whole frames** a flat interleaved buffer of `len`
    /// samples holds at `layout`'s width: `len / channels`, the inverse of
    /// [`interleaved_len`](Self::interleaved_len).
    ///
    /// A trailing partial frame is **not** counted, matching
    /// [`Interleaved::len`](crate::Interleaved::len): a short frame is not a
    /// frame, and writing one desynchronises the interleave for everything
    /// after it.
    ///
    /// A zero-width layout holds no frames, so
    /// [`EMPTY`](crate::ChannelLayout::EMPTY) gives [`ZERO`](Self::ZERO) rather
    /// than a divide-by-zero panic. That is the answer a pump over an empty bus
    /// wants: nothing to move.
    #[inline]
    #[must_use]
    pub const fn from_interleaved_len(len: usize, layout: crate::ChannelLayout) -> Samples {
        match layout.count() {
            0 => Samples::ZERO,
            ch => Samples(len / ch as usize),
        }
    }

    /// Returns the span these frames occupy at `rate`; the inverse of
    /// [`Seconds::to_samples`](super::units::Seconds::to_samples).
    ///
    /// No rounding variants, unlike the forward direction: that one lands on a
    /// discrete count, so it has to say whether it is allocating, measuring or
    /// counting. This lands on a continuous quantity, where there is one answer.
    ///
    /// A non-positive rate gives `Seconds(0.0)` rather than an infinity — the
    /// same choice [`BeatDuration::to_seconds`](super::units::BeatDuration::to_seconds)
    /// makes for a non-positive tempo, and for the same reason: the degenerate
    /// value reaches a readout or a scheduler, where zero is recoverable and
    /// `inf` is not.
    ///
    /// Divided in `f64`: a frame count leaves `f32`'s exact-integer range (2^24)
    /// after about six minutes at 48 kHz.
    ///
    /// The result is an `f64` [`Seconds`](super::units::Seconds), so a long
    /// span keeps its frame: an hour at 48 kHz is resolved to well under a
    /// nanosecond.
    #[inline]
    pub fn to_seconds(self, rate: impl Into<super::units::SampleRate>) -> super::units::Seconds {
        let rate = rate.into().get();
        if rate > 0.0 {
            super::units::Seconds(self.0 as f64 / rate)
        } else {
            super::units::Seconds(0.0)
        }
    }
}

// ── Operators ───────────────────────────────────────────────────────────────
//
// Written out rather than named as `unit!` abilities: a count adds, scales and
// divides by saturating or truncating integer rules, which the float
// abilities do not state.
//
// Deliberately absent: `Sub`. See the ledger in the tests below.

/// Frame counts sum. Saturating: the only way to overflow a `usize` count is a
/// corrupt input, and this runs during graph rebuild — clamping beats a panic
/// next to the audio thread.
impl core::ops::Add for Samples {
    type Output = Self;
    #[inline]
    fn add(self, rhs: Self) -> Self {
        Samples(self.0.saturating_add(rhs.0))
    }
}

impl core::ops::AddAssign for Samples {
    #[inline]
    fn add_assign(&mut self, rhs: Self) {
        self.0 = self.0.saturating_add(rhs.0);
    }
}

/// A count scaled by a bare integer is a count — mip levels, channel strides.
impl core::ops::Mul<usize> for Samples {
    type Output = Self;
    #[inline]
    fn mul(self, k: usize) -> Self {
        Samples(self.0.saturating_mul(k))
    }
}

/// Splitting a span into `k` equal chunks. Truncates, like integer division,
/// and panics when `k` is zero.
impl core::ops::Div<usize> for Samples {
    type Output = Self;
    #[inline]
    fn div(self, k: usize) -> Self {
        Samples(self.0 / k)
    }
}

/// How many of `rhs` fit in `self` — a bare number, not a count. This is the
/// STFT overlap factor (`window / hop`), which is dimensionless by construction.
/// Panics when `rhs` is zero.
impl core::ops::Div for Samples {
    type Output = usize;
    #[inline]
    fn div(self, rhs: Self) -> usize {
        self.0 / rhs.0
    }
}

/// Position within a repeating span — ring-buffer wraparound. Panics when
/// `rhs` is zero.
///
/// Safe here in a way `Modular` is not for the float units: unsigned
/// integer `%` has no sign to preserve, so there is no negative-input trap.
impl core::ops::Rem for Samples {
    type Output = Self;
    #[inline]
    fn rem(self, rhs: Self) -> Self {
        Samples(self.0 % rhs.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn align_to_yields_the_gap() {
        assert_eq!(Samples(100).align_to(Samples(512)), Samples(412));
        assert_eq!(Samples(512).align_to(Samples(512)), Samples(0));
    }

    #[test]
    fn align_to_saturates_when_already_late() {
        // A signal arriving later than the target needs no delay.
        assert_eq!(Samples(900).align_to(Samples(512)), Samples(0));
    }

    #[test]
    fn frames_convert_to_a_span_and_back() {
        use super::super::units::{SampleRate, Seconds};
        let rate = SampleRate(48_000.0);
        assert_eq!(Samples(48_000).to_seconds(rate), Seconds(1.0));
        assert_eq!(Samples(0).to_seconds(rate), Seconds(0.0));
        // Round-trips through the forward direction it is the inverse of.
        assert_eq!(Seconds(2.5).to_samples(rate), Samples(120_000));
        assert_eq!(Samples(120_000).to_seconds(rate), Seconds(2.5));
    }

    /// A degenerate rate yields no span rather than an infinity — the value
    /// reaches a readout or a scheduler, and zero is the recoverable one.
    #[test]
    fn a_non_positive_rate_is_no_span() {
        use super::super::units::{SampleRate, Seconds};
        assert_eq!(Samples(48_000).to_seconds(SampleRate(0.0)), Seconds(0.0));
        assert_eq!(Samples(48_000).to_seconds(SampleRate(-1.0)), Seconds(0.0));
    }

    /// A long frame count keeps its frame through `Seconds` and back.
    ///
    /// `2^24 + 1` frames is past `f32`'s exact-integer range, and an hour
    /// plus one frame is past where an `f32` second could tell neighbouring
    /// frames apart; both come back as the same count.
    ///
    /// Mutation (run): `to_seconds` dividing in `f32` (`(self.0 as f32 / rate as
    /// f32) as f64`) -> the `2^24 + 1` count comes back off by a frame -> fails.
    #[test]
    fn a_long_frame_count_round_trips_through_seconds() {
        use super::super::units::SampleRate;
        let rate = SampleRate(48_000.0);
        for frames in [Samples((1 << 24) + 1), Samples(48_000 * 3600 + 1)] {
            assert_eq!(frames.to_seconds(rate).to_samples(rate), frames);
        }
    }

    /// What is deliberately *absent* is as load-bearing as what is present.
    /// These must not compile; the ledger is here so a future contributor does
    /// not "helpfully" add them.
    ///
    /// - `Samples - Samples` — an unsigned count has no safe `-`. A plain one
    ///   wraps to ~1.8e19 in release (a buffer size that reads off the end of
    ///   the world); a saturating one silently returns zero and hides the
    ///   ordering bug that produced it. The three real uses are named instead:
    ///   [`align_to`](Samples::align_to), [`remaining_after`](Samples::remaining_after),
    ///   and [`checked_sub`](Samples::checked_sub).
    /// - `Samples * Samples` — frames squared is not a quantity.
    /// - `Samples + usize` — the bare operand could be a count, an index, or a
    ///   channel number; make the caller say `Samples(n)`.
    ///
    /// The body is empty and this test cannot fail. It exists as documentation:
    /// the omission ledger above is the contract, and a `#[test]` is what keeps
    /// it beside the operators rather than drifting into a README. Adding an
    /// omitted operator means deleting its entry here, not making this pass.
    #[test]
    fn omitted_operators_are_documented() {}

    #[test]
    fn add_saturates_rather_than_wrapping() {
        assert_eq!(Samples(480) + Samples(32), Samples(512));
        // The overflow path clamps instead of wrapping to a tiny count — a
        // wrapped length would size a buffer at near-zero and read past it.
        assert_eq!(Samples(usize::MAX) + Samples(1), Samples(usize::MAX));

        let mut acc = Samples(100);
        acc += Samples(28);
        assert_eq!(acc, Samples(128));
    }

    #[test]
    fn remaining_after_and_align_to_are_mirror_images() {
        let buffer = Samples(512);
        assert_eq!(buffer.remaining_after(Samples(100)), Samples(412));
        assert_eq!(Samples(100).align_to(buffer), Samples(412));

        // Both saturate rather than wrap, in their own direction.
        assert_eq!(buffer.remaining_after(Samples(900)), Samples::ZERO);
        assert_eq!(Samples(900).align_to(buffer), Samples::ZERO);
    }

    #[test]
    fn checked_sub_reports_underflow_instead_of_absorbing_it() {
        assert_eq!(Samples(512).checked_sub(Samples(100)), Some(Samples(412)));
        assert_eq!(Samples(100).checked_sub(Samples(512)), None);
    }

    #[test]
    fn ratio_counts_the_overlap_factor() {
        // window / hop — dimensionless, which is why it yields a bare number.
        assert_eq!(Samples(1024) / Samples(256), 4);
        assert_eq!(Samples(512) * 2, Samples(1024));
        assert_eq!(Samples(1024) / 2, Samples(512));
    }

    /// Frames ↔ interleaved length, checked at six channels, where confusing
    /// the two is a 6× error rather than a 2× one that can hide in a round
    /// number.
    ///
    /// Mutation: `interleaved_len` returning `self.0` (frames as the slice
    /// length) → the first assertion fails; `from_interleaved_len` returning
    /// `Samples(len)` (samples as the frame count) → the second fails.
    #[test]
    fn interleaved_len_is_frames_times_width() {
        use crate::ChannelLayout;
        let six = ChannelLayout::from(6u16);
        assert_eq!(Samples(10).interleaved_len(six), 60);
        assert_eq!(Samples::from_interleaved_len(60, six), Samples(10));

        // A trailing partial frame is not a frame.
        assert_eq!(Samples::from_interleaved_len(65, six), Samples(10));
        // Round trip from the frame side is exact.
        for n in [0usize, 1, 7, 512] {
            let f = Samples(n);
            assert_eq!(
                Samples::from_interleaved_len(f.interleaved_len(six), six),
                f
            );
        }
    }

    /// A zero-width buffer holds no frames — and says so rather than dividing
    /// by zero.
    ///
    /// Mutation: drop the `0 =>` arm → the division panics → this fails.
    #[test]
    fn an_empty_layout_holds_no_frames() {
        use crate::ChannelLayout;
        assert_eq!(
            Samples::from_interleaved_len(64, ChannelLayout::EMPTY),
            Samples::ZERO
        );
        assert_eq!(Samples(64).interleaved_len(ChannelLayout::EMPTY), 0);
    }

    #[test]
    fn rem_wraps_a_ring_position() {
        let capacity = Samples(512);
        assert_eq!(Samples(600) % capacity, Samples(88));
        assert_eq!(Samples(512) % capacity, Samples::ZERO);
    }

    #[test]
    fn counts_order_and_round_trip() {
        assert!(Samples(100) < Samples(512));
        assert_eq!(
            [Samples(3), Samples(9), Samples(1)].iter().max(),
            Some(&Samples(9))
        );
        assert_eq!(usize::from(Samples::new(2)), 2);
        assert_eq!(Samples::from(1).get(), 1);
    }
}
