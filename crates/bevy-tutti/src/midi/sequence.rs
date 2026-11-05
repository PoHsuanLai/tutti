//! Beat-scheduled MIDI playback, declared in the ECS and clocked by the engine.
//!
//! A [`MidiSourceInstall`] names a target entity and the events to play at it.
//! [`rebuild`] compiles those into the session's clip for the target
//! ([`tutti_session::Midi::play_clip`]: a `MidiClipNode` wired to its MIDI
//! event input through the session's feeds): a synth, a SoundFont
//! player or a plugin inserted as a graph node
//! ([`spawn_audio_node`](crate::graph::SpawnAudioNode), a plugin load). The
//! clip reads the block's `Env`, so its notes land on their frames through
//! seeks and loop wraps, in the same block, and an export forks it with the
//! target. An edit replaces its events in place, ending the
//! notes the old events left sounding; so does removing the last install
//! naming the target. A target with no event input (an effect) takes no
//! MIDI, and is skipped.
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
//! controllers, program change.
//!
//! There is no note-with-duration constructor: MIDI 2.0 defines no such record
//! (the wire carries a note-on and a note-off, and duration is only the gap
//! between them). Callers build `TimedMidiEvent`s, the vocabulary the engine
//! already has.

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;
use std::collections::{HashMap, HashSet};

use tutti_midi_runtime::TimedMidiEvent;

use crate::graph::{AudioGraphRes, EventWiring, GraphReconcileSystems, engine_ready};
use tutti_core::AudioNode;

/// Declares events for the synth (or plugin) on `target` to play.
///
/// The events are absolute-beat positioned; the engine decides where in a block
/// each lands. Add or edit this component and [`rebuild`] replaces the clip's
/// events.
///
/// Several installs may name one target — they are merged into its one clip.
#[derive(Component, Debug, Clone)]
pub struct MidiSourceInstall {
    /// The entity whose synth plays this: its `AudioNode` must have a MIDI
    /// event input (a node inserted with `spawn_audio_node`, a plugin).
    pub target: Entity,
    /// Absolute-beat positioned events, in any order — the engine sorts them.
    pub events: Vec<TimedMidiEvent>,
}

impl MidiSourceInstall {
    /// Creates an install that plays `events` at `target`.
    pub fn new(target: Entity, events: Vec<TimedMidiEvent>) -> Self {
        Self { target, events }
    }
}

/// The clip node playing each target that has an event input: the
/// session's clip ([`tutti_session::Midi::play_clip`]), keyed by the
/// entity. The node is the sequencer's, not an entity's: emptied when the
/// last install naming its target goes (which ends its notes), removed
/// when the target loses its node.
#[derive(Resource, Default, Debug)]
pub struct SequencedClips {
    clips: HashMap<Entity, AudioNode>,
    /// Each install's target as [`rebuild`] last saw it: where a removed
    /// install, or one moved to another target, played.
    installs: HashMap<Entity, Entity>,
    /// Targets an edit reached that [`rebuild`] could not finish (no graph
    /// yet, or the session refused the clip): tried again on the next run.
    retry: HashSet<Entity>,
    /// Targets whose last attempt failed and was logged: a repeat is not
    /// logged again.
    failed: HashSet<Entity>,
}

impl SequencedClips {
    /// Returns the clip node playing `target`, if it has one.
    pub fn node(&self, target: Entity) -> Option<AudioNode> {
        self.clips.get(&target).copied()
    }
}

