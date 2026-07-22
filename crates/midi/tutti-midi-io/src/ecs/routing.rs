//! Component-driven MIDI routing-table reconciliation.
//!
//! [`MidiSink`] (and, under `mpe`, `MpeReceiver`) components declare which
//! audio-graph node each MIDI channel feeds. [`midi_routing_sync_system`]
//! rebuilds the engine's `MidiRoutingTable` whenever those components change and
//! stages the edit for the Commit phase (it sets `GraphDirty` rather than
//! committing inline, so the route-table flush coalesces with the fundsp net
//! flush in `AudioGraph::commit()`).

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;
use tutti_core::NodeId;

/// Marks an audio-graph node as a **MIDI sink** — a destination that incoming
/// MIDI events flow into, for one channel or all. The engine's `MidiRoutingTable`
/// is rebuilt automatically whenever these components change.
///
/// Distinct from `tutti_midi_runtime::MidiReceiver`, which is the lock-free inbox
/// *half* a node owns; this is the ECS-side *routing declaration* that points
/// events at that node.
///
/// Not `Reflect`: `node_id` wraps a foreign fundsp `NodeId`.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct MidiSink {
    pub node_id: NodeId,
    /// MIDI channel filter. `None` = receive all channels.
    pub channel: Option<u8>,
}

/// Unlike [`MidiSink`], routes all MIDI channels to one synth via
/// `table.fallback()` (standard MPE pattern).
///
/// Not `Reflect`: `node_id` wraps a foreign fundsp `NodeId`.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct MpeReceiver {
    pub node_id: NodeId,
}

/// MPE-receiver views needed by [`midi_routing_sync_system`], grouped into a
/// single [`SystemParam`] so the system stays under clippy's argument limit.
///
/// [`SystemParam`]: bevy_ecs::system::SystemParam
#[derive(bevy_ecs::system::SystemParam)]
pub struct MpeReceiverQueries<'w, 's> {
    changed: Query<'w, 's, &'static MpeReceiver, Changed<MpeReceiver>>,
    all: Query<'w, 's, &'static MpeReceiver>,
    removed: RemovedComponents<'w, 's, MpeReceiver>,
}

impl MpeReceiverQueries<'_, '_> {
    /// Whether any MPE receiver was added, changed, or removed this frame.
    fn has_changes(&mut self) -> bool {
        !self.changed.is_empty() || self.removed.read().next().is_some()
    }
}

pub fn midi_routing_sync_system(
    mut graph: ResMut<tutti_core::graph::AudioGraphRes>,
    mut dirty: ResMut<tutti_core::graph::GraphDirty>,
    changed: Query<&MidiSink, Changed<MidiSink>>,
    all_receivers: Query<&MidiSink>,
    mut removed: RemovedComponents<MidiSink>,
    mut mpe: MpeReceiverQueries,
) {
    let has_changes =
        !changed.is_empty() || removed.read().next().is_some() || mpe.has_changes();

    if !has_changes {
        return;
    }

    let mut routes = Vec::new();
    let mut fallback = None;
    for receiver in all_receivers.iter() {
        let unit_id = tutti_midi_types::MidiUnitId::new(receiver.node_id.value());
        match receiver.channel {
            Some(ch) => {
                routes.push(tutti_midi_types::MidiRoute::for_channel(ch).with_target(unit_id))
            }
            // Channel-less receivers route every channel via the fallback.
            None => fallback = Some(unit_id),
        }
    }

    // MPE receivers route all channels to one synth via the fallback.
    for mpe_recv in mpe.all.iter() {
        fallback = Some(tutti_midi_types::MidiUnitId::new(mpe_recv.node_id.value()));
    }

    graph.0.midi_route_mut().set_routes(routes, fallback);

    // The staged route-table edits are published by the Commit-phase
    // `commit_graph` — `AudioGraph::commit()` flushes both the fundsp net
    // and the MIDI routing snapshot in one step, so coalescing here is
    // equivalent to committing inline. This system is anchored before the
    // Commit phase.
    dirty.0 = true;
}

/// Rebuilds the engine MIDI routing table from [`MidiSink`] components.
pub struct MidiRoutingPlugin;

impl Plugin for MidiRoutingPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            midi_routing_sync_system
                .run_if(tutti_core::graph::engine_ready)
                .before(tutti_core::graph::GraphReconcileSystems::Commit),
        );
    }
}
