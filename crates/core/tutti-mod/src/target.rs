//! The modulation **sink** — the keyed accumulator a source drives.
//!
//! [`ModTarget`] is the receive role. It is *addressed* — it **is** the
//! destination and does not know its own [`crate::ModTargetId`] (the router
//! holds that). Its payload carries a **contributor key** ([`LayerKey`]).
//!
//! That key belongs on the sink, not on the router, because the three
//! properties it buys are properties of *this accumulator's own keyed register*:
//!
//! - **idempotent re-assert** — a source re-writing its offset each frame
//!   overwrites its own layer in place, never doubling up;
//! - **independent clear** — one contributor can be removed without disturbing
//!   the others (or the automation layer);
//! - **order-independent sum** — `base + Σ offsets` is a set-sum.
//!
//! Pushing the key into the router would make the router re-derive the
//! accumulator's keyed storage. The router keys by *sink id* and the sink keys
//! by *contributor* — two clean lookups, two owners.
//!
//! Methods take `&self` (interior-mutable implementors) so the driver can hold
//! an `Arc<dyn ModTarget>` and drive it once per frame. This is control-rate,
//! not the audio callback — a target's write is a value upsert, never an
//! enqueue, so there is no ring/mailbox.

use crate::id::LayerKey;

/// A keyed modulation accumulator: `final = clamp(base + Σ keyed offsets,
/// [min, max])`. The receive role of the modulation subsystem.
pub trait ModTarget: Send + Sync {
    /// The `(min, max)` clamp range. The driver reads this to scale a `[-1, 1]`
    /// raw modulator value into the target's units before accumulating — the
    /// range is the *target's* property, so it lives here (not on the source).
    fn range(&self) -> (f32, f32);

    /// The authored base value. Modulation never mutates it — offsets are
    /// computed against it, which is what keeps summation drift-free.
    fn base(&self) -> f32;

    /// Set the authored base — the one write reserved for the base's single
    /// owner (projection / UI), never modulation. Re-flushes `final_value`. The
    /// value is clamped into the target's range by the implementor.
    fn set_base(&self, value: f32);

    /// Upsert this contributor's offset. Re-asserting `key` overwrites in place
    /// (idempotent); contributions across keys sum order-independently.
    fn accumulate(&self, key: LayerKey, offset: f32);

    /// Remove one contributor's offset (edge deleted / source gone silent).
    /// No-op if the key is absent.
    fn clear(&self, key: LayerKey);

    /// The folded, clamped result: `clamp(base + Σ offsets, [min, max])`.
    fn final_value(&self) -> f32;

    /// Install a **beat-varying** contribution under `key`, replacing whatever
    /// that key held. Returns `false` if this sink only takes scalars.
    ///
    /// The difference from [`accumulate`](Self::accumulate) is *when the value
    /// is decided*, not what it is: a scalar is computed by the driver once per
    /// frame and stored; a curve is stored as a function and evaluated by the
    /// sink at whatever rate it reads. A sub-block sink (a plugin's per-block
    /// parameter producer) traces a smooth ramp from the same source that a
    /// frame-rate sink would see as a staircase.
    ///
    /// Defaults to declining, because for most sinks a curve layer is not
    /// *wrong* so much as pointless: [`AtomicTarget`](crate::AtomicTarget)
    /// collapses at a fixed beat, so a curve stored there would evaluate to one
    /// unchanging value and look like a stuck modulator. Returning `false` lets
    /// a caller fall back to scalar delivery, which is always correct — rather
    /// than installing something that silently never moves.
    ///
    /// The curve must be an **offset** (swinging around zero), not an absolute
    /// value: the sink adds the base and applies the clamp. Build one with
    /// [`ShapedCurve`](crate::ShapedCurve), which derives the offset from the
    /// same [`shape`](crate::shape) call the scalar path uses.
    ///
    /// Routing-gated, like [`Curve`](crate::Curve) itself — the pure floor has
    /// no notion of a beat to evaluate one at.
    #[cfg(feature = "routing")]
    fn accumulate_curve(&self, key: LayerKey, curve: std::sync::Arc<dyn crate::Curve>) -> bool {
        let _ = (key, curve);
        false
    }
}
