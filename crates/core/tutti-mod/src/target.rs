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
}
