//! Getting a node into the graph, and swapping the unit behind one.
//!
//! Both operations queue a deferred world command, for the same reason:
//! [`Net::add`](tutti_core::dsp::Net::add) returns the `NodeId` *inside* the
//! command, so binding it to an entity cannot be done from outside. That is what
//! makes [`SpawnAudioNode`] irreducible rather than a convenience — it is the
//! only place the entity↔node binding can be formed.

use bevy_ecs::prelude::*;
use bevy_ecs::system::EntityCommands;

use tutti_core::dsp::AudioUnit;
use tutti_core::node::AudioNode;

use crate::graph::{AudioGraphRes, GraphDirty};

/// `Commands` extension that adds a unit to the graph and spawns an entity
/// with `AudioNode(id)` attached.
///
/// The graph mutation is queued as a deferred command and applies at the
/// next command-buffer flush — the returned `EntityCommands` lets the
/// caller chain further components onto the same entity in the usual fashion.
///
/// # The node arrives unwired
///
/// A fresh node has no edges and renders nothing. Declare what feeds it with
/// [`AudioSources`](crate::graph::AudioSources) on this entity, and declare what
/// reaches the speakers with [`MasterSources`](crate::graph::MasterSources):
///
/// ```rust,ignore
/// let osc = commands.spawn_audio_node(sine_hz::<f32>(440.0)).id();
/// let filt = commands
///     .spawn_audio_node(lowpass_hz(1000.0, 1.0))
///     .insert(AudioSources::from(osc))
///     .id();
/// commands.insert_resource(MasterSources::from(filt));
/// ```
///
/// The entity is bound to the node via [`AudioNode`] only. A host that needs to
/// distinguish node types (for a type-specific reconciler) attaches its own
/// marker component alongside.
pub trait SpawnAudioNode {
    /// Add `unit` to the graph and spawn an entity bound to it via [`AudioNode`].
    fn spawn_audio_node<U>(&mut self, unit: U) -> EntityCommands<'_>
    where
        U: AudioUnit + 'static;
}

impl<'w, 's> SpawnAudioNode for Commands<'w, 's> {
    fn spawn_audio_node<U>(&mut self, unit: U) -> EntityCommands<'_>
    where
        U: AudioUnit + 'static,
    {
        let entity = self.spawn_empty().id();
        self.queue(move |world: &mut World| {
            let id = match world.get_resource_mut::<AudioGraphRes>() {
                Some(mut graph) => graph.0.add(unit),
                None => {
                    bevy_log::warn!(
                        "spawn_audio_node: AudioGraphRes missing; entity {:?} left without AudioNode",
                        entity
                    );
                    return;
                }
            };
            // Mark the graph dirty so the per-frame commit system flushes
            // this addition along with whatever else mutated this frame.
            if let Some(mut dirty) = world.get_resource_mut::<GraphDirty>() {
                dirty.0 = true;
            }
            if let Ok(mut e) = world.get_entity_mut(entity) {
                e.insert(AudioNode(id));
            }
        });
        self.entity(entity)
    }
}

/// Crossfade-replace an entity's underlying graph node with `new_unit`.
///
/// Queues a deferred world command that:
///
/// 1. Looks up the entity's [`AudioNode(NodeId)`](AudioNode).
/// 2. Calls [`Net::crossfade`](tutti_core::dsp::Net::crossfade) with a 5 ms `Smooth` fade.
/// 3. Marks [`GraphDirty`] so the per-frame
///    [`commit_graph`](crate::graph::commit_graph) flushes.
///
/// The same `NodeId` survives the crossfade — connections to/from this node
/// stay valid, and any [`AudioSources`](crate::graph::AudioSources) naming this
/// entity keeps resolving. Callers don't need to update any other components.
///
/// Use this for parameter changes that aren't safe to mutate live (e.g. a
/// filter cutoff baked into the unit at construction, a sampler loop range
/// that requires re-priming the streamer). For RT-safe atomic changes, edit the
/// [`AudioParam`](crate::graph::AudioParam) component instead and let the
/// reconcile pipeline handle it.
///
/// If the entity has no `AudioNode` (e.g. it was despawned), or the
/// graph resource is missing, this is a no-op and logs a warning.
pub fn crossfade_audio_node(
    commands: &mut Commands<'_, '_>,
    entity: Entity,
    new_unit: Box<dyn AudioUnit>,
) {
    commands.queue(move |world: &mut World| {
        let Some(node) = world.get::<AudioNode>(entity).copied() else {
            bevy_log::warn!(
                "crossfade_audio_node: entity {:?} has no AudioNode; nothing to crossfade",
                entity
            );
            return;
        };
        let Some(mut graph) = world.get_resource_mut::<AudioGraphRes>() else {
            bevy_log::warn!(
                "crossfade_audio_node: AudioGraphRes missing; entity {:?} not crossfaded",
                entity
            );
            return;
        };
        graph
            .0
            .crossfade(node.0, tutti_core::Fade::Smooth, 0.005, new_unit);
        if let Some(mut dirty) = world.get_resource_mut::<GraphDirty>() {
            dirty.0 = true;
        }
    });
}
