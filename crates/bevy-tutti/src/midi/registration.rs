//! Keeping the [`MidiBus`](tutti_midi_runtime::MidiBus) in step with the graph.
//!
//! The bus routes by [`MidiUnitId`](tutti_midi_types::MidiUnitId), so every
//! MIDI-receiving node has to put its sender there before anything can address
//! it, and take it back out when the node goes. Previously only the first half
//! happened, open-coded in the soundfont spawner with a comment claiming it was
//! done "like every other MIDI-producing unit" — there was no other, and there
//! was no removal anywhere in the crate. Ids come from a monotonic counter and
//! are never reused, so the map grew without bound across spawn/despawn cycles.
//!
//! # Why a steady-state pass, not `Added<AudioNode>`
//!
//! An entity can carry `AudioNode` before its node is reachable, and a host may
//! add one from any schedule position. `Added` fires exactly once and is gone;
//! anything not resolvable on that frame would never be registered at all. A
//! pass over the not-yet-registered instead converges whenever the node turns
//! up, which is the same "skip and retry" the modulation resolver documents.
//!
//! Registration is idempotent regardless — [`MidiBus::insert`] keys on the
//! sender's own unit id — so the retry is cheap and a double-register is a
//! no-op rather than a duplicate.

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;

use tutti_core::AudioNode;
use tutti_midi_types::MidiUnitId;

use super::bus::MidiBusRes;
use super::target::MidiTargetResolver;
use crate::graph::{engine_ready, GraphReconcileSystems};

/// "This entity's MIDI sender is on the bus, under this id."
///
/// Carries the id purely so [`unregister_midi_sender`] can remove the right
/// entry after the node — and with it the port that knew the id — is already
/// gone. It is *not* an address to route by: resolution always re-derives from
/// the graph, because a `crossfade` can replace a node's port while keeping its
/// `NodeId`. Reading this to send MIDI would reintroduce exactly the staleness
/// the resolver exists to avoid.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct MidiRegistered {
    unit_id: MidiUnitId,
}

impl MidiRegistered {
    /// The bus id this entity's sender was filed under.
    pub fn unit_id(&self) -> MidiUnitId {
        self.unit_id
    }
}

/// Put every unregistered MIDI-capable node's sender on the bus.
///
/// Entities whose node is not yet reachable, or whose node type was never
/// registered with [`MidiTargetRegistry`](super::MidiTargetRegistry), are simply
/// skipped — they are retried next frame, and most never become MIDI targets at
/// all, so a log line here would be noise on every non-synth in the graph.
///
/// Every node of a registered type is registered, whether or not anything
/// currently addresses it. That is not over-eagerness: a synth's port *is* its
/// inbox, and being reachable is what lets a route or a preview arrive later
/// without a round-trip through this system. An entry costs one map slot.
pub fn register_midi_senders(
    mut commands: Commands,
    // `Option` for the same reason `unregister_midi_sender` below takes one: the
    // bus comes from `engine::build_into`, while the `engine_ready` gate reads
    // `AudioEngineState`. A host can declare the engine up without having run
    // the build — the crate's own tests do exactly that — and a hard `Res` makes
    // that a panicked schedule instead of a frame with nothing to register into.
    bus: Option<Res<MidiBusRes>>,
    resolver: MidiTargetResolver,
    pending: Query<Entity, (With<AudioNode>, Without<MidiRegistered>)>,
) {
    let Some(bus) = bus else {
        return;
    };
    for entity in pending.iter() {
        let Some(port) = resolver.port(entity) else {
            continue;
        };
        let unit_id = port.unit_id();
        bus.0.insert(port.sender());
        commands.entity(entity).insert(MidiRegistered { unit_id });
    }
}

/// Take a departed node's sender off the bus.
///
/// An observer rather than a `RemovedComponents` system, and that is
/// load-bearing: `On<Remove, AudioNode>` fires *before* the component value is
/// dropped, so [`MidiRegistered`] is still readable on the triggered entity. A
/// deferred pass would run after a despawn had taken the marker with it, and
/// the id needed to unregister would be gone — the same reasoning behind
/// [`reconcile_node_despawn`](crate::graph::reconcile_node_despawn).
///
/// Keyed on `AudioNode` removal rather than despawn, so a node pulled out of the
/// graph while its entity lives is unregistered too.
pub fn unregister_midi_sender(
    remove: On<Remove, AudioNode>,
    bus: Option<Res<MidiBusRes>>,
    registered: Query<&MidiRegistered>,
    mut commands: Commands,
) {
    let entity = remove.event_target();
    let Ok(marker) = registered.get(entity) else {
        return;
    };
    let Some(bus) = bus else { return };
    bus.0.remove(marker.unit_id);
    // The entity survives when only its `AudioNode` was removed; clearing the
    // marker lets it re-register if a node is added back.
    if let Ok(mut e) = commands.get_entity(entity) {
        e.remove::<MidiRegistered>();
    }
}

/// Keeps the bus in step with the graph: registers new MIDI nodes, unregisters
/// departed ones.
///
/// [`register_midi_senders`] runs after `Spawn` — a node must be in the graph
/// before it can be asked for its port — and before `Commit`, so a sender and
/// the node it belongs to reach the audio thread on the same frame.
pub struct MidiRegistrationPlugin;

impl Plugin for MidiRegistrationPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            register_midi_senders
                .after(GraphReconcileSystems::Spawn)
                .before(GraphReconcileSystems::Commit)
                // Resolution reads the audio graph, so it means nothing without
                // a running engine.
                .run_if(engine_ready),
        );
        // An observer, not a system: it must read `MidiRegistered` before a
        // despawn drops it. See [`unregister_midi_sender`].
        app.add_observer(unregister_midi_sender);
    }
}
