//! Beat-scheduled MIDI playback, declared in the ECS and clocked by the engine.
//!
//! A [`MidiSourceInstall`] names a target entity and the events to play at it.
//! [`rebuild`] compiles those into a [`MidiClipNode`] of the target's own,
//! wired to its MIDI event input ([`EventFeeds`]): a synth, a SoundFont
//! player or a plugin inserted as a graph node
//! ([`spawn_graph_node`](crate::graph::SpawnGraphNode), a plugin load). The
//! clip reads the block's `Env`, so its notes land on their frames through
//! seeks and loop wraps, in the same block, and an export forks it with the
//! target (doc 013 item 5). An edit replaces its events in place, ending the
//! notes the old events left sounding; so does removing the last install
//! naming the target. A target with no event input (an `AudioUnit` inserted
//! through `Legacy`) takes no MIDI, and is skipped.
//!
//! # Why the ECS cannot do the scheduling
//!
//! A per-frame system firing notes as the beat passes them is the obvious
//! design, and it cannot be sample-accurate, for three independent reasons:
//!
//! - `frame_offset` is meaningful only relative to the block that *pops* the
//!   event, and an off-thread producer cannot know which block that will be.
//! - The block size is not published off-thread; it comes from the device
//!   buffer, per callback.
//! - `Timeline::beat()` returns the beat at the **end of the last completed
//!   block** — the transport writes it back after rendering — so a frame-rate
//!   reader is behind by up to a block and sees it step, not flow.
//!
//! The engine solves the same problem for hardware input by carrying a timestamp
//! off-thread and converting it inside the block. This is that shape: the ECS
//! declares *when in musical time*, the audio thread decides *where in this
//! block*.
//!
//! # One component, one write path
//!
//! [`MidiSourceInstall`] holds `TimedMidiEvent`s, not notes, because a note model
//! cannot express most of what MIDI 2.0 carries — CC, pitch bend, per-note
//! controllers, program change. A notes-only component would have re-imposed a
//! MIDI-1.0 ceiling on a MIDI-2 engine.
//!
//! A note-with-duration record is *authoring* vocabulary, and this adapter is
//! not where it belongs. MIDI 2.0 defines no such record — the wire carries a
//! note-on and a note-off, and duration is only the gap between them — so a host
//! wanting one is inventing engine vocabulary, and a `from_notes` constructor
//! here would make bevy-tutti the accidental owner of a type every host needs.
//! Callers build `TimedMidiEvent`s, the vocabulary the engine already has.

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;
use std::collections::{HashMap, HashSet};

use tutti_midi_runtime::{MidiClipControls, MidiClipNode, TimedMidiEvent};

use crate::graph::{
    engine_ready, AudioGraphRes, EventFeeds, EventWiring, GraphDirty, GraphReconcileSystems,
};
use tutti_core::AudioNode;

/// "Play these events at that entity's synth."
///
/// The events are absolute-beat positioned; the engine decides where in a block
/// each lands. Add or edit this component and [`rebuild`] replaces the clip's
/// events.
///
/// Several installs may name one target — they are merged into its one clip.
#[derive(Component, Debug, Clone)]
pub struct MidiSourceInstall {
    /// The entity whose synth plays this: its `AudioNode` must have a MIDI
    /// event input (a node inserted with `spawn_graph_node`, a plugin).
    pub target: Entity,
    /// Absolute-beat positioned events, in any order — the engine sorts them.
    pub events: Vec<TimedMidiEvent>,
}

impl MidiSourceInstall {
    /// Play `events` at `target`.
    pub fn new(target: Entity, events: Vec<TimedMidiEvent>) -> Self {
        Self { target, events }
    }
}

/// The sequencer's name among a target's [`EventFeeds`].
const SEQUENCER: &str = "sequencer";

/// The clip node playing each target that has an event input: its node and
/// its controls. The node is the sequencer's, not an entity's: emptied when
/// the last install naming its target goes (which ends its notes), removed
/// when the target loses its node.
#[derive(Resource, Default)]
pub struct SequencedClips(HashMap<Entity, (AudioNode, MidiClipControls)>);