/// Recompiles the clips of the targets an edit reaches, through the
/// session's [`Midi`](tutti_session::Midi).
///
/// A target is reached when an install naming it (now, or before the
/// edit) was added, changed or removed, or when its own node was bound,
/// rebound or removed. Only those targets' installs are merged, sorted and
/// sent; every other clip is left alone. Removals are found by looking
/// (an install the last rebuild saw that is gone, a clip's target with no
/// node), not from removal messages, which a frame this system skips
/// (`engine_ready`) would lose. A target it cannot finish (there is no
/// [`AudioGraphRes`] yet, or the session refuses its clip) is kept and
/// tried again on the next run. **Not** when the device's rate
/// moves (a restart, `restart_device`): a clip node reads its block's
/// `Env`, rate included.
#[allow(clippy::too_many_arguments)]
pub fn rebuild(
    installs: Query<&MidiSourceInstall>,
    changed: Query<(Entity, &MidiSourceInstall), Changed<MidiSourceInstall>>,
    // A target rebound to a new node (a respawn) keeps its clip: its clip
    // is re-aimed at its current node below. One rebound to a node with no
    // event input loses its clip here.
    rebound: Query<Entity, Changed<AudioNode>>,
    mut clips: ResMut<SequencedClips>,
    graph: Option<ResMut<AudioGraphRes>>,
    nodes: Query<&AudioNode>,
) {
    let clips = &mut *clips;
    // The targets this frame's edits reach: an install gone, added or
    // changed (its target now and before), a clip's target left with no
    // node, a target (re)bound.
    let mut reached: HashSet<Entity> = std::mem::take(&mut clips.retry);
    clips.installs.retain(|&install, &mut target| {
        let kept = installs.contains(install);
        if !kept {
            reached.insert(target);
        }
        kept
    });
    for (install, now) in &changed {
        reached.insert(now.target);
        reached.extend(clips.installs.insert(install, now.target));
    }
    reached.extend(clips.clips.keys().filter(|&&t| !nodes.contains(t)));
    if !rebound.is_empty() {
        let targets: HashSet<Entity> = clips.installs.values().copied().collect();
        reached.extend(
            rebound
                .iter()
                .filter(|t| clips.clips.contains_key(t) || targets.contains(t)),
        );
    }
    if reached.is_empty() {
        return;
    }
    let Some(mut graph) = graph else {
        // The edits are consumed above; keep what they reached.
        clips.retry = reached;
        return;
    };

    // Group by target first: one clip node plays a target, so two installs
    // on one synth become one merged clip.
    let mut by_target: HashMap<Entity, Vec<TimedMidiEvent>> = HashMap::new();
    for install in installs.iter().filter(|i| reached.contains(&i.target)) {
        by_target
            .entry(install.target)
            .or_default()
            .extend(install.events.iter().copied());
    }

    // Which reached targets can play a clip, and on which node: those whose
    // node has an event input.
    let playable: HashMap<Entity, AudioNode> = reached
        .iter()
        .filter_map(|&t| {
            let &node = nodes.get(t).ok()?;
            (graph.node_event_inputs(node) > 0).then_some((t, node))
        })
        .collect();

    // A clip whose target lost its event input (no node, or one without):
    // its node goes (and with it, from every feed), and there is nothing
    // left for it to silence.
    for target in &reached {
        if playable.contains_key(target) {
            continue;
        }
        if let Some(clip) = clips.clips.remove(target) {
            graph.remove(clip);
        }
    }

    let session = graph.session();
    let mut midi = session.midi();
    // Targets whose clip node was removed behind the sequencer's back.
    let mut stale: Vec<Entity> = Vec::new();
    for (target, &sink) in &playable {
        let Some(&clip) = clips.clips.get(target) else {
            continue;
        };
        // Re-aimed on every rebuild of its target: a target whose
        // `AudioNode` was removed and re-inserted lost its old node's feeds
        // with that node (`Graph::remove`), and nothing else would restore
        // them. An unchanged feed (and the edit flag) is left alone.
        match midi.retarget(clip, sink) {
            Ok(()) => {
                clips.failed.remove(target);
            }
            // The clip node is gone: forget it, and play a fresh one below
            // if installs remain.
            Err(tutti_session::Error::NotSessionMidi(_)) => stale.push(*target),
            Err(e) => {
                if clips.failed.insert(*target) {
                    tracing::error!("sequencer: {target:?}'s clip not re-aimed: {e}");
                }
                clips.retry.insert(*target);
            }
        }
        // A clip no install names any more plays nothing from the next
        // block, ending the notes it left sounding (as new events end the
        // old ones').
        if !by_target.contains_key(target) {
            midi.stop_clip(sink);
        }
    }

    for target in stale {
        clips.clips.remove(&target);
    }

    for (target, events) in by_target {
        let Some(&sink) = playable.get(&target) else {
            continue;
        };
        match midi.play_clip(sink, events) {
            Ok(clip) => {
                clips.clips.insert(target, clip);
                clips.failed.remove(&target);
            }
            Err(e) => {
                if clips.failed.insert(target) {
                    tracing::error!("sequencer: {target:?} takes no clip: {e}");
                }
                clips.retry.insert(target);
            }
        }
    }
    let retry = &clips.retry;
    clips.failed.retain(|t| retry.contains(t));
}

/// Compiles [`MidiSourceInstall`]s into clip nodes.
#[derive(Debug)]
pub struct MidiSequencePlugin;

impl Plugin for MidiSequencePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SequencedClips>();
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
