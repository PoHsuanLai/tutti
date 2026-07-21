//! Pure MIDI types for the Tutti audio engine.
//!
//! Canonical event type is [`MidiEvent`] — a packed 20-byte UMP event
//! (sample-accurate frame offset + up to four UMP words) carrying any MIDI
//! 1.0 / 2.0 / SysEx / utility message. Construction uses inherent
//! constructors ([`MidiEvent::note_on`], [`MidiEvent::cc`], ...). Decoding
//! goes through `midi2::UmpMessage::try_from(ev.data_words())`.
//!
//! Tutti-domain logic lives in dedicated modules: [`routing`] (DAW routing
//! table), [`mpe`] (MIDI Polyphonic Expression, RP-053), [`sync`] (clock and
//! MTC decoders), [`cc`] (CC→DAW-target mapping).
//!
//! SMF file parsing and MIDI-1 wire codec are provided by the re-exported
//! `midly` crate. Typed UMP messages are provided by re-exported `midi2`.

pub use midi2;
pub use midly;

pub mod cc;
pub mod clip_file;
pub mod message;
pub mod mpe;
pub mod note_id;
pub mod routing;
pub mod sync;
pub mod traits;
/// Translation between the MIDI 1.0 and MIDI 2.0 Protocols (M2-104 §4.1 / App. D):
/// bit scaling, the wire codec, and the stateless / stateful promotions. See
/// [`translation`] for the layering.
pub mod translation;
pub mod ump;
pub mod unit_id;

/// Bit Scaling and Resolution (M2-104 §1.7 — MIDI 1.0 ↔ 2.0 widths + DSP-edge
/// f32). Kept reachable at the crate root as `tutti_midi_types::convert::*`, the
/// path many consumers import directly, while physically living under
/// [`translation`] with the rest of the MIDI-1↔2 boundary.
pub use translation::scaling as convert;

pub use clip_file::{
    read_clip_file, write_clip_file, write_clip_file_from_beats, ClipEvent, ClipFileError,
    ParsedClipFile,
};
pub use message::{MidiMessage, NoteAttribute, PerNoteController, UnencodableMessage};
pub use translation::{normalize, Midi1ToMidi2Translator, MidiParseError};
pub use mpe::{
    MpeChannelVoiceMap, MpeMode, MpeZone, MpeZoneConfig, NoteRotationAllocator,
    PitchBendSensitivity,
};
pub use note_id::{NoteId, PerNoteMap};
pub use routing::{MidiRoute, MidiRoutingSnapshot, MidiRoutingTable, RouteIterator};
pub use traits::{MidiInputSource, MidiQueue, MidiSource, MidiTarget, NoMidiInput};
pub use ump::{
    BarAccents, EndpointCapabilities, EndpointDiscoveryRequest, FunctionBlockDirection,
    FunctionBlocks, JrTimestamps, MidiEvent, Protocol, UmpVersion,
};
pub use unit_id::MidiUnitId;
