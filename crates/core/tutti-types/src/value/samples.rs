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

/// A count of audio frames: reported latency, compensation delay, ring length.
///
/// Integer by construction. Distinct from `SamplePosition`, which is an `f64`
/// *position* within a wave that may be fractional so interpolating readers can
/// address between two integer sample indices.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
// `transparent` for the same reason the float units have it: a `Samples` is
// `512` on the wire, not `[512]`. Load-bearing for the latency path — the
// plugin-metadata `latency_samples` field migrates from a bare `usize` to this
// type without moving a byte, so host and subprocess upgrade independently.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
pub struct Samples(pub usize);

impl From<usize> for Samples {
    #[inline]
    fn from(v: usize) -> Self {
        Self(v)
    }
}

impl From<Samples> for usize {
    #[inline]
    fn from(v: Samples) -> usize {
        v.0
    }
}

impl core::fmt::Display for Samples {
    #[inline]
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Display::fmt(&self.0, f)
    }
}

impl Samples {
    /// No frames at all.
    pub const ZERO: Samples = Samples(0);

    #[inline]
    pub const fn new(v: usize) -> Self {
        Self(v)
    }

    #[inline]
    pub const fn get(self) -> usize {
        self.0
    }

    /// The delay needed so a signal arriving at `self` lines up with one
    /// arriving at `target`.
    ///
    /// Saturates: a signal already later than `target` needs no delay, and
    /// never a negative one.
    #[inline]
    pub const fn align_to(self, target: Samples) -> Samples {
        Samples(target.0.saturating_sub(self.0))
    }

    /// Frames left after consuming `taken`.
    ///
    /// The mirror of [`align_to`](Self::align_to) — same subtraction, opposite
    /// argument order and opposite intent, which is why both are named rather
    /// than left to a bare `-`. Saturates at zero: a reader that consumed more
    /// than the buffer held has a bug upstream, and wrapping to ~1.8e19 would
    /// turn that bug into an out-of-bounds read.
    #[inline]
    pub const fn remaining_after(self, taken: Samples) -> Samples {
        Samples(self.0.saturating_sub(taken.0))
    }

    /// `self - rhs` when the result is representable, `None` when `rhs` is
    /// larger.
    ///
    /// For callers that must *branch* on underflow rather than absorb it — the
    /// difference between "clamp to zero and carry on" and "this ordering was
    /// supposed to be impossible".
    #[inline]
    pub const fn checked_sub(self, rhs: Samples) -> Option<Samples> {
        match self.0.checked_sub(rhs.0) {
            Some(v) => Some(Samples(v)),
            None => None,
        }
    }

    #[inline]
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    /// The span these frames occupy at `rate`. Inverse of
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
    /// Divided in `f64` before narrowing, because a frame count leaves `f32`'s
    /// exact-integer range (2^24) after about six minutes at 48 kHz.
    ///
    /// That buys less than the forward direction's `f64`, and the difference is
    /// worth stating: the *result* here is `f32` `Seconds`, so past a few
    /// minutes the return type is coarser than the error the division avoids —
    /// at an hour, an `f32` second cannot resolve 20 frames and both orderings
    /// give the same answer. Dividing first is still right, and free, but a
    /// caller who needs a long span to the frame wants `f64` end to end, per
    /// the carve-out on [`Seconds`](super::units::Seconds) itself.
    #[inline]
    pub fn to_seconds(self, rate: impl Into<super::units::SampleRate>) -> super::units::Seconds {
        let rate = rate.into().get();
        if rate > 0.0 {
            super::units::Seconds((self.0 as f64 / rate) as f32)
        } else {
            super::units::Seconds(0.0)
        }
    }
}

// ── Operators ───────────────────────────────────────────────────────────────
//
// Written out rather than generated: the `unit_*` macros in `super::units` are
// float-shaped (`unit_bounded!` calls `<$raw>::min`, `unit_scalable!` divides by
// a float), and `Samples` is integer-backed. `meter.rs` hand-writes its
// `BarNumber`/`BarCount` algebra for the same reason.
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

/// Splitting a span into `k` equal chunks. Truncates, like integer division.
impl core::ops::Div<usize> for Samples {
    type Output = Self;
    #[inline]
    fn div(self, k: usize) -> Self {
        Samples(self.0 / k)
    }
}

/// How many of `rhs` fit in `self` — a bare number, not a count. This is the
/// STFT overlap factor (`window / hop`), which is dimensionless by construction.
impl core::ops::Div for Samples {
    type Output = usize;
    #[inline]
    fn div(self, rhs: Self) -> usize {
        self.0 / rhs.0
    }
}

/// Position within a repeating span — ring-buffer wraparound.
///
/// Safe here in a way `unit_modular!` is not for the float units: unsigned
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

    /// Dividing in `f64` changes the answer, but only in a narrow band.
    ///
    /// The count must exceed `f32`'s exact-integer range (2^24) for narrowing
    /// *first* to lose anything — and not by so much that the `f32` result is
    /// coarser than the error. `2^24 + 1` frames is inside that band; one hour
    /// is not, because at 3600 s an `f32` second only resolves ~20 frames and
    /// both paths round to the same value. So this pins a real property with a
    /// genuinely small reach, rather than the "long renders need it" claim the
    /// forward direction can make and this one cannot.
    #[test]
    fn dividing_before_narrowing_keeps_a_frame_f32_would_lose() {
        use super::super::units::SampleRate;
        let rate = SampleRate(48_000.0);
        let frames = Samples((1 << 24) + 1);
        let exact = frames.to_seconds(rate).get();
        let narrowed = (frames.get() as f32) / 48_000.0;
        assert_ne!(
            exact.to_bits(),
            narrowed.to_bits(),
            "dividing in f32 must lose the odd frame this keeps"
        );

        // And the honest limit of that: an hour is past where `f32` `Seconds`
        // can tell the two apart at all.
        let hour = Samples(48_000 * 3600 + 1);
        assert_eq!(
            hour.to_seconds(rate).get().to_bits(),
            ((hour.get() as f32) / 48_000.0).to_bits()
        );
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
