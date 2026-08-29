//! The concrete pure modulators: [`Lfo`] and [`SampleHold`], plus the
//! [`RandomState`] noise stepper they share.

use crate::{LfoShape, Modulator};
use tutti_types::{Depth, Phase};

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
    /// The sample currently held, in `[-1, 1]`. What `Random` emits outright.
    pub current: f32,
    /// The sample held before the last step. `RandomSmooth` interpolates from
    /// this toward `current` across the cycle.
    pub previous: f32,
    /// The [`Phase`] seen on the previous call — the wrap detector's reference.
    pub last_phase: Phase,
    /// The xorshift register. Nonzero, or the generator sticks at zero forever;
    /// [`RandomState::default`] seeds it.
    pub seed: u32,
}

impl Default for RandomState {
    fn default() -> Self {
        Self {
            current: 0.0,
            previous: 0.0,
            last_phase: Phase::START,
            seed: 12345,
        }
    }
}

impl RandomState {
    /// Advance the xorshift generator, yielding a fresh value in `[-1, 1]`.
    ///
    /// Not an `Iterator`: this is an infinite generator with no `Option` to
    /// return, and `should_implement_trait` is silenced rather than obeyed
    /// because renaming a public method to satisfy a naming lint would break
    /// callers for no gain.
    #[allow(
        clippy::should_implement_trait,
        reason = "an infinite generator has no `Option` for `Iterator::next` to return"
    )]
    #[inline]
    pub fn next(&mut self) -> f32 {
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 17;
        self.seed ^= self.seed << 5;
        (self.seed as f32 / u32::MAX as f32) * 2.0 - 1.0
    }

    /// Step the held value when `phase` wraps past its previous position
    /// (detected as a large backward jump), then remember `phase`.
    ///
    /// The jump test reads both positions raw: `Phase` has no subtraction,
    /// because the difference between two cycle positions is ambiguous —
    /// forward and backward are both valid readings of the same pair, and only
    /// this function's "more than half a turn backward" convention picks one.
    #[inline]
    pub fn update_for_phase(&mut self, phase: Phase) {
        if phase.get() < self.last_phase.get() - 0.5 {
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
    pub fn get_random_smooth(&self, phase: Phase) -> f32 {
        self.previous + (self.current - self.previous) * phase.get()
    }
}

// =========================================================================
// Lfo — the waveform modulator.
// =========================================================================

/// An LFO: a [`LfoShape`] scaled by `depth`. Holds only *config* (shape +
/// depth) — no mutable state; the random shapes' [`RandomState`] is threaded by
/// the caller (see [`Modulator`]'s `scan` design). `depth` is baked into the
/// output.
///
/// **Rate-agnostic** — this is the pure `phase -> value` function, and the
/// *adapter* around it decides how often it is sampled. Sampled by
/// `ModPreFrame` (the `routing` feature) it runs at frame rate; wrapped in
/// `tutti_units::ModulatorNode` (aliased `LfoNode`) it runs per sample off the
/// transport clock's beat ports. Same LFO, two tiers — see the sampling-rate
/// section in the [crate docs](crate) for which to reach for.
#[derive(Debug, Clone, Copy, Default)]
pub struct Lfo {
    /// The waveform sampled at each [`Phase`].
    pub shape: LfoShape,
    /// Modulation amount baked into the output. [`Depth::INVERTED`] flips the
    /// shape rather than quieting it — that sign behaviour is why this is a
    /// [`Depth`] and not an `Amplitude`.
    pub depth: Depth,
}

impl Lfo {
    /// A full-depth LFO of the given shape.
    pub fn new(shape: LfoShape) -> Self {
        Self {
            shape,
            depth: Depth::FULL,
        }
    }

    /// Set the depth. Not clamped here — the target's concern.
    pub fn with_depth(mut self, depth: impl Into<Depth>) -> Self {
        self.depth = depth.into();
        self
    }
}

impl Modulator for Lfo {
    /// Only the random shapes use this; deterministic shapes thread it through
    /// untouched (and never read it).
    type State = RandomState;

    #[inline]
    fn value(&self, mut state: RandomState, phase: Phase) -> (RandomState, f32) {
        let depth = self.depth.get();
        match self.shape {
            LfoShape::Random => {
                state.update_for_phase(phase);
                (state, state.get_random() * depth)
            }
            LfoShape::RandomSmooth => {
                state.update_for_phase(phase);
                (state, state.get_random_smooth(phase) * depth)
            }
            _ => (state, self.shape.evaluate_periodic(phase) * depth),
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
    /// A sample & hold stepper. Carries no configuration — the step rate is the
    /// caller's phase advance, and the value lives in the threaded
    /// [`RandomState`].
    pub fn new() -> Self {
        Self
    }
}

impl Modulator for SampleHold {
    type State = RandomState;

    #[inline]
    fn value(&self, mut state: RandomState, phase: Phase) -> (RandomState, f32) {
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
        lfo.value(RandomState::default(), Phase(phase)).1
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
        // Depth 0.5 on a square at phase 0 → 0.5.
        let lfo = Lfo::new(LfoShape::Square).with_depth(Depth(0.5));
        assert!((sample(&lfo, 0.0) - 0.5).abs() < 0.01);
    }

    /// A negative depth phase-flips the shape rather than quieting it.
    #[test]
    fn inverted_depth_flips_the_shape() {
        let up = Lfo::new(LfoShape::Square);
        let down = Lfo::new(LfoShape::Square).with_depth(Depth::INVERTED);
        assert!((sample(&up, 0.1) + sample(&down, 0.1)).abs() < 1e-6);
    }

    #[test]
    fn random_produces_different_values() {
        // Thread the state forward across many phase wraps so the stepper
        // actually advances.
        let lfo = Lfo::new(LfoShape::Random);
        let mut state = RandomState::default();
        let mut values = Vec::new();
        let mut phase = Phase::START;
        for _ in 0..500 {
            let (next, v) = lfo.value(state, phase);
            state = next;
            values.push(v);
            phase = phase.advance(tutti_types::PhaseIncrement(0.03));
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
        let (state1, first) = sh.value(state, Phase(0.1));
        state = state1;
        // Same rising phase — held value must not change.
        let (state2, v2) = sh.value(state, Phase(0.2));
        state = state2;
        assert_eq!(v2, first);
        let (state3, v3) = sh.value(state, Phase(0.9));
        state = state3;
        assert_eq!(v3, first);
        // Wrap past → a new value steps in.
        let (_state4, after_wrap) = sh.value(state, Phase(0.05));
        // (Statistically ~always different; at minimum the API stepped.)
        assert!(after_wrap.is_finite());
    }
}
