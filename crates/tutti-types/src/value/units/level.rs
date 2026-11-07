// Gain: the logarithmic and the linear spelling of one quantity.

use super::*;

unit! {
    /// Amplitude in decibels. Used for thresholds, gains, makeup, ceilings.
    pub struct Db(pub f32): Unit, Ordered, Bounded, Additive, Signed;
}
// dB is logarithmic, so cascaded gain stages ADD — unlike `Beat + Beat`, this
// is meaningful. `Neg` inverts a gain (cut <-> boost).
// NOT `Scalable`: `Db(-6.0) * 2.0 == Db(-12.0)` squares the *amplitude*,
// it does not double the gain. Convert through `Amplitude` for amplitude scaling.
// NOT `Ratio`: a quotient of logarithms is not a quantity.

impl Db {
    /// The metering floor. Silence has no logarithm, so a display needs a
    /// finite stand-in for it; `-inf` cannot be drawn on a fader.
    ///
    /// −144 dB is roughly the noise floor of 24-bit audio, so it is below
    /// anything real while staying finite.
    pub const FLOOR: Db = Db(-144.0);

    /// Unity gain.
    pub const UNITY: Db = Db(0.0);

    /// This gain as a linear amplitude multiplier.
    #[inline]
    #[must_use]
    pub fn to_amplitude(self) -> Amplitude {
        Amplitude(10.0_f32.powf(self.0 / 20.0))
    }

    /// Amplitude as decibels. **Silence is `-inf`, not [`FLOOR`](Self::FLOOR).**
    ///
    /// No floor is applied. Silence has no logarithm, and what to substitute
    /// for it belongs to the consumer, not to the conversion: a meter wants one
    /// engine-wide floor to bottom out at, a ratio on a ±60 dB scale wants its
    /// own bound, and arithmetic that must round-trip wants no floor at all
    /// (`to_amplitude` of `-inf` is exactly `0.0`, which a pinned floor loses).
    /// A caller that needs a floor pins it at its own boundary, where the
    /// choice is visible.
    ///
    /// `From<Amplitude> for Db` is this method, so `.into()` also yields `-inf`
    /// for silence — reach for it only where that is correct.
    #[inline]
    #[must_use]
    pub fn from_amplitude(amp: Amplitude) -> Db {
        Db(20.0 * amp.0.log10())
    }
}

unit! {
    /// A linear gain multiplier.
    ///
    /// **Not** a 0..1 quantity: a +6 dB peak is `Amplitude(2.0)`, and the
    /// dynamics detectors routinely see values above unity. The only floor is
    /// zero — [`SILENT`](Self::SILENT).
    pub struct Amplitude(pub f32): Unit, Ordered, Bounded, Scalable;
}
// NOT `Additive`: cascading two gain stages MULTIPLIES them. Summing
// amplitudes is what mixing two signals does, and that is the signals' job,
// not the gains'.

impl Amplitude {
    /// Silence.
    pub const SILENT: Amplitude = Amplitude(0.0);
    /// Unity gain — the signal passes unchanged.
    pub const UNITY: Amplitude = Amplitude(1.0);

    /// This amplitude in decibels. Silence is `-inf`; see
    /// [`Db::from_amplitude`], which this is.
    #[inline]
    #[must_use]
    pub fn to_db(self) -> Db {
        Db::from_amplitude(self)
    }
}

// Silence converts to `-inf`, per `Db::from_amplitude` — the floor belongs to
// the consumer. A site that needs a finite floor pins it itself rather than
// reaching for `.into()`.
impl From<Amplitude> for Db {
    #[inline]
    fn from(amp: Amplitude) -> Db {
        Db::from_amplitude(amp)
    }
}

impl From<Db> for Amplitude {
    #[inline]
    fn from(db: Db) -> Amplitude {
        db.to_amplitude()
    }
}

#[cfg(test)]
mod tests;
