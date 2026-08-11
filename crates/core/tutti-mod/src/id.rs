//! Modulation addressing vocabulary: [`ModTargetId`] and [`LayerKey`].
//!
//! Two distinct addresses:
//!
//! - [`ModTargetId`] is a **target's** address — the id the router
//!   ([`crate::ModBus`]) keys on. A target does not know its own id (it *is* the
//!   destination); the router holds the id→sink map.
//! - [`LayerKey`] is a **contributor's** identity — which source's offset a
//!   layer in a target's accumulator holds. A modulation sink is a *keyed
//!   accumulator*, so the contributor key rides on the value.

use core::sync::atomic::{AtomicU64, Ordering};

/// Opaque per-instance id for a modulation **target** (a keyed accumulator).
///
/// The router's address space. Allocated via [`ModTargetId::next`] from a
/// process-wide atomic counter, so
/// collisions are impossible by construction. `From<u64>` / [`ModTargetId::new`]
/// stay available for deserialization and deterministic tests; they do **not**
/// increment the allocator.
#[cfg_attr(
    feature = "bevy",
    derive(bevy_ecs::prelude::Component, bevy_reflect::Reflect)
)]
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct ModTargetId(u64);

/// Process-wide allocator for `ModTargetId`s. Starts at 1 so the `Default`
/// value (0) is always distinguishable from a real id.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

impl ModTargetId {
    /// Allocate a fresh id, unique across the process. Call once per target at
    /// construction time.
    #[inline]
    pub fn next() -> Self {
        Self(NEXT_ID.fetch_add(1, Ordering::Relaxed))
    }

    /// Construct from a raw `u64`. Prefer [`ModTargetId::next`] for new targets;
    /// this is for deserialization and deterministic tests.
    #[inline]
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    /// The raw id, for serialization or a stable sort key. Carries no meaning
    /// beyond identity — the numbers are allocation order, not an ordering the
    /// router respects.
    #[inline]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl From<u64> for ModTargetId {
    #[inline]
    fn from(id: u64) -> Self {
        Self(id)
    }
}

impl From<ModTargetId> for u64 {
    #[inline]
    fn from(id: ModTargetId) -> Self {
        id.0
    }
}

impl core::fmt::Display for ModTargetId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "ModTargetId({})", self.0)
    }
}

/// Identifies one contributor's offset within a target's accumulator.
///
/// Identifies which contributor an offset belongs to — **not** the target's
/// address (that's [`ModTargetId`]). A parameter's final value is its base plus
/// the sum
/// of every layer's offset; each *writer* (automation, a modulation source, …)
/// owns one `LayerKey` so its contribution updates **in place** rather than
/// accumulating duplicates, and can be cleared independently.
///
/// The key is deliberately an opaque `u64` so this type carries no vocabulary of
/// its own — a writer maps its own stable identity (e.g. a routing-edge id) onto
/// a `u64` at the call site. [`LayerKey::AUTOMATION`] (`0`) is reserved.
#[cfg_attr(feature = "bevy", derive(bevy_reflect::Reflect))]
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct LayerKey(pub u64);

impl LayerKey {
    /// Reserved layer for automation-envelope output. Distinct from any
    /// modulation key: modulation keys must be non-zero (a routing edge derives
    /// its key so it never collides with this).
    pub const AUTOMATION: LayerKey = LayerKey(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_nonzero() {
        let a = ModTargetId::next();
        let b = ModTargetId::next();
        assert_ne!(a, b);
        assert_ne!(a, ModTargetId::default()); // default is 0, allocations start at 1
        assert_ne!(a.as_u64(), 0);
    }

    #[test]
    fn id_roundtrips_u64() {
        let id = ModTargetId::new(42);
        assert_eq!(id.as_u64(), 42);
        assert_eq!(u64::from(id), 42);
        assert_eq!(ModTargetId::from(42u64), id);
    }

    #[test]
    fn automation_layer_is_zero() {
        assert_eq!(LayerKey::AUTOMATION, LayerKey(0));
        assert_ne!(LayerKey(1), LayerKey::AUTOMATION);
    }
}
