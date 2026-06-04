//! Component-driven MIDI routing-table reconciliation.
//!
//! [`MidiReceiver`] (and, under `mpe`, [`MpeReceiver`]) components declare which
//! audio-graph node each MIDI channel feeds. [`midi_routing_sync_system`]
//! rebuilds the engine's `MidiRoutingTable` whenever those components change and
//! stages the edit for the Commit phase (it sets `GraphDirty` rather than
//! committing inline, so the route-table flush coalesces with the fundsp net
//! flush in `AudioGraph::commit()`).

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;
use tutti_core::NodeId;

/// Routes hardware MIDI input to an audio graph node via `MidiRoutingTable`.
/// The routing table is rebuilt automatically when these components change.
///
/// Not `Reflect`: `node_id` wraps a foreign fundsp `NodeId`.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct MidiReceiver {
    pub node_id: NodeId,
    /// MIDI channel filter. `None` = receive all channels.
    pub channel: Option<u8>,
}

/// Unlike [`MidiReceiver`], routes all MIDI channels to one synth via
/// `table.fallback()` (standard MPE pattern).
///
/// Not `Reflect`: `node_id` wraps a foreign fundsp `NodeId`.
#[cfg(feature = "mpe")]
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct MpeReceiver {
    pub node_id: NodeId,
}

/// MPE-receiver views needed by [`midi_routing_sync_system`], grouped into a
/// single [`SystemParam`] so the system stays under clippy's argument limit.
///
/// [`SystemParam`]: bevy_ecs::system::SystemParam
#[cfg(feature = "mpe")]
#[derive(bevy_ecs::system::SystemParam)]
pub struct MpeReceiverQueries<'w, 's> {
    changed: Query<'w, 's, &'static MpeReceiver, Changed<MpeReceiver>>,
    all: Query<'w, 's, &'static MpeReceiver>,
    removed: RemovedComponents<'w, 's, MpeReceiver>,
}

#[cfg(feature = "mpe")]
impl MpeReceiverQueries<'_, '_> {
    /// Whether any MPE receiver was added, changed, or removed this frame.
    fn has_changes(&mut self) -> bool {
        !self.changed.is_empty() || self.removed.read().next().is_some()
    }
}

pub fn midi_routing_sync_system(
    mut graph: ResMut<tutti_core::graph::AudioGraphRes>,
    mut dirty: ResMut<tutti_core::graph::GraphDirty>,
    changed: Query<&MidiReceiver, Changed<MidiReceiver>>,
    all_receivers: Query<&MidiReceiver>,
    mut removed: RemovedComponents<MidiReceiver>,
    #[cfg(feature = "mpe")] mut mpe: MpeReceiverQueries,
) {
    #[allow(unused_mut)]
    let mut has_changes = !changed.is_empty() || removed.read().next().is_some();

    #[cfg(feature = "mpe")]
    {
        has_changes = has_changes || mpe.has_changes();
    }

    if !has_changes {
        return;
    }

    let table = graph.0.midi_route_mut();
    table.clear();
    for receiver in all_receivers.iter() {
        let unit_id = tutti_midi_types::MidiUnitId::new(receiver.node_id.value());
        if let Some(ch) = receiver.channel {
            table.channel(ch, unit_id);
        } else {
            table.fallback(unit_id);
        }
    }

    // MPE receivers route all channels to one synth via fallback
    #[cfg(feature = "mpe")]
    for mpe_recv in mpe.all.iter() {
        table.fallback(tutti_midi_types::MidiUnitId::new(mpe_recv.node_id.value()));
    }

    // The staged route-table edits are published by the Commit-phase
    // `commit_graph` — `AudioGraph::commit()` flushes both the fundsp net
    // and the MIDI routing snapshot in one step, so coalescing here is
    // equivalent to committing inline. This system is anchored before the
    // Commit phase.
    dirty.0 = true;
}

/// Rebuilds the engine MIDI routing table from [`MidiReceiver`] components.
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
