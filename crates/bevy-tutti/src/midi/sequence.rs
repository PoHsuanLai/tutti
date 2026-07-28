//! Beat-scheduled MIDI playback, declared in the ECS and clocked by the engine.
//!
//! A [`MidiSourceInstall`] names a target entity and the events to play at it.
//! [`rebuild`] compiles those into a
//! [`MidiClipSource`](tutti_midi_runtime::MidiClipSource) and installs it on the
//! target's port; the engine converts beats to sample offsets *on the audio
//! thread*, where the block being rendered is known.
//!
//! # Why the ECS cannot do the scheduling
//!
//! A per-frame system firing notes as the beat passes them is the obvious
//! design, and it is what the previous version did. It cannot be sample-accurate,
//! for three independent reasons:
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
//! Notes are still the common case, so they arrive through
//! [`from_notes`](MidiSourceInstall::from_notes) — a *constructor*, not a second
//! component. An app with its own richer note model builds the events itself and
//! never touches [`MidiNote`]. Either way `rebuild` reads one component, so
//! there is one way for playback to be declared.

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tutti_midi_runtime::{MidiClipSource, TimedMidiEvent};
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::{MidiMessage, NoteId};

use super::target::MidiTargetResolver;
use crate::graph::{engine_ready, AudioConfig, GraphReconcileSystems, TransportRes};

/// A note in the convenience form [`MidiSourceInstall::from_notes`] compiles.
///
/// Deliberately small: this is the *adapter's* note, not a DAW's. Anything it
/// cannot say — per-note controllers, expression lanes, articulation — is a
/// reason to build [`TimedMidiEvent`]s directly rather than to grow this.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MidiNote {
    /// Pitch in semitones, 60.0 = middle C. A fractional value is carried as a
    /// MIDI 2.0 `Pitch7_9` note attribute rather than rounded away.
    pub pitch: f64,
    /// Velocity, `0.0..=1.0`, widened to the full 16-bit MIDI 2.0 field.
    pub velocity: f32,
    /// Onset, in beats from the sequence start.
    pub start: f64,
    /// Length in beats.
    pub duration: f64,
    /// MIDI channel.
    pub channel: u8,
}

impl Default for MidiNote {
    fn default() -> Self {
        Self {
            pitch: 60.0,
            velocity: 0.8,
            start: 0.0,
            duration: 1.0,
            channel: 0,
        }
    }
}

impl MidiNote {
    /// A note at `pitch` starting on `start`, lasting `duration` beats.
    pub fn new(pitch: f64, start: f64, duration: f64) -> Self {
        Self {
            pitch,
            start,
            duration,
            ..Default::default()
        }
    }

    /// Set the velocity (`0.0..=1.0`).
    pub fn with_velocity(mut self, velocity: f32) -> Self {
        self.velocity = velocity;
        self
    }

    /// Set the MIDI channel.
    pub fn with_channel(mut self, channel: u8) -> Self {
        self.channel = channel;
        self
    }

    /// This note's on/off pair as timed events.
    ///
    /// Built through [`MidiMessage`] rather than `MidiEvent::note_on` so the
    /// velocity keeps its full 16 bits and a fractional pitch survives as a
    /// note attribute. `note_on_7bit` — what the old layer used — crushed an
    /// `f32` velocity to 7 bits before widening it back.
    fn to_events(self, id: NoteId) -> [TimedMidiEvent; 2] {
        let number = self.pitch.floor().clamp(0.0, 127.0) as u8;
        let attribute = pitch_attribute(self.pitch, number);
        let velocity = (self.velocity.clamp(0.0, 1.0) * u16::MAX as f32).round() as u16;

        let on = MidiMessage::NoteOn {
            frame_offset: 0,
            id,
            channel: self.channel,
            note: number,
            velocity,
            attribute,
        };
        let off = MidiMessage::NoteOff {
            frame_offset: 0,
            id,
            channel: self.channel,
            note: number,
            velocity: 0,
            attribute,
        };
        [
            TimedMidiEvent::new(self.start, encode(on)),
            TimedMidiEvent::new(self.start + self.duration, encode(off)),
        ]
    }
}

/// The fractional part of `pitch` as a MIDI 2.0 `Pitch7_9` attribute, or `None`
/// when the note lands on a semitone.
///
/// `None` rather than a zero attribute so an ordinary note stays an ordinary
/// note on the wire — a receiver that ignores attributes sees exactly what it
/// would have seen before.
fn pitch_attribute(pitch: f64, number: u8) -> Option<tutti_midi_types::NoteAttribute> {
    let cents = pitch - number as f64;
    if cents.abs() < f64::EPSILON {
        return None;
    }
    // Pitch7_9 is a 7.9 fixed-point *note number*: 9 fractional bits.
    let bits = ((number as f64 + cents) * 512.0)
        .round()
        .clamp(0.0, 65_535.0) as u16;
    Some(tutti_midi_types::NoteAttribute::Pitch7_9(
        tutti_midi_types::midi2::num::Fixed7_9::from_bits(bits),
    ))
}

/// Encode a semantic message, falling back to a no-op it cannot.
///
/// `TryFrom` only fails for hand-built controller/bank values with no MIDI-2
/// encoding, which the shapes above never produce — but a panic in a note
/// compiler would be a poor trade for that.
fn encode(msg: MidiMessage) -> MidiEvent {
    MidiEvent::try_from(msg).unwrap_or_else(|_| MidiEvent::noop())
}

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
    /// [`MidiTargetRegistry`](super::MidiTargetRegistry), so it must carry an
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

    /// Play `notes` at `target`, compiled to note-on/note-off pairs.
    ///
    /// Each note gets a distinct [`NoteId`], so two notes of the same pitch may
    /// overlap without the second's note-off cutting the first — the per-note
    /// identity MIDI 2.0 added for exactly this.
    pub fn from_notes(target: Entity, notes: &[MidiNote]) -> Self {
        let mut events = Vec::with_capacity(notes.len() * 2);
        for (i, note) in notes.iter().enumerate() {
            let id = NoteId::from_raw(i as u32);
            events.extend(note.to_events(id));
        }
        Self { target, events }
    }
}

/// The targets [`rebuild`] currently has a source installed on.
///
/// Kept because the removal of a `MidiSourceInstall` says nothing about *which*
/// target lost it — the component is gone by the time we look, and its `target`
/// with it. Remembering what we installed is what lets a target be cleared when
/// the last install naming it goes away.
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
    transport: Res<TransportRes>,
    config: Res<AudioConfig>,
) {
    let dirty = !changed.is_empty() || !removed.is_empty();
    // Draining is what marks this frame's removals as seen, so it happens
    // whether or not a rebuild follows.
    removed.clear();
    if !dirty {
        return;
    }

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

        // `timeline()` rather than a hand-rolled `Arc::new(transport.0.clone())`
        // — the accessor is where the per-frame/per-block seam is named, and
        // where "the clone shares state, it is not a snapshot" is written down.
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
        sender.queue(&[MidiEvent::cc(0, channel, 123, 0)]);
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
                .after(super::registration::register_midi_senders)
                .before(GraphReconcileSystems::Commit)
                .run_if(engine_ready),
        );
    }
}
