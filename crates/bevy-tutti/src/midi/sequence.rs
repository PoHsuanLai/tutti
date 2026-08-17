//! Beat-scheduled MIDI playback, declared in the ECS and clocked by the engine.
//!
//! A [`MidiSourceInstall`] names a target entity and the events to play at it.
//! [`rebuild`] compiles those into a
//! [`MidiClipSource`] and installs it on the
//! target's port; the engine converts beats to sample offsets *on the audio
//! thread*, where the block being rendered is known.
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
use std::sync::Arc;

use tutti_midi_runtime::{MidiClipSource, TimedMidiEvent};
use tutti_midi_types::ump::MidiEvent;

use super::endpoint::target::MidiTargetResolver;
use crate::graph::{engine_ready, AudioConfig, GraphReconcileSystems, TransportRes};
use tutti_midi_types::cc;
use tutti_midi_types::{MidiChannel, MidiGroup};

/// "Play these events at that entity's synth."
///
/// The events are absolute-beat positioned; the engine decides where in a block
/// each lands. Add or edit this component and [`rebuild`] reinstalls the source.
///
/// Several installs may name one target — they are merged, because a port holds
/// one installed source and a second `install` would replace the first.
#[derive(Component, Debug, Clone)]
pub struct MidiSourceInstall {
    /// The entity whose synth plays this. Resolved through
    /// [`MidiTargetRegistry`](super::endpoint::target::MidiTargetRegistry), so it must carry an
    /// `AudioNode` of a registered type.
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

/// The targets [`rebuild`] currently has a source installed on.
///
/// Kept because the removal of a `MidiSourceInstall` says nothing about *which*
/// target lost it — the component is gone by the time the rebuild runs, and its
/// `target` field with it. This record is what lets a target be cleared when the
/// last install naming it goes away.
#[derive(Resource, Default)]
pub struct InstalledMidiSources(HashSet<Entity>);

/// Recompile every changed install into a clip source and install it.
///
/// Runs only when an install changed or went away. Rebuilding is not free: a
/// fresh [`MidiClipSource`] carries a fresh cursor, so it restarts at the
/// transport's current beat — and a rebuild landing between a note-on and its
/// note-off would drop the note-off and leave the note sounding. Hence the
/// all-notes-off below, and hence not rebuilding every frame.
pub fn rebuild(
    installs: Query<&MidiSourceInstall>,
    changed: Query<Entity, Changed<MidiSourceInstall>>,
    mut removed: RemovedComponents<MidiSourceInstall>,
    mut installed: ResMut<InstalledMidiSources>,
    resolver: MidiTargetResolver,
    // Both `engine::build_into`'s, and `engine_ready` covers neither — it reads
    // `AudioEngineState`, which a host can insert alone.
    transport: Option<Res<TransportRes>>,
    config: Option<Res<AudioConfig>>,
) {
    let dirty = !changed.is_empty() || !removed.is_empty();
    // Draining is what marks this frame's removals as seen, so it happens
    // whether or not a rebuild follows.
    removed.clear();
    if !dirty {
        return;
    }
    // After the drain, so a frame with no engine still marks removals seen.
    let (Some(transport), Some(config)) = (transport, config) else {
        return;
    };

    // Group by target first: a port holds *one* installed source, so two
    // installs on one synth have to become one merged clip. Installing each in
    // turn would silently leave only the last.
    let mut by_target: HashMap<Entity, Vec<TimedMidiEvent>> = HashMap::new();
    for install in installs.iter() {
        by_target
            .entry(install.target)
            .or_default()
            .extend(install.events.iter().copied());
    }

    // Targets that had a source but no longer have any install naming them.
    let orphaned: Vec<Entity> = installed
        .0
        .iter()
        .copied()
        .filter(|t| !by_target.contains_key(t))
        .collect();
    for target in orphaned {
        if let Some(port) = resolver.port(target) {
            all_notes_off(port);
            port.clear();
        }
        installed.0.remove(&target);
    }

    for (target, events) in by_target {
        let Some(port) = resolver.port(target) else {
            // No node yet, or its type was never registered — retried next
            // rebuild, same as any unresolvable target.
            continue;
        };

        // Silence anything the outgoing source had sounding: its note-offs are
        // about to be replaced by a clip whose cursor starts at the current
        // beat, so they would never be delivered.
        all_notes_off(port);

        // `timeline()` rather than a hand-rolled `Arc::new(transport.0.clone())`:
        // the accessor is where the per-frame/per-block seam is named, and where
        // "the clone shares state, it is not a snapshot" is written down.
        port.install(Arc::new(MidiClipSource::new(
            port.unit_id(),
            events,
            transport.timeline(),
            config.sample_rate,
        )));
        installed.0.insert(target);
    }
}

/// Send an all-notes-off on every channel to a port's own mailbox.
///
/// CC 123 rather than 128 individual note-offs: it is one message per channel,
/// and every synth that receives MIDI honours it.
fn all_notes_off(port: &tutti_midi_runtime::MidiInPort) {
    let sender = port.sender();
    for channel in 0..16u8 {
        sender.queue(&[MidiEvent::cc(
            MidiGroup::FIRST,
            MidiChannel::new(channel),
            cc::ALL_NOTES_OFF,
            0,
        )]);
    }
}

/// Compiles [`MidiSourceInstall`]s into installed clip sources.
pub struct MidiSequencePlugin;

impl Plugin for MidiSequencePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<InstalledMidiSources>();
        app.add_systems(
            Update,
            rebuild
                // After `Spawn` so a target added this frame is resolvable, and
                // before `Commit` so the install reaches the audio thread with
                // the node it belongs to. After registration, because a target
                // resolves through the same registry that populates the bus.
                .after(GraphReconcileSystems::Spawn)
                .after(super::endpoint::registration::register_midi_senders)
                .before(GraphReconcileSystems::Commit)
                .run_if(engine_ready),
        );
    }
}
