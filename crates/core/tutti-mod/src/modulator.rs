//! The [`Modulator`] trait — the core abstraction of this crate.

use tutti_types::{Phase, PhaseIncrement};

/// A pure `phase -> value` function, in the shape of [`Iterator::scan`]: it
/// carries no interior state — the caller threads a [`State`](Modulator::State)
/// value **in and out** of each sample.
///
/// ## Why state-threaded (`scan`, not `Iterator`)
/// State is a *value* the caller carries through pure steps, not something the
/// modulator stores.
///
/// - **`&self`, so it is trivially `Send + Sync`.** A stateful `&mut self`
///   modulator could never be a fundsp graph node (`AudioUnit: Send + Sync`) or
///   a shared `&self` plugin `Curve`. State-threading sidesteps that entirely —
///   the modulator holds nothing mutable.
///
/// Stateless shapes use `State = ()`. Stateful ones
/// (sample & hold) use a small `Copy` state the caller round-trips.
pub trait Modulator {
    /// The state this modulator threads between samples. `()` for a stateless
    /// (purely phase-deterministic) modulator; a small `Copy` struct for a
    /// stateful one (e.g. sample & hold's RNG). `Default` provides the seed.
    ///
    /// `Send + Sync` so a `ModulatorNode<M>` — which owns the state — can be a
    /// fundsp graph node (`AudioUnit: Send + Sync`). A plain-data state always
    /// satisfies this; it costs stateless (`()`) modulators nothing.
    type State: Copy + Default + Send + Sync;

    /// Pure sample: given the incoming `state` and a [`Phase`], return the
    /// `(next_state, value)`. `value` is typically `[-1, 1]`.
    ///
    /// `&self` — the modulator holds no mutable state; the `state` value is the
    /// only thing that carries between samples (the `scan` accumulator).
    ///
    /// [`Phase`] rather than a bare `f32` because an implementation may index a
    /// shape table directly with it: the type's `[0, 1)` range is the
    /// precondition, and it is one only [`Phase::wrapped`] can establish.
    fn value(&self, state: Self::State, phase: Phase) -> (Self::State, f32);

    /// Block form, alloc-free: fill `out` starting at `phase`, advancing by
    /// `dphase` per sample and threading `state` through. Returns the final
    /// state. The default loops [`Modulator::value`]; impls may override for a
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

#[cfg(test)]
mod tests {
    use super::*;

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
