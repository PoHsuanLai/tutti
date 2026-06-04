//! Transport-driven MIDI sequence playback.
//!
//! A [`MidiSequence`] component holds a list of beat-positioned notes; the
//! per-frame [`midi_sequence_tick_system`] fires note_on/note_off through the
//! [`MidiBusRes`](crate::MidiBusRes) as the transport's beat position crosses
//! each note's bounds. [`MidiSequenceState`] tracks the currently-sounding
//! notes per entity and is auto-inserted by [`midi_sequence_setup_system`].

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;
use bevy_reflect::prelude::*;
use tutti_core::NodeId;

/// A single note within a [`MidiSequence`]. Runtime ECS mirror of
/// `dawai_types::SymbolicNote` (without the expression lanes — this form
/// only fires note_on/note_off). `pitch` is continuous semitones; the
/// firing system rounds to the nearest integer note number.
#[derive(Debug, Clone, Copy, PartialEq, Reflect)]
pub struct MidiSequenceNote {
    /// Pitch in semitones (60.0 = middle C). Fractional = microtonal.
    pub pitch: f64,
    /// Onset velocity, normalized `0.0..=1.0`.
    pub velocity: f32,
    /// Start time in beats, relative to the sequence start.
    pub start: f64,
    /// Duration in beats.
    pub duration: f64,
}

/// Persistent MIDI sequence that fires note_on/note_off based on transport beat.
///
/// Ticked every frame by [`midi_sequence_tick_system`].
///
/// Not `Reflect`: `target` wraps a foreign fundsp `NodeId`.
#[derive(Component, Debug, Clone)]
pub struct MidiSequence {
    pub target: NodeId,
    pub notes: Vec<MidiSequenceNote>,
    pub start_beat: f64,
    pub duration_beats: f64,
    pub loop_enabled: bool,
}

/// Tracks which notes are currently sounding for a [`MidiSequence`].
#[derive(Component, Default)]
pub struct MidiSequenceState {
    active_notes: std::collections::HashSet<u8>,
}

/// Auto-inserts [`MidiSequenceState`] on entities that have [`MidiSequence`]
/// but not yet a state component.
pub fn midi_sequence_setup_system(
    mut commands: Commands,
    query: Query<Entity, (With<MidiSequence>, Without<MidiSequenceState>)>,
) {
    for entity in query.iter() {
        commands.entity(entity).insert(MidiSequenceState::default());
    }
}

/// Ticks all [`MidiSequence`] entities, firing note_on/note_off based on
/// the transport's current beat position.
pub fn midi_sequence_tick_system(
    transport: Res<tutti_core::graph::TransportRes>,
    midi: Res<crate::MidiBusRes>,
    mut query: Query<(&MidiSequence, &mut MidiSequenceState)>,
) {
    if !transport.0.is_playing() {
        // All-notes-off when transport is not rolling
        for (seq, mut state) in query.iter_mut() {
            let unit_id = tutti_midi_types::MidiUnitId::new(seq.target.value());
            for note in state.active_notes.drain() {
                let event = note_off_event(note);
                midi.0.queue(unit_id, &[event]);
            }
        }
        return;
    }

    let beat = transport.0.current_beat();

    for (seq, mut state) in query.iter_mut() {
        let unit_id = tutti_midi_types::MidiUnitId::new(seq.target.value());
        let local_beat = if seq.loop_enabled && seq.duration_beats > 0.0 {
            let offset = beat - seq.start_beat;
            ((offset % seq.duration_beats) + seq.duration_beats) % seq.duration_beats
        } else {
            beat - seq.start_beat
        };

        // Outside range (non-looped)
        if !seq.loop_enabled && (local_beat < 0.0 || local_beat > seq.duration_beats) {
            for note in state.active_notes.drain() {
                let event = note_off_event(note);
                midi.0.queue(unit_id, &[event]);
            }
            continue;
        }

        // Determine which notes should be active at this beat. This
        // sequence path is integer-note-keyed; continuous pitch rounds to
        // the nearest semitone (the rich MPE path lives in dawai-model's
        // `flatten_notes`, not here).
        let mut should_be_active = std::collections::HashSet::new();
        for n in &seq.notes {
            if local_beat >= n.start && local_beat < n.start + n.duration {
                should_be_active.insert(seq_note_number(n));
            }
        }

        // Note-off for notes that ended
        for &note in &state.active_notes {
            if !should_be_active.contains(&note) {
                let event = note_off_event(note);
                midi.0.queue(unit_id, &[event]);
            }
        }

        // Note-on for newly active notes
        for n in &seq.notes {
            let note = seq_note_number(n);
            if should_be_active.contains(&note) && !state.active_notes.contains(&note) {
                let velocity = (n.velocity.clamp(0.0, 1.0) * 127.0).round() as u8;
                let event = note_on_event(note, velocity);
                midi.0.queue(unit_id, &[event]);
            }
        }

        state.active_notes = should_be_active;
    }
}

/// Nearest integer MIDI note number for a sequence note's continuous pitch.
fn seq_note_number(n: &MidiSequenceNote) -> u8 {
    n.pitch.round().clamp(0.0, 127.0) as u8
}

/// Channel-0 MIDI 2.0 note-on event with a 7-bit MIDI 1 velocity
/// (upconverted to the 16-bit MIDI 2 velocity range).
fn note_on_event(note: u8, velocity_midi1: u8) -> crate::MidiEvent {
    crate::MidiEvent::note_on(0, 0, note, (velocity_midi1 as u16) << 9)
}

fn note_off_event(note: u8) -> crate::MidiEvent {
    crate::MidiEvent::note_off(0, 0, note, 0)
}

/// Transport-beat-driven note firing for [`MidiSequence`] entities.
pub struct MidiSequencePlugin;

impl Plugin for MidiSequencePlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<MidiSequenceNote>();
        app.add_systems(
            Update,
            (midi_sequence_setup_system, midi_sequence_tick_system)
                .chain()
                .run_if(tutti_core::graph::engine_ready)
                .before(tutti_core::graph::GraphReconcileSystems::Commit),
        );
    }
}
