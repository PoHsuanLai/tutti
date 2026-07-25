//! The modulation **dispatch** role — [`ModRouter`] + [`ModBus`].
//!
//! A fan-out that owns the id→sink map and dispatches a keyed offset to a
//! [`ModTarget`] selected by [`ModTargetId`]. The id names *which* sink; a
//! write to an unknown id is silently dropped.
//!
//! The map value is `Arc<dyn ModTarget>` (a shared accumulator): modulation
//! accumulates a continuous value rather than enqueuing events, so there is no
//! sender/receiver pair — the "sender" *is* the target.

use std::sync::Arc;

use dashmap::DashMap;

use crate::id::{LayerKey, ModTargetId};
use crate::target::ModTarget;

/// Dispatch a keyed offset to a target selected by [`ModTargetId`].
///
/// The id does the sink lookup; the [`LayerKey`] rides through to the sink (the
/// sink owns its keyed register — see [`ModTarget`]). Splitting "route by id"
/// (here) from "be a keyed sink"
/// ([`ModTarget`]) is the whole point — two clean roles.
pub trait ModRouter: Send + Sync {
    /// Upsert `offset` under `key` on the target addressed by `target`.
    fn accumulate(&self, target: ModTargetId, key: LayerKey, offset: f32);
    /// Clear the `key` layer on the target addressed by `target`.
    fn clear(&self, target: ModTargetId, key: LayerKey);
    /// Read a target's folded value (for a UI mirror / test). `None` if the id
    /// is not registered.
    fn final_value(&self, target: ModTargetId) -> Option<f32>;
}

/// Fan-out bus: `DashMap<ModTargetId, Arc<dyn ModTarget>>`. The [`ModRouter`]
/// implementor. Cheap to clone (shares the map `Arc`).
#[derive(Clone, Default)]
pub struct ModBus {
    targets: Arc<DashMap<ModTargetId, Arc<dyn ModTarget>>>,
}

impl ModBus {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a target under its id. Off-thread; avoid at playback time
    /// (mutating the sink registry mid-run is discouraged — the *routing* is
    /// what hot-swaps, not the target set).
    pub fn insert(&self, id: ModTargetId, target: Arc<dyn ModTarget>) {
        self.targets.insert(id, target);
    }

    /// Deregister a target.
    pub fn remove(&self, id: ModTargetId) {
        self.targets.remove(&id);
    }

    /// Fetch a target handle by id.
    pub fn get(&self, id: ModTargetId) -> Option<Arc<dyn ModTarget>> {
        self.targets.get(&id).map(|r| Arc::clone(r.value()))
    }

    /// How many targets are registered.
    pub fn len(&self) -> usize {
        self.targets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }
}

impl ModRouter for ModBus {
    #[inline]
    fn accumulate(&self, id: ModTargetId, key: LayerKey, offset: f32) {
        if let Some(t) = self.targets.get(&id) {
            t.accumulate(key, offset);
        }
    }
    #[inline]
    fn clear(&self, id: ModTargetId, key: LayerKey) {
        if let Some(t) = self.targets.get(&id) {
            t.clear(key);
        }
    }
    #[inline]
    fn final_value(&self, id: ModTargetId) -> Option<f32> {
        self.targets.get(&id).map(|t| t.final_value())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::param::AtomicTarget;

    #[test]
    fn dispatches_by_id_to_the_right_target() {
        let bus = ModBus::new();
        let cutoff = Arc::new(AtomicTarget::new(1000.0, 0.0, 2000.0));
        let gain = Arc::new(AtomicTarget::new(0.5, 0.0, 1.0));
        let (id_cut, id_gain) = (ModTargetId::next(), ModTargetId::next());
        bus.insert(id_cut, cutoff.clone());
        bus.insert(id_gain, gain.clone());

        bus.accumulate(id_cut, LayerKey(1), 500.0);
        bus.accumulate(id_gain, LayerKey(1), 0.25);

        assert!((bus.final_value(id_cut).unwrap() - 1500.0).abs() < 1e-6);
        assert!((bus.final_value(id_gain).unwrap() - 0.75).abs() < 1e-6);
    }

    #[test]
    fn unknown_id_is_a_silent_drop() {
        let bus = ModBus::new();
        // Nothing registered — accumulate/clear must not panic, final_value None.
        bus.accumulate(ModTargetId::new(999), LayerKey(1), 1.0);
        bus.clear(ModTargetId::new(999), LayerKey(1));
        assert_eq!(bus.final_value(ModTargetId::new(999)), None);
    }

    #[test]
    fn clear_removes_only_that_key() {
        let bus = ModBus::new();
        let t = Arc::new(AtomicTarget::new(0.0, -2.0, 2.0));
        let id = ModTargetId::next();
        bus.insert(id, t);
        bus.accumulate(id, LayerKey(1), 0.5);
        bus.accumulate(id, LayerKey(2), 0.3);
        assert!((bus.final_value(id).unwrap() - 0.8).abs() < 1e-6);
        bus.clear(id, LayerKey(2));
        assert!((bus.final_value(id).unwrap() - 0.5).abs() < 1e-6);
    }
}
