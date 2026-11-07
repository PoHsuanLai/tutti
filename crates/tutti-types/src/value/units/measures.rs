use super::*;

// ── Measurements ────────────────────────────────────────────────────────────
//
// Every other unit is a *control*: a value the caller sets on a processor.
// These three are *readings*: values the engine measures and reports back.
//
// That distinction is what separates them from the controls they resemble.
// `Correlation` and `Pan` share `Depth`'s range and most of its algebra, and
// `Confidence` shares `Mix`'s range — but a meter's phase coherence is not an
// LFO's modulation amount, and swapping them is silent. This is the same
// argument `Q` and `Resonance` settle by splitting: two quantities that
// describe different things stay different types even when their numbers line
// up.

unit! {
    /// How certain an estimator is of its own answer, `0..1`. 0 is a guess,
    /// 1 is certainty.
    ///
    /// Not a [`Mix`] despite the shared range — nothing is being blended, and
    /// `Mix::blend`, the reason that type exists, is meaningless here. Not an
    /// [`Amplitude`] either: a confidence is never multiplied onto a signal,
    /// and giving it a gain's algebra would license `sample * confidence`,
    /// silently turning an uncertain pitch estimate into a volume dip.
    pub struct Confidence(pub f32): Unit, Ordered, Bounded, Scalable;
}
// Scalable because the smoothers genuinely attenuate it: a Viterbi track
// penalizes a jump by multiplying the confidence by a decay factor.
// NOT `Additive`: two estimates being 0.6 confident does not make
// anything 1.2 confident. Combining evidence is `combine`, which states the
// independence precondition that multiplying them requires.

impl Confidence {
    /// No information — the estimator is guessing.
    pub const NONE: Confidence = Confidence(0.0);
    /// Certain.
    pub const CERTAIN: Confidence = Confidence(1.0);

    /// Constrain into `0..=1`.
    #[inline]
    #[must_use]
    pub fn new_clamped(v: f32) -> Confidence {
        Confidence(v).clamp(Self::NONE, Self::CERTAIN)
    }

    /// Two *independent* estimates of the same fact, combined as a joint
    /// probability.
    ///
    /// Named rather than `Mul` because the multiplication is only correct when
    /// the estimates are independent, and the name is where that precondition
    /// is stated.
    #[inline]
    #[must_use]
    pub fn combine(self, other: Confidence) -> Confidence {
        Confidence(self.0 * other.0)
    }
}

unit! {
    /// Phase coherence between two channels, `-1..1`. `+1` is identical
    /// (mono-compatible), `0` uncorrelated, `-1` polarity-inverted.
    ///
    /// Shares [`Depth`]'s range and sign convention but not its meaning: a
    /// modulation depth is a control you set, a correlation is a measurement
    /// reported back. Feeding a correlation meter into an LFO's depth is not a
    /// plausible operation, so the compiler should refuse it.
    ///
    /// The negative half is the whole point — `-1` is the mono-compatibility
    /// failure a correlation meter exists to catch, and it is not "less" than
    /// `+1` in any audible sense; it is the opposite fault.
    pub struct Correlation(pub f32): Unit, Ordered, Bounded, Signed;
}
// NOT `Scalable`: the value is a normalized inner product, so rescaling
// denormalizes it and the result no longer means "coherence".
// NOT `Additive`: two correlations do not sum. Averaging one over time
// is meter ballistics, which smooths rather than accumulates.

impl Correlation {
    /// Identical channels — fully mono-compatible.
    pub const MONO: Correlation = Correlation(1.0);
    /// Uncorrelated channels.
    pub const UNCORRELATED: Correlation = Correlation(0.0);
    /// Polarity-inverted — cancels completely when summed to mono.
    pub const INVERTED: Correlation = Correlation(-1.0);

    /// Constrain into `-1..=1`.
    #[inline]
    #[must_use]
    pub fn new_clamped(v: f32) -> Correlation {
        Correlation(v).clamp(Self::INVERTED, Self::MONO)
    }

    /// Whether summing to mono would audibly cancel.
    ///
    /// Named because the threshold is a convention, and a bare comparison at
    /// the call site hides that it is one.
    #[inline]
    #[must_use]
    pub fn has_phase_issues(self) -> bool {
        self.0 < -0.3
    }

