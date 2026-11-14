use super::*;

// ── Dimensionless ratios ────────────────────────────────────────────────────
//
// Three types. They share only the property of being bare numbers; their
// ranges do not overlap and one cannot be substituted for another:
//
//   CompressionRatio  [1, inf)  1:1 is no compression; below 1 would EXPAND
//   Q                 (0, inf)  filter sharpness; 0.707 is flat, high is a
//                               narrow peak
//   Resonance         [0, 1]    ladder feedback; 1.0 self-oscillates
//
// `Q` and `Resonance` are the sharp case: both describe "how resonant", but a
// ladder at 1.0 self-oscillates while a Q of 1.0 is a mild bell. Passing one
// where the other belongs is silent and sounds like a mistuned filter.

unit! {
    /// Compressor ratio: input dB over threshold per 1 dB of output.
    ///
    /// `1.0` is no compression. Below 1.0 would be an *expander*, which this
    /// type deliberately does not represent — a compressor clamps
    /// up to 1.0 rather than silently inverting its own behaviour.
    pub struct CompressionRatio(pub f32): Unit, Ordered, Bounded;
}
// NOT `Additive`: 4:1 plus 4:1 is not 8:1.
// NOT `Scalable`: doubling a ratio is not doubling anything audible —
// the mapping from ratio to gain reduction is logarithmic.

impl CompressionRatio {
    /// No compression.
    pub const UNITY: CompressionRatio = CompressionRatio(1.0);

    /// Constrain to a real compression ratio, at least 1. NaN stays NaN.
    #[inline]
    #[must_use]
    pub fn new_clamped(v: f32) -> CompressionRatio {
        CompressionRatio(if v < 1.0 { 1.0 } else { v })
    }
}

unit! {
    /// Filter quality factor — how sharply a filter resonates at its cutoff.
    ///
    /// `0.707` (1/sqrt2) is the maximally-flat Butterworth response; higher is
    /// a narrower, more peaked resonance. Must stay above zero: `Q` divides
    /// into the filter's damping term.
    pub struct Q(pub f32): Unit, Ordered, Bounded, Scalable;
}
// NOT `Additive`: two Q values do not sum into a third.

impl Q {
    /// Butterworth — the maximally flat response, `1/sqrt(2)`.
    pub const BUTTERWORTH: Q = Q(core::f32::consts::FRAC_1_SQRT_2);

    /// Constrain above zero, since `Q` divides into the damping term. NaN
    /// stays NaN.
    #[inline]
    #[must_use]
    pub fn new_clamped(v: f32) -> Q {
        Q(if v < f32::MIN_POSITIVE {
            f32::MIN_POSITIVE
        } else {
            v
        })
    }
}

unit! {
    /// Ladder-filter resonance, `0..1` — the normalized feedback around the
    /// filter's four poles.
    ///
    /// Distinct from [`Q`] even though both mean "how resonant": at `1.0` a
    /// ladder self-oscillates, whereas `Q(1.0)` is a mild bell. They are
    /// different parameterizations of different topologies, and swapping them
    /// is silent.
    pub struct Resonance(pub f32): Unit, Ordered, Bounded;
}
// NOT `Scalable` / `Additive`: as with `Feedback`, scaling walks
// the value toward self-oscillation with no check.

impl Resonance {
    /// No resonance.
    pub const NONE: Resonance = Resonance(0.0);
    /// The self-oscillation threshold.
    pub const SELF_OSCILLATION: Resonance = Resonance(1.0);

    /// Constrain into `0..=1`.
    #[inline]
    #[must_use]
    pub fn new_clamped(v: f32) -> Resonance {
        Resonance(v).clamp(Self::NONE, Self::SELF_OSCILLATION)
    }
}

unit! {
    /// Pitch offset in semitones. 12 semitones = 1 octave.
    pub struct Semitones(pub f32): Unit, Ordered, Bounded, Additive, Signed, Scalable;
}

