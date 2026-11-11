//! A voice's per-note (MPE) expression, and the one place it is applied.

use tutti_types::{Amplitude, Semitones};

/// Slide (CC74) centre: "no timbre shift". The cutoff factor is
/// `4^(slide - SLIDE_CENTER)`, so a note with no CC74 must start here.
pub(crate) const SLIDE_CENTER: f32 = 0.5;

/// One per-note expression value, as a message addresses it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum MpeControl {
    /// Bend in semitones.
    PitchBend(Semitones),
    /// Key pressure, `0..=1`.
    Pressure(f32),
    /// Slide (CC74), `0..=1`, centred at [`SLIDE_CENTER`].
    Slide(f32),
    /// Per-note volume, `0..=1`.
    Gain(f32),
}

/// A voice's MPE state: whether it is applied at render time and the
/// expression received so far. Which messages reach a voice (Detach, the bend's
/// sensitivity) is `tutti-voicing`'s decision; the values arrive resolved.
#[derive(Debug, Clone, Copy)]
pub struct MpeVoiceState {
    enabled: bool,
    /// Per-note pitch bend.
    pub pitch_bend_semitones: Semitones,
    /// Per-note pressure, `0..=1`.
    pub pressure: f32,
    /// Per-note slide, `0..=1`, centred at [`SLIDE_CENTER`].
    pub slide: f32,
    /// Per-note gain, `0..=1`.
    pub gain: Amplitude,
}

impl MpeVoiceState {
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            pitch_bend_semitones: Semitones(0.0),
            pressure: 0.0,
            slide: SLIDE_CENTER,
            gain: Amplitude::UNITY,
        }
    }

    /// Whether the expression is applied at render time.
    #[cfg(test)]
    pub(crate) fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Turns the expression on or off. Turning it off also resets it: the
    /// controllers store while disabled, and would otherwise snap into effect
    /// when re-enabled.
    pub(crate) fn set_enabled(&mut self, enabled: bool) {
        if self.enabled != enabled {
            self.enabled = enabled;
            if !enabled {
                self.reset();
            }
        }
    }

    /// Stores one controller.
    pub(crate) fn apply(&mut self, control: MpeControl) {
        match control {
            MpeControl::PitchBend(bend) => self.pitch_bend_semitones = bend,
            MpeControl::Pressure(p) => self.pressure = p.clamp(0.0, 1.0),
            MpeControl::Slide(s) => self.slide = s.clamp(0.0, 1.0),
            // Per-note Volume's `0..1`, narrower than `Amplitude`'s own floor.
            MpeControl::Gain(g) => self.gain = Amplitude(g.clamp(0.0, 1.0)),
        }
    }

    /// Returns the controllers to their defaults.
    pub(crate) fn reset(&mut self) {
        *self = Self::new(self.enabled);
    }

    /// The bend in effect: `None` when disabled or inaudibly small.
    pub(crate) fn effective_bend(&self) -> Option<Semitones> {
        (self.enabled && self.pitch_bend_semitones.get().abs() > 0.001)
            .then_some(self.pitch_bend_semitones)
    }

    /// The slide in effect: `None` when disabled or at centre.
    pub(crate) fn effective_slide(&self) -> Option<f32> {
        (self.enabled && (self.slide - SLIDE_CENTER).abs() > 0.001).then_some(self.slide)
    }

    /// The gain factors in effect, `(pressure, note)`; unity when disabled.
    pub(crate) fn gain_factors(&self) -> (f32, f32) {
        if self.enabled {
            (1.0 + self.pressure * 0.5, self.gain.get())
        } else {
            (1.0, 1.0)
        }
    }
}
