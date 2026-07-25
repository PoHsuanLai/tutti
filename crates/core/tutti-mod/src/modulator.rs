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
    fn fill(
        &self,
        mut state: Self::State,
        phase: f32,
        dphase: f32,
        out: &mut [f32],
    ) -> Self::State {
        let mut p = phase;
        for s in out.iter_mut() {
            let (next, v) = self.value(state, p);
            state = next;
            *s = v;
            p = (p + dphase).fract();
        }
        state
    }
}
