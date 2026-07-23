//! Node parameter types — plain data, always compiled.
//!
//! These are the foundational parameter values every leaf audio crate speaks:
//! [`Volume`], [`Pan`], [`Mute`], [`AudioNode`], [`PluginParam`],
//! and the layered [`ModParam`]. Under the `bevy` feature they gain
//! `#[derive(Component, Reflect)]` and become the ECS parameter components the
//! reconcile hub reads; without it they stay plain structs a non-Bevy host can
//! carry and pass to the graph's imperative API.
//!
//! No `Entity` fields and no relationship semantics live here — that's what lets
//! them degrade cleanly. The Bevy-only wiring (the reconcile systems, the
//! `GraphReconcilePlugin`) lives in the sibling `#[cfg(feature = "bevy")]`
//! modules; edges themselves are wired by the host through `Net`'s imperative
//! `connect`/`disconnect` API.

#[cfg(feature = "bevy")]
use bevy_ecs::prelude::Component;
#[cfg(feature = "bevy")]
use bevy_ecs::reflect::ReflectComponent;
#[cfg(feature = "bevy")]
use bevy_reflect::Reflect;

use crate::dsp::NodeId;

/// Component identity for a graph node.
///
/// Wraps a fundsp [`NodeId`]. Inserted by host helpers (e.g.
/// `Commands::spawn_audio_node`) immediately after the underlying node is
/// added to the graph; removing this component (or despawning the entity)
/// is the signal for the host to remove the node and `commit()`.
///
/// Plain `Copy`. Cheap to clone, store in queries, and hand around.
///
/// Not `Reflect`: the wrapped fundsp `NodeId` is foreign and not reflected.
/// If a host needs scene-serialization for these entities it should map
/// `AudioNode` to/from a stable id of its own (e.g. an asset path or
/// document node id) at serialization time.
#[cfg_attr(feature = "bevy", derive(Component))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AudioNode(pub NodeId);

impl AudioNode {
    /// Convenience: returns the wrapped [`NodeId`].
    #[inline]
    pub fn id(self) -> NodeId {
        self.0
    }
}

impl From<NodeId> for AudioNode {
    #[inline]
    fn from(id: NodeId) -> Self {
        Self(id)
    }
}

impl From<AudioNode> for NodeId {
    #[inline]
    fn from(node: AudioNode) -> Self {
        node.0
    }
}

/// Linear gain component, applied to the node's primary level setter.
///
/// `1.0` is unity gain. Values outside `[0.0, 4.0]` are passed through
/// verbatim (no clamping at this layer); audio-thread soft-limiters in
/// the unit itself are responsible for safety.
#[cfg_attr(feature = "bevy", derive(Component, Reflect), reflect(Component))]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Volume(pub f32);

impl Default for Volume {
    #[inline]
    fn default() -> Self {
        Self(1.0)
    }
}

/// Stereo pan position. `-1.0` is hard-left, `0.0` is centered,
/// `+1.0` is hard-right.
#[cfg_attr(feature = "bevy", derive(Component, Reflect), reflect(Component))]
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Pan(pub f32);

/// Mute flag. When `true`, reconcile systems route the node's output to
/// silence (typically by setting gain to `0.0` on a sampler / track unit
/// or by the bus topology layer if one is present).
#[cfg_attr(feature = "bevy", derive(Component, Reflect), reflect(Component))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mute(pub bool);

/// Generic plugin-parameter component. Reconciled by writing
/// `value` into the plugin's parameter `id` via the RT-safe
/// `PluginHandle::set_parameter` channel.
///
/// Hosts that need many simultaneous parameters typically attach
/// one of these per parameter as a child entity, or extend with a
/// dedicated component per known parameter id.
#[cfg_attr(feature = "bevy", derive(Component, Reflect), reflect(Component))]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PluginParam {
    /// Plugin-defined parameter id (matches `ParameterInfo::id`).
    pub id: u32,
    /// Normalized value. Range is plugin-defined (typically `0.0..=1.0`).
    pub value: f32,
}

// =============================================================================
// Layered parameter (ModParam) — base + named offset layers.
// =============================================================================

/// Opaque identity for one offset layer of a [`ModParam`].
///
/// A parameter's final value is its base plus the sum of every layer's
/// offset. Each *writer* of an offset (automation, a modulation source, …)
/// owns one `LayerKey` so its contribution updates **in place** rather than
/// accumulating duplicates. Reserved: [`LayerKey::AUTOMATION`] (`0`).
///
/// The key is deliberately an opaque `u64` so this engine type carries no
/// DAW vocabulary — the host (dawai) maps its own stable identity
/// (e.g. a routing-edge id) onto a `u64` at the call site.
#[cfg_attr(feature = "bevy", derive(Reflect))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LayerKey(pub u64);

