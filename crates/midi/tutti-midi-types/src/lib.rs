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
pub use tutti_types;

pub mod cc;
/// MIDI Capability Inquiry (MIDI-CI, M2-101) — Discovery, Profile Configuration,
/// and Property Exchange over Universal SysEx, transported by the SysEx7
/// fragmenter. Hand-rolled (midi2's `ci` module is a WIP stub).
pub mod ci;
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
    read_clip_file, write_clip_file, write_clip_file_from_beats, write_clip_file_with_header,
    ClipEvent, ClipFileError, ClipHeader, ClipNote, ParsedClipFile, CLIP_FILE_MAGIC,
};
pub use message::{
    ControllerNamespace, MidiMessage, NoteAttribute, PerNoteController, UnencodableMessage,
};
pub use mpe::{
    MpeChannelVoiceMap, MpeMode, MpeZone, MpeZoneConfig, NoteRotationAllocator,
    PitchBendSensitivity,
};
pub use note_id::{NoteId, PerNoteMap};
pub use routing::{MidiRoute, MidiRoutingSnapshot, MidiRoutingTable, RouteIterator};
pub use traits::{MidiIn, MidiOut, MidiRouter, MidiUnitIn};
pub use translation::{normalize, Midi1ToMidi2Translator, MidiParseError};
pub use ump::{
    Alteration, BarAccents, ChordBass, ChordName, ChordSharpsFlats, ChordType,
    EndpointCapabilities, EndpointDiscoveryRequest, FlexTextKind, FunctionBlockDirection,
    FunctionBlockDiscoveryRequest, FunctionBlocks, JrTimestamps, KeySharpsFlats, MidiEvent,
    Protocol, Tonic, UmpMessageType, UmpVersion, ALL_FUNCTION_BLOCKS,
};
pub use unit_id::MidiUnitId;

/// The common MIDI-types surface, for `use tutti_midi_types::prelude::*;`.
///
/// Pulls in what building and decoding MIDI needs: the wire event
/// ([`MidiEvent`]) and its decoded view ([`MidiMessage`] via
/// [`MidiEvent::message`]), per-note identity ([`NoteId`]), the unit id, the
/// [`normalize`] seam, and the MIDI 2.0 Clip File codec (beat-domain
/// [`write_clip_file_from_beats`] / [`read_clip_file`] / [`ParsedClipFile`]).
///
/// Deliberately *narrow* — the advanced surfaces (UMP-Stream endpoint
/// negotiation, Flex Data, RPN/NRPN translation state, MPE zone config, the sync
/// decoders, the raw scaling `convert` module) stay explicit imports so a glob
/// import doesn't flood scope. Reach for them by path when you need them.
///
/// For hardware and file I/O on top of these types, use
/// `tutti_midi_hardware::prelude::*`, which re-exports this prelude plus [`MidiIo`]
/// and the runtime delivery types.
///
/// ```
/// use tutti_midi_types::prelude::*;
///
/// // Build a note, decode it back — no midi2 imports, no width juggling.
/// let ev = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000);
/// let msg = ev.message();
/// assert!(msg.is_note_on());
/// assert_eq!(msg.note(), Some(60));
///
/// // Round-trip a phrase through a MIDI 2.0 Clip File, in beats.
/// let bytes = write_clip_file_from_beats(96, [(Beat(0.0), ev)]);
/// let clip = read_clip_file(&bytes).unwrap();
/// assert_eq!(clip.timed().count(), 1);
///
/// // Velocity survives at full MIDI 2.0 width — the point of the format.
/// let (_, first) = clip.timed().next().unwrap();
/// assert_eq!(first.velocity_u16(), Some(0x8000));
/// ```
///
/// A clip written with a [`ClipHeader`] declares its own tempo and time
/// signature (M2-116 §7.1.1/§7.1.2), so an importer can place it in real time
/// rather than assuming the project's:
///
/// ```
/// use tutti_midi_types::{
///     read_clip_file, write_clip_file_with_header, ClipEvent, ClipHeader, MidiEvent,
///     CLIP_FILE_MAGIC,
/// };
/// use tutti_midi_types::tutti_types::{MidiChannel, MidiGroup};
///
/// let bytes = write_clip_file_with_header(
///     480,
///     ClipHeader { tempo_bpm: 174.0, time_signature: (7, 8) },
///     &[ClipEvent::new(0, MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000))],
/// );
///
/// // Self-identifying: a file can be recognised before it is parsed.
/// assert_eq!(&bytes[..8], &CLIP_FILE_MAGIC);
///
/// let clip = read_clip_file(&bytes).unwrap();
/// assert_eq!(clip.time_signature(), Some((7, 8)));
/// assert!((clip.tempo_bpm().unwrap() - 174.0).abs() < 0.05);
/// ```
///
/// [`ParsedClipFile::notes`] pairs the event stream into whole notes, so an
/// importer gets durations without reimplementing note matching — and without
/// narrowing velocity to 7 bits on the way:
///
/// ```
/// use tutti_midi_types::{read_clip_file, write_clip_file_from_beats, MidiEvent};
/// use tutti_midi_types::tutti_types::{Beat, BeatDuration, MidiChannel, MidiGroup};
///
/// let bytes = write_clip_file_from_beats(96, [
///     (Beat(0.0), MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0xABCD)),
///     (Beat(1.5), MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0)),
/// ]);
///
/// let notes = read_clip_file(&bytes).unwrap().notes();
/// assert_eq!(notes.len(), 1);
/// assert_eq!(notes[0].duration_beats, BeatDuration(1.5));
/// assert_eq!(notes[0].velocity, 0xABCD);
/// ```
///
/// [`MidiEvent::message`]: crate::MidiMessage
/// [`MidiIo`]: https://docs.rs/tutti-midi-hardware
pub mod prelude {
    pub use crate::{
        normalize, read_clip_file, write_clip_file, write_clip_file_from_beats, ClipEvent,
        ClipFileError, ControllerNamespace, MidiEvent, MidiMessage, MidiUnitId, MidiUnitIn,
        NoteAttribute, NoteId, ParsedClipFile, PerNoteController, Protocol,
    };
    // The clip API positions events in `Beat` and measures them in
    // `BeatDuration`, so a caller of `write_clip_file_from_beats` needs both
    // names to say anything at all.
    //
    // `MidiGroup` and `MidiChannel` are here for the same reason, and more
    // strongly: every UMP constructor takes them, so without these two names a
    // caller cannot build a single event. They are the crate's addressing
    // vocabulary even though they are defined one crate down.
    pub use tutti_types::{Beat, BeatDuration, MidiChannel, MidiGroup};
}