unit! {
    /// Pitch offset in cents. 100 cents = 1 semitone.
    pub struct Cents(pub f32): Unit, Ordered, Bounded, Additive, Signed, Scalable;
}

impl Cents {
    /// One semitone.
    pub const SEMITONE: Cents = Cents(100.0);

    /// This offset in semitones.
    ///
    /// A real converter, not `self / 100.0`: `Cents` is `Scalable`, so
    /// `cents / 100.0` compiles and returns **`Cents`** —
    /// a value wrong by 100x whose *type* says it is fine. Sites that divide
    /// the raw `f32` today are correct by luck; this is the form that stays
    /// correct once they hold the typed value.
    #[inline]
    #[must_use]
    pub fn to_semitones(self) -> Semitones {
        Semitones(self.0 / 100.0)
    }

    /// This offset as a frequency multiplier. 1200 cents doubles the pitch.
    #[inline]
    #[must_use]
    pub fn to_pitch_ratio(self) -> f32 {
        2.0_f32.powf(self.0 / 1200.0)
    }

    /// The interval a frequency multiplier spans. Inverse of
    /// [`to_pitch_ratio`](Self::to_pitch_ratio).
    ///
    /// A named method rather than a `From`, because it is not total: a
    /// non-positive ratio has no logarithm. Those give [`Cents(0.0)`](Cents) —
    /// the unison a caller building a scale degree wants — rather than a NaN
    /// that reaches a frequency table before anyone notices.
    #[inline]
    #[must_use]
    pub fn from_pitch_ratio(ratio: f32) -> Cents {
        if ratio > 0.0 {
            Cents(1200.0 * ratio.log2())
        } else {
            Cents(0.0)
        }
    }
}

/// A pitch distance in cents, never below zero: how far a unison stack spreads
/// its sub-voices to either side of the written pitch.
///
/// Distinct from [`Cents`], which is a signed offset: a distance has no
/// direction, so a negative one is unwritable rather than clamped at each use.
///
/// **Range-checked**: [`new`](Self::new) refuses NaN, the infinities and
/// anything below zero, and so do `TryFrom<f32>` and serde. [`of`](Self::of)
/// is for a literal. [`clamped`](Self::clamped) is the one saturating
/// constructor, for a value that arrives from a lane or a knob;
/// [`Unit::from_raw`] is it too, since a cell holds any float.
///
/// ```
/// use tutti_types::CentsSpan;
///
/// assert_eq!(CentsSpan::new(12.0).map(CentsSpan::get), Some(12.0));
/// assert_eq!(CentsSpan::new(-1.0), None);
/// assert_eq!(CentsSpan::clamped(-1.0), CentsSpan::ZERO);
/// ```
#[repr(transparent)]
#[derive(Copy, Clone, Debug, PartialEq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "f32", into = "f32"))]
pub struct CentsSpan(f32);

// NOT `Additive` and NOT `Scalable`: a difference or a negative factor leaves
// the range. Do the arithmetic on the raw number and call `new` (or
// `clamped`, where saturating is meant).
impl CentsSpan {
    /// No distance.
    pub const ZERO: CentsSpan = CentsSpan(0.0);

    /// The distance, or `None` when `raw` is NaN, infinite or below zero.
    #[inline]
    #[must_use]
    pub const fn new(raw: f32) -> Option<CentsSpan> {
        if raw >= 0.0 && raw <= f32::MAX {
            Some(CentsSpan(raw))
        } else {
            None
        }
    }

    /// The distance for a literal or a constant. For a value that arrives from
    /// outside, use [`new`](Self::new).
    ///
    /// # Panics
    ///
    /// When `raw` is NaN, infinite or below zero; in a `const` that is a
    /// compile error.
    #[inline]
    #[must_use]
    #[track_caller]
    pub const fn of(raw: f32) -> CentsSpan {
        match Self::new(raw) {
            Some(span) => span,
            None => panic!("a cents span is finite and not below zero"),
        }
    }

