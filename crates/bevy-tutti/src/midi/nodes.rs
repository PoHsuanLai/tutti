//! The engine's MIDI as graph nodes: the hardware input, the clock and the
//! hardware out the engine build inserts ([`MidiEngineNodes`]), the MPE mode
//! the input ingests in, and a keyboard's way in ([`LiveMidiInput`]).
//!
//! MIDI reaches a node over event edges and nothing else. Which node hears
//! the hardware is declared with `MidiRouteRule`s (wired to the input
//! node's per-channel ports); a clip is a `MidiSourceInstall`; a keyboard, a
//! preview or an all-notes-off a host sends is a [`LiveMidiInput`] on the
//! entity it plays.

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;
use tutti_core::AudioNode;
use tutti_midi_runtime::{MidiInputControls, MidiSender};

use crate::graph::{engine_ready, AudioGraphRes, EventWiring};

/// The MIDI nodes the engine build inserts, each bound to an entity like any
/// node:
///
/// - `input`: the hardware input (`MidiInputNode`), one event output per
///   channel and one for channelless messages. `MidiRouteRule`s wire its
///   ports; nothing else needs to.
/// - `clock`: outbound Beat Clock and MTC (`ClockNode`), wired to
///   `hardware_out`. Configure it through [`ClockMasterRes`](super::ClockMasterRes).
/// - `hardware_out`: a `MidiOutNode` the hardware pump drains to the OS.
///   Declare `EventSources` on it (a clip, a plugin's MIDI out) to send
///   them to external gear, beside the clock.
///
/// Inserted by [`build_into`](crate::engine::build_into) with the `midi`
/// feature; absent when the engine did not build.
#[derive(Resource, Clone, Copy, Debug)]
pub struct MidiEngineNodes {
    /// The hardware input's entity.
    pub input: Entity,
    /// Its node, whose ports the route rules wire.
    pub input_node: AudioNode,
    /// The clock's entity.
    pub clock: Entity,
    /// The hardware out's entity.
    pub hardware_out: Entity,
}

/// Configures the MPE mode the engine's MIDI input ingests in. Insert this
/// *before* the engine builds to override the default.
///
/// Default is [`MpeMode::Disabled`](tutti_midi_types::MpeMode) — apps that want
/// MPE flip it to `LowerZone` / `UpperZone` / `DualZone` /
/// `SingleChannelRotation`. Read once, at build; [`MpeModeRes`] changes it
/// afterwards.
#[derive(Resource, Debug, Clone)]
pub struct MpeModeConfig(pub tutti_midi_types::MpeMode);

impl Default for MpeModeConfig {
    fn default() -> Self {
        Self(tutti_midi_types::MpeMode::Disabled)
    }
}

/// The live handle for changing the MPE mode **after** the engine is built:
/// the MIDI input node's controls.
///
/// [`MpeModeConfig`] is the build-time seed; this is how the mode changes
/// afterwards.
/// Inserted by [`build_into`](crate::engine::build_into), so it is absent
/// when the engine failed or is disabled — hold it as `Option<Res<_>>`.
///
/// A mode change resets MPE voice allocation, so drive it from a change
/// detector rather than each frame (setting the mode in force changes
/// nothing).
#[derive(Resource, Clone)]
pub struct MpeModeRes(pub MidiInputControls);

impl std::ops::Deref for MpeModeRes {
    type Target = MidiInputControls;
    fn deref(&self) -> &MidiInputControls {
        &self.0
    }
}

/// Marks an entity (a synth, a hosted plugin) that the host sends MIDI to
/// from the control thread: a keyboard, a preview, an all-notes-off. The
/// session's live input (a `MidiQueueNode` wired to its event input) is
/// made for it, and the entity gets [`LiveMidi`], the sender. Removing this (or despawning the
/// entity) removes the node.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct LiveMidiInput;

/// The sender into an entity's [`LiveMidiInput`] queue: what is queued plays
/// from the next block, on its frame offset clamped into the block. Cheap to
/// clone.
#[derive(Component, Clone, Debug)]
pub struct LiveMidi {
    sender: MidiSender,
    node: AudioNode,
}

impl LiveMidi {
    /// Returns the queue node feeding the entity.
    pub fn node(&self) -> AudioNode {
        self.node
    }
}

impl std::ops::Deref for LiveMidi {
    type Target = MidiSender;
    fn deref(&self) -> &MidiSender {
        &self.sender
    }
}

/// Gives every [`LiveMidiInput`] without one the session's live input on
/// the entity's node (`tutti_session::Midi::live_input`: a queue node fed
/// into its event input); one whose node is not in yet waits for it, and
/// one whose node takes no MIDI is warned of and tried again when it is
/// rebound. It runs only once the graph exists (the plugin's `run_if`), so
/// one added before that is attached when it arrives. The
/// queue is re-aimed (`Midi::retarget`) at the node an entity is bound to
/// whenever its `AudioNode` changes: a node bound in place of the old one,
/// and one bound after the old node was removed (whose feeds went with it,
/// `Graph::remove`).
#[allow(clippy::type_complexity)]
pub fn attach_live_midi(
    mut commands: Commands,
    wanted: Query<
        (Entity, &AudioNode),
        (
            With<LiveMidiInput>,
            Without<LiveMidi>,
            Or<(Added<LiveMidiInput>, Changed<AudioNode>)>,
        ),
    >,
    rebound: Query<(Entity, &AudioNode, &LiveMidi), (With<LiveMidiInput>, Changed<AudioNode>)>,
    mut graph: ResMut<AudioGraphRes>,
) {
    let mut midi = graph.midi();
    for (entity, &sink) in &wanted {
        match midi.live_input(sink) {
            Ok(live) => {
                commands.entity(entity).insert(LiveMidi {
                    sender: live.sender,
                    node: live.node,
                });
            }
            Err(e) => tracing::warn!("live MIDI for {entity:?}: {e}"),
        }
    }
    // Derived at the point of use: an unchanged feed (and the edit flag) is
    // left alone.
    for (entity, &sink, live) in &rebound {
        if let Err(e) = midi.retarget(live.node, sink) {
            tracing::warn!("live MIDI for {entity:?} not re-aimed: {e}");
        }
    }
}

/// Removes the queue node of an entity whose [`LiveMidiInput`] went (or
/// that was despawned).
pub fn detach_live_midi(
    removed: On<Remove, LiveMidiInput>,
    live: Query<&LiveMidi>,
    graph: Option<ResMut<AudioGraphRes>>,
    mut commands: Commands,
) {
    let entity = removed.event_target();
    // Removing the queue node takes it out of every feed it is in.
    if let (Ok(live), Some(mut graph)) = (live.get(entity), graph) {
        graph.remove(live.node);
    }
    if let Ok(mut e) = commands.get_entity(entity) {
        e.try_remove::<LiveMidi>();
    }
}

/// Adds [`LiveMidiInput`]'s systems: [`attach_live_midi`] before the event
/// wiring, so a new queue is wired on the frame it is made, and the
/// [`detach_live_midi`] observer. Part of
/// [`TuttiMidiPlugin`](super::TuttiMidiPlugin).
pub struct LiveMidiPlugin;

impl Plugin for LiveMidiPlugin {
    fn build(&self, app: &mut App) {
        app.add_observer(detach_live_midi);
        app.add_systems(
            Update,
            attach_live_midi
                .before(EventWiring)
                // A hard gate, not an early return: a system that runs
                // consumes its change ticks, so a `LiveMidiInput` added
                // before the graph exists would never be seen again.
                .run_if(engine_ready)
                .run_if(resource_exists::<AudioGraphRes>),
        );
    }
}