impl LayerKey {
    /// Reserved layer for automation envelope output. Distinct from any
    /// host-assigned modulation key (hosts should map their ids into the
    /// non-zero space, e.g. by ensuring the value is never `0`).
    pub const AUTOMATION: LayerKey = LayerKey(0);
}

/// A parameter as **base + named offset layers**.
///
/// ```text
/// final = clamp( base + Σ layer_offset , range )
/// ```
///
/// - `base` is the authored value, owned by exactly one writer
///   (projection / UI). Modulation and automation **never** mutate it —
///   they contribute *offsets* via [`set_layer`](Self::set_layer). This is
///   what makes summation order-independent and drift-free: every
///   contribution is computed against the fixed base, and reading
///   [`final_value`](Self::final_value) never writes anything back.
/// - layers are `(LayerKey, offset)` pairs. Counts are tiny in practice
///   (automation + a couple of modulators), so a plain `Vec` is used — no
///   extra dependency, and the audio thread never sees this type (only the
///   computed `final_value` is pushed into the node's atomic).
///
/// `dirty` is set on every mutation and cleared by
/// [`take_dirty`](Self::take_dirty), so the per-frame reconciler can push
/// to the graph exactly once when (and only when) the value moved.
#[cfg_attr(feature = "bevy", derive(Component, Reflect), reflect(Component))]
#[derive(Debug, Clone, PartialEq)]
pub struct ModParam {
    base: f32,
    min: f32,
    max: f32,
    layers: std::vec::Vec<(LayerKey, f32)>,
    dirty: bool,
}

impl ModParam {
    /// Create a parameter with an authored `base` clamped into `[min, max]`.
    #[inline]
    pub fn new(base: f32, min: f32, max: f32) -> Self {
        Self {
            base,
            min,
            max,
            layers: std::vec::Vec::new(),
            dirty: true,
        }
    }

    /// The authored value, before any layer offsets. Owned by
    /// projection / UI; this is what modulation reads as its reference.
    #[inline]
    pub fn base(&self) -> f32 {
        self.base
    }

    /// The parameter's `[min, max]` range. Offsets are in the parameter's
    /// own units; the final value is clamped to this range.
    #[inline]
    pub fn range(&self) -> (f32, f32) {
        (self.min, self.max)
    }

    /// Set the authored base value (projection / UI). Does not touch layers.
    #[inline]
    pub fn set_base(&mut self, value: f32) {
        if self.base != value {
            self.base = value;
            self.dirty = true;
        }
    }

    /// Add or update the offset contributed by `key` (automation /
    /// modulation source). Updating an existing key replaces its offset in
    /// place — contributions never accumulate duplicates.
    #[inline]
    pub fn set_layer(&mut self, key: LayerKey, offset: f32) {
        if let Some(entry) = self.layers.iter_mut().find(|(k, _)| *k == key) {
            if entry.1 != offset {
                entry.1 = offset;
                self.dirty = true;
            }
        } else {
            self.layers.push((key, offset));
            self.dirty = true;
        }
    }

    /// Remove the offset contributed by `key`. No-op if absent.
    #[inline]
    pub fn clear_layer(&mut self, key: LayerKey) {
        let before = self.layers.len();
        self.layers.retain(|(k, _)| *k != key);
        if self.layers.len() != before {
            self.dirty = true;
        }
    }

    /// Remove every modulation layer (all keys except
    /// [`LayerKey::AUTOMATION`]). Called at the start of a modulation pass
    /// so stale sources stop contributing; automation persists.
    #[inline]
    pub fn clear_mod_layers(&mut self) {
        let before = self.layers.len();
        self.layers.retain(|(k, _)| *k == LayerKey::AUTOMATION);
        if self.layers.len() != before {
            self.dirty = true;
        }
    }

    /// The computed value pushed to the audio node:
    /// `clamp(base + Σ offsets, range)`. Pure — never mutates state, so
    /// repeated reads are stable and modulation cannot drift the base.
    #[inline]
    pub fn final_value(&self) -> f32 {
        let sum: f32 = self.layers.iter().map(|(_, off)| *off).sum();
        (self.base + sum).clamp(self.min, self.max)
    }

    /// Returns whether the parameter changed since the last call, clearing
    /// the flag. Drives the once-per-frame push in the reconciler.
    #[inline]
    pub fn take_dirty(&mut self) -> bool {
        core::mem::replace(&mut self.dirty, false)
    }

