use bevy_ecs::prelude::*;
use bevy_reflect::prelude::*;
use tutti::NodeId;

/// A single note within a [`MidiSequence`].
#[cfg(feature = "midi")]
#[derive(Debug, Clone, Copy, PartialEq, Reflect)]
pub struct MidiSequenceNote {
    pub note: u8,
    pub velocity: u8,
    /// Start time in beats, relative to the sequence start.
    pub start: f64,
    /// Duration in beats.
    pub duration: f64,
}

/// Persistent MIDI sequence that fires note_on/note_off based on transport beat.
///
/// Ticked every frame by [`super::systems::midi_sequence_tick_system`].
///
/// Not `Reflect`: `target` wraps a foreign fundsp `NodeId`.
#[cfg(feature = "midi")]
#[derive(Component, Debug, Clone)]
pub struct MidiSequence {
    pub target: NodeId,
    pub notes: Vec<MidiSequenceNote>,
    pub start_beat: f64,
    pub duration_beats: f64,
    pub loop_enabled: bool,
}

/// Routes hardware MIDI input to an audio graph node via `MidiRoutingTable`.
/// The routing table is rebuilt automatically when these components change.
///
/// Not `Reflect`: `node_id` wraps a foreign fundsp `NodeId`.
#[cfg(feature = "midi")]
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct MidiReceiver {
    pub node_id: NodeId,
    /// MIDI channel filter. `None` = receive all channels.
    pub channel: Option<u8>,
}

/// One-shot trigger: connect to a MIDI input device by name (partial match).
#[cfg(feature = "midi-hardware")]
#[derive(Component, Debug, Clone, Reflect)]
#[reflect(Component, Clone)]
pub struct ConnectMidiDevice {
    pub name: String,
}

/// One-shot trigger: disconnect a specific MIDI input device by name.
#[cfg(feature = "midi-hardware")]
#[derive(Component, Debug, Clone, Reflect)]
#[reflect(Component, Clone)]
pub struct DisconnectMidiDevice {
    pub name: String,
}

/// Unlike `MidiReceiver`, routes all MIDI channels to one synth via
/// `table.fallback()` (standard MPE pattern).
///
/// Not `Reflect`: `node_id` wraps a foreign fundsp `NodeId`.
#[cfg(feature = "mpe")]
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct MpeReceiver {
    pub node_id: NodeId,
}

