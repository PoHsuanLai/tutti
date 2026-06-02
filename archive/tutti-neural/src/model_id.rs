//! Opaque handle for a registered neural model.
//!
//! Backends produce [`ModelId`]s via [`Backend::load`](crate::Backend::load)
//! or [`Backend::register_model`](crate::Backend::register_model) (Burn
//! escape hatch); callers reference the same id when building audio nodes
//! or calling [`Engine::unload`](crate::Engine::unload). The inner counter
//! is monotonic within a process; ids are `Copy + Hash` for use in maps.

use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

static MODEL_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Opaque model handle. `Copy + Hash`; the inner counter is not part of the
/// public contract beyond `Display`.
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelId(u64);

impl ModelId {
    /// Allocate a fresh id from the crate-wide monotonic counter.
    pub fn new() -> Self {
        Self(MODEL_ID_COUNTER.fetch_add(1, Ordering::Relaxed))
    }

    /// Reconstruct an id from a raw `u64` — only valid for ids that were
    /// previously produced by [`Self::as_u64`] on the same process run, or
    /// by hand when the caller is certain no collision exists.
    pub fn from_raw(id: u64) -> Self {
        Self(id)
    }

    /// The raw `u64` behind this id. Useful for stable serialisation.
    pub fn as_u64(&self) -> u64 {
        self.0
    }
}

impl Default for ModelId {
    fn default() -> Self {
        Self::new()
    }
}

impl core::fmt::Display for ModelId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Model({})", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_id_generation() {
        let id1 = ModelId::new();
        let id2 = ModelId::new();
        assert_ne!(id1.as_u64(), id2.as_u64());
    }

    #[test]
    fn test_model_id_from_raw() {
        let id = ModelId::from_raw(12345);
        assert_eq!(id.as_u64(), 12345);
    }
}
