//! Component-driven MIDI routing-table reconciliation.
//!
//! [`MidiSink`] (and, under `mpe`, `MpeReceiver`) components declare which
//! audio-graph node each MIDI channel feeds. [`midi_routing_sync_system`]
//! rebuilds and publishes [`MidiRoutingRes`] whenever those components change.
//!
//! The table is *not* part of the audio graph: it maps a MIDI channel to a
//! destination unit's mailbox, and no fundsp edge is involved. It is owned here,
//! next to the hardware inputs it serves. Only the inbound device edge reads it
//! — `MidiPreBlock` for hardware in, `PluginMidiOut` for a plugin's MIDI-out
//! re-entering as if it were a device. Everything already bound to a unit (clip
//! playback, musical typing, previews) writes to that unit's `MidiInPort`
//! directly and never consults a route.

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;
use tutti_core::NodeId;
use tutti_midi_types::MidiRoutingTable;

/// The MIDI routing table: MIDI channel → destination unit mailbox.
///
/// Rebuilt from [`MidiSink`] / [`MpeReceiver`] components by
/// [`midi_routing_sync_system`]. Audio-thread readers hold the
/// `Arc<ArcSwap<MidiRoutingSnapshot>>` handed out by
/// [`MidiRoutingTable::snapshot_arc`].
///
/// Deliberately not [`Default`]: the table must be the *same* instance whose
/// snapshot the RT `MidiPreBlock` was built with. It only ever arrives via the
/// engine's `PendingMidi` handoff — a default-initialised one would publish
/// routes nothing reads, silently dropping all hardware MIDI.
#[derive(Resource)]
pub struct MidiRoutingRes(pub MidiRoutingTable);

/// Marks an audio-graph node as a **MIDI sink** — a destination that incoming
/// MIDI events flow into, for one channel or all. [`MidiRoutingRes`] is rebuilt
/// automatically whenever these components change.
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
    mut table: ResMut<MidiRoutingRes>,
    changed: Query<&MidiSink, Changed<MidiSink>>,
    all_receivers: Query<&MidiSink>,
    mut removed: RemovedComponents<MidiSink>,
    mut mpe: MpeReceiverQueries,
) {
    let has_changes = !changed.is_empty() || removed.read().next().is_some() || mpe.has_changes();

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

    table.0.set_routes(routes, fallback);
    table.0.commit();
}

/// Rebuilds the MIDI routing table from [`MidiSink`] components.
pub struct MidiRoutingPlugin;

impl Plugin for MidiRoutingPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            midi_routing_sync_system
                .run_if(tutti_core::ecs::engine_ready)
                // The table publishes itself, but stays anchored ahead of the
                // graph flush so a route and the node it points at still land in
                // the same frame — the ordering the old `GraphDirty` batching
                // gave us, now expressed as a schedule constraint rather than a
                // field on the graph.
                .before(tutti_core::ecs::GraphReconcileSystems::Commit),
        );
    }
}
