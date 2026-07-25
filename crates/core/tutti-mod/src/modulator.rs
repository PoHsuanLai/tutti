//! The [`Modulator`] trait — the core abstraction of this crate.

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

    /// Pure sample: given the incoming `state` and `phase ∈ [0, 1)`, return the
    /// `(next_state, value)`. `value` is typically `[-1, 1]`.
    ///
    /// `&self` — the modulator holds no mutable state; the `state` value is the
    /// only thing that carries between samples (the `scan` accumulator).
    fn value(&self, state: Self::State, phase: f32) -> (Self::State, f32);

    /// Block form, alloc-free: fill `out` starting at `phase`, advancing by
    /// `dphase` (wrapped to `[0, 1)`) per sample, threading `state` through.
    /// Returns the final state. The default loops [`Modulator::value`]; impls
    /// may override for a tighter inner loop.
    ///
    /// Wraps with `rem_euclid`, not `.fract()`: `fract` keeps the sign of its
    /// input, so a negative `dphase` — a modulator running backwards — walks
    /// the phase below zero and leaves the `[0, 1)` range every implementation
    /// of [`value`](Modulator::value) assumes, indexing off the front of a
    /// shape table. `tutti_types::Phase` is the typed form of this rule, but
    /// this crate's pure floor deliberately does not depend on `tutti-types`,
    /// so the invariant is spelled out here instead.
    fn fill(
        &self,
        mut state: Self::State,
        phase: f32,
        dphase: f32,
        out: &mut [f32],
    ) -> Self::State {
        let mut p = phase.rem_euclid(1.0);
        for s in out.iter_mut() {
            let (next, v) = self.value(state, p);
            state = next;
            *s = v;
            p = (p + dphase).rem_euclid(1.0);
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
        fn value(&self, _state: (), phase: f32) -> ((), f32) {
            ((), phase)
        }
    }

    #[test]
    fn fill_keeps_phase_in_range_when_running_backwards() {
        // A negative `dphase` is a modulator running in reverse. Under the old
        // `.fract()` wrap this walked straight past zero and stayed negative,
        // because `fract` preserves the sign of its input — every sample after
        // the first would index off the front of a shape table.
        let mut out = [0.0_f32; 16];
        PhaseProbe.fill((), 0.1, -0.25, &mut out);

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

    #[test]
    fn fill_wraps_a_starting_phase_that_is_already_out_of_range() {
        let mut out = [0.0_f32; 4];
        PhaseProbe.fill((), -0.25, 0.1, &mut out);
        assert!((out[0] - 0.75).abs() < 1e-6);
        assert!(out.iter().all(|p| (0.0..1.0).contains(p)));
    }

    #[test]
    fn fill_still_wraps_forwards() {
        let mut out = [0.0_f32; 8];
        PhaseProbe.fill((), 0.9, 0.25, &mut out);
        assert!((out[0] - 0.9).abs() < 1e-6);
        assert!((out[1] - 0.15).abs() < 1e-6);
        assert!(out.iter().all(|p| (0.0..1.0).contains(p)));
    }
}
