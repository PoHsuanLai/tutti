//! MIDI type re-exports for the audio graph.
//!
//! Pure types and traits live in [`tutti_midi_types`]. Runtime state (registry,
//! snapshot, routing table, MPE, CC mapping) lives in `tutti-midi-runtime`.
//! Hardware I/O lives in `tutti-midi-io`.
//!
//! Legacy MIDI-1 types (for SMF files and VST/CLAP bridges) come from
//! `midly::live` and `midly::MidiMessage`. UMP message decoding goes through
//! `midi2::UmpMessage`.
//!
//! [`AudioProcessor`]: crate::processor::AudioProcessor

pub use tutti_midi_types::{
    cc, input_source, midi2, midly, MidiInputSource, MidiQueue, MidiRoute, MidiRoutingSnapshot,
    MidiSource, MidiTarget, MidiUnitId, NoMidiInput,
};

pub use crate::processor::MidiProcessor;