    /// `raw` constrained into `0..=f32::MAX`; NaN is [`ZERO`](Self::ZERO).
    #[inline]
    #[must_use]
    pub const fn clamped(raw: f32) -> CentsSpan {
        if raw.is_nan() || raw < 0.0 {
            CentsSpan::ZERO
        } else if raw > f32::MAX {
            CentsSpan(f32::MAX)
        } else {
            CentsSpan(raw)
        }
    }

    /// Returns the raw distance, for a boundary the unit types do not cross.
    #[inline]
    #[must_use]
    pub const fn get(self) -> f32 {
        self.0
    }

    /// This distance as a signed offset upward.
    #[inline]
    #[must_use]
    pub const fn to_cents(self) -> Cents {
        Cents(self.0)
    }

    /// This distance in semitones.
    #[inline]
    #[must_use]
    pub fn to_semitones(self) -> Semitones {
        self.to_cents().to_semitones()
    }
}

impl PartialOrd for CentsSpan {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        self.0.partial_cmp(&other.0)
    }
}

impl core::fmt::Display for CentsSpan {
    #[inline]
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Display::fmt(&self.0, f)
    }
}

/// `from_raw` saturates ([`CentsSpan::clamped`]): a parameter cell holds any
/// float, and a unit's `from_raw` cannot refuse.
impl Unit for CentsSpan {
    type Raw = f32;
    #[inline]
    fn from_raw(v: f32) -> Self {
        Self::clamped(v)
    }
    #[inline]
    fn to_raw(self) -> f32 {
        self.0
    }
}

impl TryFrom<f32> for CentsSpan {
    type Error = CentsSpanOutOfRange;

    #[inline]
    fn try_from(raw: f32) -> Result<CentsSpan, CentsSpanOutOfRange> {
        CentsSpan::new(raw).ok_or(CentsSpanOutOfRange(raw))
    }
}

impl From<CentsSpan> for f32 {
    #[inline]
    fn from(span: CentsSpan) -> f32 {
        span.0
    }
}

/// A raw number that is not a cents span: NaN, infinite or below zero (see
/// [`CentsSpan::new`]).
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
#[error("a cents span is finite and not below zero, got {0}")]
pub struct CentsSpanOutOfRange(pub f32);

impl Semitones {
    /// One octave.
    pub const OCTAVE: Semitones = Semitones(12.0);

    /// This offset in cents. The inverse of [`Cents::to_semitones`], and
    /// likewise a converter rather than a scalar multiply.
    #[inline]
    #[must_use]
    pub fn to_cents(self) -> Cents {
        Cents(self.0 * 100.0)
    }

    /// This offset as a frequency multiplier. 12 semitones doubles the pitch.
    #[inline]
    #[must_use]
    pub fn to_pitch_ratio(self) -> f32 {
        2.0_f32.powf(self.0 / 12.0)
    }

    /// The interval a frequency multiplier spans. Inverse of
    /// [`to_pitch_ratio`](Self::to_pitch_ratio), and the semitone twin of
    /// [`Cents::from_pitch_ratio`].
    ///
    /// Named rather than a `From` for that method's reason: it is not total,
    /// because a non-positive ratio has no logarithm. Those give
    /// [`Semitones(0.0)`](Semitones) — a unison — rather than a NaN that
    /// reaches a note table before anyone notices.
    ///
    #[inline]
    #[must_use]
    pub fn from_pitch_ratio(ratio: f32) -> Semitones {
        if ratio > 0.0 {
            Semitones(12.0 * ratio.log2())
        } else {
            Semitones(0.0)
        }
    }
}

impl From<Cents> for Semitones {
    #[inline]
    fn from(c: Cents) -> Semitones {
        c.to_semitones()
    }
}

impl From<Semitones> for Cents {
    #[inline]
    fn from(s: Semitones) -> Cents {
        s.to_cents()
    }
}

#[cfg(test)]
mod tests;
