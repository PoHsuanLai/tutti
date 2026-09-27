//! The engine's MIDI as graph nodes (doc 013, rewrite item 5): the hardware
//! input, the clock and the hardware out the engine build inserts
//! ([`MidiEngineNodes`]), the MPE mode the input ingests in, and a
//! keyboard's way in ([`LiveMidiInput`]).
//!
//! MIDI reaches a node over event edges and nothing else. Which node hears
//! the hardware is declared with `MidiRouteRule`s (wired to the input
//! node's per-channel ports); a clip is a `MidiSourceInstall`; a keyboard, a
//! preview or an all-notes-off a host sends is a [`LiveMidiInput`] on the
//! entity it plays.

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;
use tutti_core::AudioNode;
use tutti_midi_runtime::{MidiInputControls, MidiQueueNode, MidiSender};

use crate::graph::{engine_ready, AudioGraphRes, EventFeeds, EventWiring, GraphDirty};

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
/// afterwards, which is what lets zone configuration live in a document.
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

/// Put on an entity (a synth, a hosted plugin) to send it MIDI from the
/// control thread: a keyboard, a preview, an all-notes-off. A
/// `MidiQueueNode` is inserted for it and wired to its event input, and the
/// entity gets [`LiveMidi`], the sender. Removing this (or despawning the
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
    /// The queue node feeding the entity.
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

/// Who feeds an entity through its [`LiveMidiInput`], in `EventFeeds`.
const LIVE: &str = "live midi";

/// Insert a queue node for every [`LiveMidiInput`] without one.
pub fn attach_live_midi(
    mut commands: Commands,
    wanted: Query<Entity, (With<LiveMidiInput>, Without<LiveMidi>)>,
    graph: Option<ResMut<AudioGraphRes>>,
    mut feeds: ResMut<EventFeeds>,
    mut dirty: ResMut<GraphDirty>,
) {
    let Some(mut graph) = graph else {
        return;
    };
    for entity in &wanted {
        let (node, sender) = graph.insert(MidiQueueNode::new());
        feeds.set(entity, LIVE, vec![node.into()]);
        commands.entity(entity).insert(LiveMidi { sender, node });
        dirty.0 = true;
    }
}

/// Remove the queue node of an entity whose [`LiveMidiInput`] went (or
/// that was despawned).
pub fn detach_live_midi(
    removed: On<Remove, LiveMidiInput>,
    live: Query<&LiveMidi>,
    graph: Option<ResMut<AudioGraphRes>>,
    mut feeds: ResMut<EventFeeds>,
    mut dirty: ResMut<GraphDirty>,
    mut commands: Commands,
) {
    let entity = removed.event_target();
    feeds.remove(entity, LIVE);
    if let (Ok(live), Some(mut graph)) = (live.get(entity), graph) {
        graph.remove(live.node);
        dirty.0 = true;
    }
    if let Ok(mut e) = commands.get_entity(entity) {
        e.try_remove::<LiveMidi>();
    }
}

/// [`LiveMidiInput`]'s systems: attach before the event wiring, so a new
/// queue is wired on the frame it is made.
pub struct LiveMidiPlugin;

impl Plugin for LiveMidiPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<EventFeeds>();
        app.init_resource::<GraphDirty>();
        app.add_observer(detach_live_midi);
        app.add_systems(
            Update,
            attach_live_midi.before(EventWiring).run_if(engine_ready),
        );
    }
}