    /// The mid/side width this coherence implies.
    ///
    /// A derivation, not a second field: storing both lets a smoother move one
    /// without the other and leave the pair contradicting itself.
    #[inline]
    #[must_use]
    pub fn to_stereo_width(self) -> StereoWidth {
        StereoWidth(1.0 - self.0)
    }
}

unit! {
    /// Position on the left/right axis, `-1..1`. `-1` is hard left, `0`
    /// center, `+1` hard right.
    ///
    /// A *position*, like [`Azimuth`] — but on a segment rather than a circle,
    /// so it saturates at the ends instead of wrapping, and unlike `Azimuth`
    /// it orders (left really is less than right).
    ///
    /// Distinct from [`Depth`] for the reason this section exists: a pan
    /// position and a modulation amount are different quantities that happen
    /// to share a range.
    pub struct Pan(pub f32): Unit, Ordered, Bounded, Signed;
}
// NOT `Scalable`: half of a pan position is not a pan position — the
// same argument `Mix` makes. Moving a source is `Azimuth::lerp_shortest`'s
// job on the circle, and interpolation here is a `lerp`, not a `*`.
// NOT `Additive`: two pan positions do not sum into a third.

impl Pan {
    /// Hard left.
    pub const LEFT: Pan = Pan(-1.0);
    /// Centered — equal in both channels.
    pub const CENTER: Pan = Pan(0.0);
    /// Hard right.
    pub const RIGHT: Pan = Pan(1.0);

    /// Constrain into `-1..=1`.
    #[inline]
    #[must_use]
    pub fn new_clamped(v: f32) -> Pan {
        Pan(v).clamp(Self::LEFT, Self::RIGHT)
    }
}

unit! {
    /// How hard a note was struck or released, `0..1`.
    ///
    /// Normalized rather than a 7-bit code because MIDI 2.0's field is 16-bit:
    /// storing `u8` would quantize to 128 steps permanently, and the widening
    /// back out is not a multiply. Note-on and note-off velocity are the same quantity, which is why one type serves
    /// both.
    ///
    /// **Not an [`Amplitude`]**, despite `0..1` and the temptation. A velocity
    /// is never multiplied onto a signal: it selects a sample layer, sets an
    /// envelope's peak, and opens a filter — a synth may be *louder* at low
    /// velocity if that is how it is programmed. Giving it a gain's algebra
    /// would license `sample * velocity`, silently turning a soft note into a
    /// volume dip. Same argument [`Confidence`] makes.
    ///
    /// **Not a [`Mix`]**: nothing is being blended, and `Mix::blend` — the
    /// reason that type exists — is meaningless here. **Not a [`Depth`]**: that
    /// is bipolar, and there is no such thing as negative velocity.
    pub struct Velocity(pub f32): Unit, Ordered, Bounded;
}
// NOT `Scalable`: `velocity * 0.5` reads as "half as loud", which is
// exactly the amplitude confusion this type exists to prevent — the mapping
// from velocity to loudness is the instrument's, not arithmetic's. Scaling a
// performance is `Velocity::scaled`, which says what it does.
// NOT `Additive`: two velocities do not sum into a third.

impl Velocity {
    /// Silence — a note-on at zero velocity is a note-off by MIDI convention.
    pub const SILENT: Velocity = Velocity(0.0);
    /// The hardest strike.
    pub const MAX: Velocity = Velocity(1.0);
    /// MIDI 2.0's center code (`0x8000`) — mezzo-forte, and the value a
    /// controller sending no velocity data should be treated as.
    pub const CENTER: Velocity = Velocity(0.5);

    /// Constrain into `0..=1`.
    #[inline]
    #[must_use]
    pub fn new_clamped(v: f32) -> Velocity {
        Velocity(v).clamp(Self::SILENT, Self::MAX)
    }

    /// Scale a whole performance's dynamics, staying in range.
    ///
    /// The named replacement for the omitted `Mul<f32>`: this is the one
    /// operation that is genuinely a rescale (a "soften the take" edit), and
    /// naming it keeps the *loudness* reading — which is the instrument's job,
    /// not arithmetic's — from being spelled the same way.
    #[inline]
    #[must_use]
    pub fn scaled(self, factor: f32) -> Velocity {
        Velocity::new_clamped(self.0 * factor)
    }
}

impl From<Correlation> for StereoWidth {
    #[inline]
    fn from(c: Correlation) -> StereoWidth {
        c.to_stereo_width()
    }
}

#[cfg(test)]
mod tests;
