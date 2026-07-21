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
pub mod convert;
pub mod input_source;
pub mod message;
pub mod mpe;
pub mod normalize;
pub mod note_id;
pub mod queue;
pub mod routing;
pub mod source;
pub mod sync;
pub mod target;
pub mod translate;
pub mod ump;
pub mod unit_id;

pub use clip_file::{read_clip_file, write_clip_file, ClipEvent, ClipFileError, ParsedClipFile};
pub use input_source::{MidiInputSource, NoMidiInput};
pub use message::{MidiMessage, NoteAttribute, PerNoteController, UnencodableMessage};
pub use mpe::{
    MpeChannelVoiceMap, MpeMode, MpeZone, MpeZoneConfig, NoteRotationAllocator,
    PitchBendSensitivity,
};
pub use normalize::normalize;
pub use note_id::{NoteId, PerNoteMap};
pub use queue::MidiQueue;
pub use routing::{MidiRoute, MidiRoutingSnapshot, MidiRoutingTable, RouteIterator};
pub use source::MidiSource;
pub use target::MidiTarget;
pub use translate::Midi1ToMidi2Translator;
pub use ump::{
    BarAccents, EndpointCapabilities, EndpointDiscoveryRequest, FunctionBlockDirection,
    FunctionBlocks, JrTimestamps, MidiEvent, MidiParseError, Protocol, UmpVersion,
};
pub use unit_id::MidiUnitId;
