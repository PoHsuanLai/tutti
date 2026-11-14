//! A voice's per-note (MPE) expression, and the one place it is applied.

use tutti_midi_protocol::value_to_unit_f32;
use tutti_ump::ControlValue;

/// One per-note expression value, `0..=1`: a pressure, a slide position or a
/// per-note gain, as the controller that carries it.
///
/// **Range-checked**: [`new`](Self::new) refuses NaN, the infinities and
/// anything outside `0..=1`. [`of`](Self::of) is for a literal;
/// [`from_control`](Self::from_control) is the one total construction, from a
/// controller word.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub(crate) struct Expression(f32);

impl Expression {
    /// The bottom of the range.
    pub(crate) const MIN: Self = Self(0.0);
    /// The top of the range.
    pub(crate) const MAX: Self = Self(1.0);

    /// The value, or `None` when `raw` is NaN, infinite or outside `0..=1`.
    #[must_use]
    pub(crate) const fn new(raw: f32) -> Option<Self> {
        if raw >= 0.0 && raw <= 1.0 {
            Some(Self(raw))
        } else {
            None
        }
    }

    /// The value for a literal or a constant.
    ///
    /// # Panics
    ///
    /// When `raw` is NaN or outside `0..=1`; in a `const` that is a compile
    /// error.
    #[must_use]
    #[track_caller]
    pub(crate) const fn of(raw: f32) -> Self {
        match Self::new(raw) {
            Some(value) => value,
            None => panic!("an expression value is within 0..=1"),
        }
    }

    /// The position a controller word stands for: `0` is the bottom of the
    /// range and `u32::MAX` the top, so every word lands inside `0..=1`.
    #[must_use]
    pub(crate) fn from_control(value: ControlValue) -> Self {
        Self(value_to_unit_f32(value))
    }

    /// The raw position.
    #[must_use]
    pub(crate) const fn get(self) -> f32 {
        self.0
    }
}

/// Slide (CC74) centre: "no timbre shift". The cutoff factor is
/// `4^(slide - SLIDE_CENTER)`, so a note with no CC74 must start here.
pub(crate) const SLIDE_CENTER: Expression = Expression::of(0.5);

/// One per-note expression value, as a message addresses it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum MpeControl {
    /// Key pressure.
    Pressure(Expression),
    /// Slide (CC74), centred at [`SLIDE_CENTER`].
    Slide(Expression),
    /// Per-note volume.
    Gain(Expression),
}

/// A voice's per-note expression received so far. Which messages reach a voice (Detach, the bend's
/// sensitivity) is `tutti-voicing`'s decision; the values arrive resolved.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MpeVoiceState {
    /// Per-note pressure.
    pub pressure: Expression,
    /// Per-note slide, centred at [`SLIDE_CENTER`].
    pub slide: Expression,
    /// Per-note gain.
    pub gain: Expression,
}

impl MpeVoiceState {
    pub(crate) fn new() -> Self {
        Self {
            pressure: Expression::MIN,
            slide: SLIDE_CENTER,
            gain: Expression::MAX,
        }
    }

    /// Stores one controller.
    pub(crate) fn apply(&mut self, control: MpeControl) {
        match control {
            MpeControl::Pressure(p) => self.pressure = p,
            MpeControl::Slide(s) => self.slide = s,
            MpeControl::Gain(g) => self.gain = g,
        }
    }

    /// Returns the controllers to their defaults.
    pub(crate) fn reset(&mut self) {
        *self = Self::new();
    }

    /// The slide in effect: `None` at centre.
    pub(crate) fn effective_slide(&self) -> Option<Expression> {
        ((self.slide.get() - SLIDE_CENTER.get()).abs() > 0.001).then_some(self.slide)
    }

    /// The gain factors in effect, `(pressure, note)`. The pressure is the
    /// larger of the note's own and the channel's, `channel_pressure`.
    pub(crate) fn gain_factors(&self, channel_pressure: f32) -> (f32, f32) {
        (
            1.0 + self.pressure.get().max(channel_pressure) * 0.5,
            self.gain.get(),
        )
    }
}

#[cfg(test)]
mod tests;
