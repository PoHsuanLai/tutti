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