    /// Whether the parameter carries no offset layers at all (neither
    /// automation nor modulation) — i.e. `final_value() == base`. Lets a host
    /// drop a shadow that has gone idle so the underlying param is free again.
    #[inline]
    pub fn is_unlayered(&self) -> bool {
        self.layers.is_empty()
    }
}

#[cfg(test)]
mod mod_param_tests {
    use super::{LayerKey, ModParam};

    const MOD_A: LayerKey = LayerKey(1);
    const MOD_B: LayerKey = LayerKey(2);

    #[test]
    fn final_value_is_base_when_no_layers() {
        let p = ModParam::new(0.7, 0.0, 2.0);
        assert_eq!(p.final_value(), 0.7);
        assert_eq!(p.base(), 0.7);
    }

    #[test]
    fn is_unlayered_tracks_layer_presence() {
        let mut p = ModParam::new(0.5, 0.0, 2.0);
        assert!(p.is_unlayered(), "fresh param has no layers");
        p.set_layer(MOD_A, 0.2);
        assert!(!p.is_unlayered());
        p.set_layer(LayerKey::AUTOMATION, 0.1);
        p.clear_mod_layers(); // drops MOD_A, keeps AUTOMATION
        assert!(!p.is_unlayered(), "automation layer still present");
        p.clear_layer(LayerKey::AUTOMATION);
        assert!(p.is_unlayered(), "back to no layers");
    }

    #[test]
    fn two_layers_sum_order_independent() {
        // Set A then B.
        let mut p1 = ModParam::new(0.5, 0.0, 2.0);
        p1.set_layer(MOD_A, 0.2);
        p1.set_layer(MOD_B, -0.1);
        // Set B then A.
        let mut p2 = ModParam::new(0.5, 0.0, 2.0);
        p2.set_layer(MOD_B, -0.1);
        p2.set_layer(MOD_A, 0.2);
        assert_eq!(p1.final_value(), p2.final_value());
        assert!((p1.final_value() - 0.6).abs() < 1e-6);
    }

    #[test]
    fn single_layer_does_not_drift_across_reads() {
        // The classic bug: reading the modulated value must not feed back
        // into the base. Repeated reads with a constant offset are stable.
        let mut p = ModParam::new(1.0, 0.0, 2.0);
        p.set_layer(MOD_A, 0.3);
        let first = p.final_value();
        for _ in 0..100 {
            assert_eq!(p.final_value(), first);
        }
        assert_eq!(p.base(), 1.0); // base never moved
        assert!((first - 1.3).abs() < 1e-6);
    }

    #[test]
    fn set_layer_updates_in_place_no_accumulation() {
        let mut p = ModParam::new(0.0, -10.0, 10.0);
        p.set_layer(MOD_A, 1.0);
        p.set_layer(MOD_A, 2.0);
        p.set_layer(MOD_A, 3.0);
        assert_eq!(p.final_value(), 3.0); // not 1+2+3
    }

    #[test]
    fn final_value_clamps_to_range() {
        let mut p = ModParam::new(0.9, 0.0, 1.0);
        p.set_layer(MOD_A, 0.5);
        assert_eq!(p.final_value(), 1.0); // 1.4 clamped
        p.set_layer(MOD_A, -2.0);
        assert_eq!(p.final_value(), 0.0); // -1.1 clamped
    }

    #[test]
    fn clear_layer_removes_contribution() {
        let mut p = ModParam::new(0.5, 0.0, 2.0);
        p.set_layer(MOD_A, 0.4);
        assert!((p.final_value() - 0.9).abs() < 1e-6);
        p.clear_layer(MOD_A);
        assert_eq!(p.final_value(), 0.5);
    }

    #[test]
    fn clear_mod_layers_keeps_automation() {
        let mut p = ModParam::new(0.5, 0.0, 2.0);
        p.set_layer(LayerKey::AUTOMATION, 0.1);
        p.set_layer(MOD_A, 0.2);
        p.set_layer(MOD_B, 0.3);
        p.clear_mod_layers();
        // Automation offset survives; mods gone.
        assert!((p.final_value() - 0.6).abs() < 1e-6);
    }

    #[test]
    fn dirty_tracks_changes() {
        let mut p = ModParam::new(0.5, 0.0, 2.0);
        assert!(p.take_dirty()); // dirty on construction
        assert!(!p.take_dirty()); // cleared
        p.set_base(0.5); // same value → not dirty
        assert!(!p.take_dirty());
        p.set_base(0.6); // changed → dirty
        assert!(p.take_dirty());
        p.set_layer(MOD_A, 0.1);
        assert!(p.take_dirty());
        p.set_layer(MOD_A, 0.1); // same offset → not dirty
        assert!(!p.take_dirty());
    }
}