impl SequencedClips {
    /// The clip node playing `target`, if it has one.
    pub fn node(&self, target: Entity) -> Option<AudioNode> {
        self.0.get(&target).map(|(n, _)| *n)
    }
}

/// Recompile every changed install into its target's clip node.
///
/// Runs only when an install changed or went away, or a node was rebound. **Not**
/// when the device's rate moves (a restart, `restart_device`): a clip node
/// reads its block's `Env`, rate included.
#[allow(clippy::too_many_arguments)]
pub fn rebuild(
    installs: Query<&MidiSourceInstall>,
    changed: Query<Entity, Changed<MidiSourceInstall>>,
    // A target rebound to a new node (a crossfade, a respawn) keeps its clip:
    // the event wiring re-derives its edge. One rebound to a node with no
    // event input (a `Legacy` unit) loses its clip here.
    rebound: Query<(), Changed<AudioNode>>,
    mut removed: RemovedComponents<MidiSourceInstall>,
    mut clips: ResMut<SequencedClips>,
    mut feeds: ResMut<EventFeeds>,
    mut graph_dirty: ResMut<GraphDirty>,
    graph: Option<ResMut<AudioGraphRes>>,
    nodes: Query<&AudioNode>,
) {
    let dirty = !changed.is_empty() || !removed.is_empty() || !rebound.is_empty();
    // Draining is what marks this frame's removals as seen, so it happens
    // whether or not a rebuild follows.
    removed.clear();
    if !dirty {
        return;
    }
    let Some(mut graph) = graph else {
        return;
    };

    // Group by target first: one clip node plays a target, so two installs
    // on one synth become one merged clip.
    let mut by_target: HashMap<Entity, Vec<TimedMidiEvent>> = HashMap::new();
    for install in installs.iter() {
        by_target
            .entry(install.target)
            .or_default()
            .extend(install.events.iter().copied());
    }

    // Which targets can play a clip: those whose node has an event input.
    let playable: HashSet<Entity> = by_target
        .keys()
        .chain(clips.0.keys())
        .copied()
        .filter(|t| {
            nodes
                .get(*t)
                .is_ok_and(|&node| graph.node_event_inputs(node) > 0)
        })
        .collect();

    // A clip whose target lost its event input (no node, or a `Legacy` one):
    // its node goes, and there is nothing left for it to silence.
    let gone: Vec<Entity> = clips
        .0
        .keys()
        .copied()
        .filter(|t| !playable.contains(t))
        .collect();
    for target in gone {
        if let Some((node, _)) = clips.0.remove(&target) {
            graph.remove(node);
            graph_dirty.0 = true;
        }
        feeds.remove(target, SEQUENCER);
    }

    // A clip no install names any more plays nothing from the next block,
    // ending the notes it left sounding (as new events end the old ones').
    for (target, (_, controls)) in &clips.0 {
        if !by_target.contains_key(target) {
            controls.clear();
        }
    }

    for (target, events) in by_target {
        if !playable.contains(&target) {
            continue;
        }
        match clips.0.get(&target) {
            Some((_, controls)) => controls.set_events(events),
            None => {
                let (clip, controls) = graph.insert_node(MidiClipNode::new(events));
                clips.0.insert(target, (clip, controls));
                feeds.set(target, SEQUENCER, vec![clip.into()]);
                graph_dirty.0 = true;
            }
        }
    }
}

/// Compiles [`MidiSourceInstall`]s into clip nodes.
pub struct MidiSequencePlugin;

impl Plugin for MidiSequencePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SequencedClips>();
        app.init_resource::<EventFeeds>();
        app.add_systems(
            Update,
            rebuild
                // After `Spawn` so a target added this frame is resolvable, and
                // before `Commit` so the install reaches the audio thread with
                // the node it belongs to.
                .after(GraphReconcileSystems::Spawn)
                // Before the event wiring, so a new clip node is wired to its
                // target on the frame it is made.
                .before(EventWiring)
                .before(GraphReconcileSystems::Commit)
                .run_if(engine_ready),
        );
    }
}
