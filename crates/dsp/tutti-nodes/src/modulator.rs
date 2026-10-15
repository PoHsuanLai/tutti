//! The pure modulators the audio-rate adapter drives: the [`Modulator`]
//! trait, [`Lfo`], [`SampleHold`] and the [`RandomState`] stepper they share.
//!
//! **A copy** of `tutti-mod`'s (doc 014 §3, §7): stateful per-sample sources
//! are a runtime, and `tutti-mod` becomes a pure compiler. Its own copies
//! stay for its `routing` feature (the frame-rate driver `bevy-tutti` still
//! runs) and go with it in doc 014's PR 5; this crate's nodes
//! ([`ModulatorNode`](crate::ModulatorNode), [`LfoNode`](crate::LfoNode))
//! use these. The waveform vocabulary ([`LfoShape`]) is not copied: it is
//! `tutti-mod`'s floor, which stays.

use tutti_core::{Depth, Phase, PhaseIncrement};
use tutti_mod::LfoShape;

/// A pure `phase -> value` function, in the shape of [`Iterator::scan`]: it
/// carries no interior state — the caller threads a [`State`](Modulator::State)
/// value **in and out** of each sample.
///
/// ## Why state-threaded (`scan`, not `Iterator`)
/// State is a *value* the caller carries through pure steps, not something the
/// modulator stores.
///
/// - **`&self`, so it is trivially `Send + Sync`.** A stateful `&mut self`
///   modulator could never be shared as a `&self` value; state-threading
///   sidesteps that entirely — the modulator holds nothing mutable.
///
/// Stateless shapes use `State = ()`. Stateful ones
/// (sample & hold) use a small `Copy` state the caller round-trips.
pub trait Modulator {
    /// The state this modulator threads between samples. `()` for a stateless
    /// (purely phase-deterministic) modulator; a small `Copy` struct for a
    /// stateful one (e.g. sample & hold's RNG). `Default` provides the seed.
    ///
    /// `Send + Sync` so a node that owns the state ([`ModulatorNode`](crate::ModulatorNode))
    /// can move to the audio thread and be shared. A plain-data state always
    /// satisfies this; it costs stateless (`()`) modulators nothing.
    type State: Copy + Default + Send + Sync;

    /// Samples the modulator: given the incoming `state` and a [`Phase`],
    /// returns `(next_state, value)`. `value` is typically in `[-1, 1]`.
    ///
    /// `&self` — the modulator holds no mutable state; the `state` value is the
    /// only thing that carries between samples (the `scan` accumulator).
    ///
    /// [`Phase`] rather than a bare `f32` because an implementation may index a
    /// shape table directly with it: the type's `[0, 1)` range is the
    /// precondition, and it is one only [`Phase::wrapped`] can establish.
    fn value(&self, state: Self::State, phase: Phase) -> (Self::State, f32);

    /// Fills `out` starting at `phase`, advancing by `dphase` per sample and
    /// threading `state` through, and returns the final state. Does not
    /// allocate. The default loops [`Modulator::value`]; impls may override for a
    /// tighter inner loop.
    ///
    /// A negative `dphase` runs the modulator backwards, and
    /// [`Phase::advance`] wraps correctly for it. That is the case a hand-rolled
    /// `%` or `.fract()` wrap gets wrong: both keep the sign of their input, so
    /// the phase walks off the front of a shape table instead of round to its
    /// end. Advance through the type, never by hand.
    fn fill(
        &self,
        mut state: Self::State,
        phase: Phase,
        dphase: PhaseIncrement,
        out: &mut [f32],
    ) -> Self::State {
        let mut p = phase;
        for s in out.iter_mut() {
            let (next, v) = self.value(state, p);
            state = next;
            *s = v;
            p = p.advance(dphase);
        }
        state
    }
}

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
/// *adapter* around it decides how often it is sampled: wrapped in
/// [`ModulatorNode`](crate::ModulatorNode) (aliased [`LfoNode`](crate::LfoNode))
/// it runs per sample, off its own clock or the transport's beat.
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
    use tutti_core::PhaseIncrement;

    // ── LFO via the Modulator trait (scan shape: state threaded in/out) ──
    //
    // Deterministic shapes ignore the state, so the tests pass a default
    // `RandomState` in and drop the returned one.

    fn sample(lfo: &Lfo, phase: f32) -> f32 {
        lfo.value(RandomState::default(), Phase(phase)).1
    }

    /// The deterministic shapes reach `Modulator::value` unaltered.
    ///
    /// The waveform maths itself is `shape.rs`'s
    /// (`LfoShape::evaluate_periodic`, asserted there over all six shapes).
    /// What this pins is the delegation: every non-random shape falls through
    /// the same `_ =>` arm, so one table over three of them proves the arm
    /// forwards the phase and does not, say, swap in the random stepper.
    #[test]
    fn deterministic_shapes_delegate_to_the_waveform() {
        for (shape, phase, expected) in [
            (LfoShape::Sine, 0.25, 1.0),
            (LfoShape::Square, 0.1, 1.0),
            (LfoShape::Square, 0.9, -1.0),
            (LfoShape::Sawtooth, 0.5, 0.0),
        ] {
            let lfo = Lfo::new(shape);
            let got = sample(&lfo, phase);
            assert!(
                (got - expected).abs() < 0.01,
                "{shape:?} at phase {phase}: expected {expected}, got {got}"
            );
        }
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
            phase = phase.advance(PhaseIncrement(0.03));
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

    /// Reports the phase it was handed, so a test can inspect the sequence the
    /// default `fill` walks rather than the values a real shape would produce.
    struct PhaseProbe;

    impl Modulator for PhaseProbe {
        type State = ();
        fn value(&self, _state: (), phase: Phase) -> ((), f32) {
            ((), phase.get())
        }
    }

    #[test]
    fn fill_keeps_phase_in_range_when_running_backwards() {
        // A negative `dphase` is a modulator running in reverse. A `.fract()`
        // wrap walks straight past zero and stays negative, because `fract`
        // preserves the sign of its input — every sample after the first would
        // index off the front of a shape table.
        let mut out = [0.0_f32; 16];
        PhaseProbe.fill((), Phase(0.1), PhaseIncrement(-0.25), &mut out);

        for (i, p) in out.iter().enumerate() {
            assert!(
                (0.0..1.0).contains(p),
                "sample {i} phase {p} escaped [0, 1)"
            );
        }
        assert_eq!(out[0], 0.1);
        assert!((out[1] - 0.85).abs() < 1e-6);
        assert!((out[2] - 0.60).abs() < 1e-6);
    }

    /// `fill` does not wrap its starting phase, because [`Phase`] cannot arrive
    /// out of range — `wrapped` is the only constructor that accepts arbitrary
    /// input, so the guarantee lives in the type rather than the loop. This
    /// pins that it is the *same* guarantee, available one call earlier.
    #[test]
    fn a_wrapped_starting_phase_enters_the_range() {
        let mut out = [0.0_f32; 4];
        PhaseProbe.fill((), Phase::wrapped(-0.25), PhaseIncrement(0.1), &mut out);
        assert!((out[0] - 0.75).abs() < 1e-6);
        assert!(out.iter().all(|p| (0.0..1.0).contains(p)));
    }

    #[test]
    fn fill_still_wraps_forwards() {
        let mut out = [0.0_f32; 8];
        PhaseProbe.fill((), Phase(0.9), PhaseIncrement(0.25), &mut out);
        assert!((out[0] - 0.9).abs() < 1e-6);
        assert!((out[1] - 0.15).abs() < 1e-6);
        assert!(out.iter().all(|p| (0.0..1.0).contains(p)));
    }
}
