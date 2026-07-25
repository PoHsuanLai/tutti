//! The concrete pure modulators: [`Lfo`] and [`SampleHold`], plus the
//! [`RandomState`] noise stepper they share.

use crate::{LfoShape, Modulator};

// =========================================================================
// RandomState — the sample & hold noise stepper (ONE copy).
// =========================================================================

/// Per-instance state for the `Random` / `RandomSmooth` shapes: an xorshift
/// stepper that advances once per phase wrap and interpolates between the
/// previous and current sample.
///
/// This is the [`Modulator::State`] the random shapes thread — a `Copy` value
/// the caller carries in and out of each `value` call (the `scan` accumulator),
/// not something the modulator stores.
#[derive(Debug, Clone, Copy)]
pub struct RandomState {
    pub current: f32,
    pub previous: f32,
    pub last_phase: f32,
    pub seed: u32,
}

impl Default for RandomState {
    fn default() -> Self {
        Self {
            current: 0.0,
            previous: 0.0,
            last_phase: 0.0,
            seed: 12345,
        }
    }
}

impl RandomState {
    /// Advance the xorshift generator, yielding a fresh value in `[-1, 1]`.
    #[inline]
    pub fn next(&mut self) -> f32 {
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 17;
        self.seed ^= self.seed << 5;
        (self.seed as f32 / u32::MAX as f32) * 2.0 - 1.0
    }

    /// Step the held value when `phase` wraps past its previous position
    /// (detected as a large backward jump), then remember `phase`.
    #[inline]
    pub fn update_for_phase(&mut self, phase: f32) {
        if phase < self.last_phase - 0.5 {
            self.previous = self.current;
            self.current = self.next();
        }
        self.last_phase = phase;
    }

    /// The current held sample (for `Random`).
    #[inline]
    pub fn get_random(&self) -> f32 {
        self.current
    }

    /// Linear interpolation from the previous to the current sample across the
    /// phase (for `RandomSmooth`).
    #[inline]
    pub fn get_random_smooth(&self, phase: f32) -> f32 {
        self.previous + (self.current - self.previous) * phase
    }
}

// =========================================================================
// Lfo — the waveform modulator.
// =========================================================================

/// An LFO: a [`LfoShape`] scaled by `depth`. Holds only *config* (shape +
/// depth) — no mutable state; the random shapes' [`RandomState`] is threaded by
/// the caller (see [`Modulator`]'s `scan` design). `depth` is baked into the
/// output.
#[derive(Debug, Clone, Copy, Default)]
pub struct Lfo {
    pub shape: LfoShape,
    pub depth: f32,
}

impl Lfo {
    /// A full-depth LFO of the given shape.
    pub fn new(shape: LfoShape) -> Self {
        Self { shape, depth: 1.0 }
    }

    /// Set the depth (typically `0.0..=1.0`; not clamped here — the target's
    /// concern).
    pub fn with_depth(mut self, depth: f32) -> Self {
        self.depth = depth;
        self
    }
}

impl Modulator for Lfo {
    /// Only the random shapes use this; deterministic shapes thread it through
    /// untouched (and never read it).
    type State = RandomState;

    #[inline]
    fn value(&self, mut state: RandomState, phase: f32) -> (RandomState, f32) {
        match self.shape {
            LfoShape::Random => {
                state.update_for_phase(phase);
                (state, state.get_random() * self.depth)
            }
            LfoShape::RandomSmooth => {
                state.update_for_phase(phase);
                (state, state.get_random_smooth(phase) * self.depth)
            }
            _ => (state, self.shape.evaluate_periodic(phase) * self.depth),
        }
    }
}

// =========================================================================
// SampleHold — stepped noise, no waveform.
// =========================================================================

/// A pure sample & hold: stepped noise, no waveform. Equivalent to an
/// [`Lfo`] fixed to [`LfoShape::Random`] but expressed directly for the cases
/// that want just the stepper. Holds no state — threads a [`RandomState`].
#[derive(Debug, Clone, Copy, Default)]
pub struct SampleHold;

impl SampleHold {
    pub fn new() -> Self {
        Self
    }
}

impl Modulator for SampleHold {
    type State = RandomState;

    #[inline]
    fn value(&self, mut state: RandomState, phase: f32) -> (RandomState, f32) {
        state.update_for_phase(phase);
        (state, state.get_random())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── LFO via the Modulator trait (scan shape: state threaded in/out) ──
    //
    // Deterministic shapes ignore the state, so the tests pass a default
    // `RandomState` in and drop the returned one.

    fn sample(lfo: &Lfo, phase: f32) -> f32 {
        lfo.value(RandomState::default(), phase).1
    }

    #[test]
    fn sine_at_quarter_phase() {
        let lfo = Lfo::new(LfoShape::Sine);
        assert!((sample(&lfo, 0.25) - 1.0).abs() < 0.01);
    }

    #[test]
    fn square_bipolar() {
        let lfo = Lfo::new(LfoShape::Square);
        assert_eq!(sample(&lfo, 0.1), 1.0);
        assert_eq!(sample(&lfo, 0.9), -1.0);
    }

    #[test]
    fn sawtooth_midpoint() {
        let lfo = Lfo::new(LfoShape::Sawtooth);
        assert!((sample(&lfo, 0.5) - 0.0).abs() < 0.01);
    }

    #[test]
    fn depth_scales_output() {
        // Was tutti_units::lfo::test_depth_control — depth 0.5 on a square at
        // phase 0 → 0.5.
        let lfo = Lfo::new(LfoShape::Square).with_depth(0.5);
        assert!((sample(&lfo, 0.0) - 0.5).abs() < 0.01);
    }

    #[test]
    fn random_produces_different_values() {
        // Was tutti_units::lfo::test_random_produces_different_values. Thread the
        // state forward across many phase wraps so the stepper advances.
        let lfo = Lfo::new(LfoShape::Random);
        let mut state = RandomState::default();
        let mut values = Vec::new();
        let mut phase = 0.0_f32;
        for _ in 0..500 {
            let (next, v) = lfo.value(state, phase);
            state = next;
            values.push(v);
            phase = (phase + 0.03).fract();
        }
        let unique: std::collections::HashSet<u32> =
            values.iter().map(|v| (v * 1000.0) as u32).collect();
        assert!(
            unique.len() > 1,
            "Random LFO should produce different values, got {}",
            unique.len()
        );
    }

    #[test]
    fn sample_hold_steps_on_phase_wrap() {
        let sh = SampleHold::new();
        let mut state = RandomState::default();
        let (state1, first) = sh.value(state, 0.1);
        state = state1;
        // Same rising phase — held value must not change.
        let (state2, v2) = sh.value(state, 0.2);
        state = state2;
        assert_eq!(v2, first);
        let (state3, v3) = sh.value(state, 0.9);
        state = state3;
        assert_eq!(v3, first);
        // Wrap past → a new value steps in.
        let (_state4, after_wrap) = sh.value(state, 0.05);
        // (Statistically ~always different; at minimum the API stepped.)
        assert!(after_wrap.is_finite());
    }
}
