//! Per-node parameter epoch — a Bevy-side change counter for audio-graph
//! params that the tutti [`Net`](crate::dsp::Net) revision deliberately ignores.
//!
//! ## Why this exists
//!
//! [`AudioGraph::revision`](crate::AudioGraph::revision) bumps only on
//! `Net::commit()`, i.e. on **structural** graph edits (add/remove/rewire).
//! Parameter setters (`set_frequency`, `set_gain`, `set_q`, …) are plain
//! in-place writes — correct and real-time-safe for audio, no commit needed —
//! so they never move the revision. That's right for the audio thread, but it
//! means anything *derived from the rendered audio* (the spectral analysis
//! cache) cannot tell that an EQ cutoff was automated.
//!
//! [`NodeParamEpoch`] closes that gap on the ECS side: a monotonic version per
//! [`NodeId`], bumped whenever a parameter component on the entity bound to
//! that node changes. The spectral render cache folds the epochs of every node
//! in a target's upstream cone into its key, so a param tweak upstream of a
//! given target — and only that target — re-renders.
//!
//! The bump is driven by a generic `Changed<T>` sweep over the param
//! components, *not* by editing each `reconcile_*_params` system. Firing on the
//! component edit itself means this can never silently drift out of sync with
//! the reconciler list: a new effect's params are covered the moment its
//! component type is added to the filter tuple here.
//!
//! Only the core (track/bus-level) param sweep lives here. Leaf-specific bumps
//! (sampler / plugin / dsp-family) are added by their own feature-gated systems
//! in bevy-tutti, since `cfg` can't be applied to elements of an `Or<>` tuple.

use bevy_ecs::prelude::*;
use std::collections::HashMap;

use crate::graph::{AudioNode, Mute, Volume};
use crate::NodeId;

/// Monotonic per-node parameter version. Distinct from
/// [`AudioGraph::revision`](crate::AudioGraph::revision) (which tracks
/// structure); this tracks in-place param writes the revision skips.
///
/// `get` returns 0 for a node that has never had a param change, so a fresh
/// node and an unbumped node hash identically — correct, since neither has a
/// param edit to invalidate against.
#[derive(Resource, Debug, Default)]
pub struct NodeParamEpoch {
    map: HashMap<NodeId, u64>,
    /// Bumped on every `bump`, so a reader can cheaply detect "no param edits
    /// happened this frame" without diffing the map.
    generation: u64,
}

impl NodeParamEpoch {
    /// Increment the epoch for `node` (and the global generation).
    pub fn bump(&mut self, node: NodeId) {
        *self.map.entry(node).or_insert(0) += 1;
        self.generation += 1;
    }

    /// Current epoch for `node`; 0 if it has never been bumped.
    pub fn get(&self, node: NodeId) -> u64 {
        self.map.get(&node).copied().unwrap_or(0)
    }

    /// Global generation — total bumps across all nodes. Monotonic; useful as
    /// a one-comparison "did anything change?" gate.
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

/// Change filter for core params: track/bus level. Sampler and plugin params
/// are bumped by their own feature-gated systems (in bevy-tutti), since `cfg`
/// can't be applied to elements of an `Or<>` tuple — separate systems keep each
/// feature's components self-contained and the tuples within arity limits.
type CoreParamChanged = Or<(Changed<Volume>, Changed<Mute>)>;

/// Bump the epoch for every node whose core param component changed this frame.
pub fn bump_param_epoch_core(
    mut epoch: ResMut<NodeParamEpoch>,
    changed: Query<&AudioNode, CoreParamChanged>,
) {
    for node in changed.iter() {
        epoch.bump(node.0);
    }
}
