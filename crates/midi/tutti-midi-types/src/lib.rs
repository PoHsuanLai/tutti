//! Pure MIDI types for the Tutti audio engine.
//!
//! Canonical event type is [`MidiEvent`] — a packed 20-byte UMP event
//! (sample-accurate frame offset + up to four UMP words) carrying any MIDI
//! 1.0 / 2.0 / SysEx / utility message. Construction uses inherent
//! constructors ([`MidiEvent::note_on`], [`MidiEvent::cc`], ...). Decoding
//! goes through `midi2::UmpMessage::try_from(ev.data_words())`.
//!
//! Tutti-domain logic: the DAW routing table ([`MidiRoutingTable`]), [`mpe`]
//! (MIDI Polyphonic Expression, RP-053), [`sync`] (clock and MTC decoders), and
//! [`cc`] (CC→DAW-target mapping).
//!
//! SMF file parsing and MIDI-1 wire codec are provided by the re-exported
//! `midly` crate. Typed UMP messages are provided by re-exported `midi2`.
//!
//! # Example: one vocabulary, whatever the source protocol
//!
//! Two notes enter — one off a MIDI 1.0 DIN cable, one authored natively at
//! MIDI 2.0 width. [`normalize`] promotes the first to Channel Voice 2, so a
//! consumer downstream matches a single protocol and never needs a
//! Channel-Voice-1 arm.
//!
//! ```
//! use tutti_midi_types::prelude::*;
//! use tutti_midi_types::{convert, UmpMessageType};
//!
//! // From the wire: a MIDI 1.0 note-on, 7-bit velocity 100.
//! let from_din = MidiEvent::from_midi1_bytes(0, &[0x90, 60, 100])
//!     .expect("0x90 is a well-formed note-on");
//! assert_eq!(from_din.message_type(), UmpMessageType::ChannelVoice1);
//!
//! let promoted = normalize(&from_din);
//! assert_eq!(promoted.message_type(), UmpMessageType::ChannelVoice2);
//!
//! // Authored natively: 16 bits of velocity, no 7-bit original to be faithful to.
//! let authored = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0xABCD);
//! assert!(authored.message().is_note_on());
//! ```
//!
//! # The resolution boundary, and why the loss hides
//!
//! Widening is the spec's Min-Center-Max scaler
//! ([`convert::midi1_velocity_to_midi2`]), not a shift: `127` must reach full
//! scale, and `127 << 9` is `65024`, which is not. Read a promoted note through
//! [`MidiEvent::velocity_u16`] and the 7-bit original is recovered exactly, in
//! both directions:
//!
//! ```
//! # use tutti_midi_types::prelude::*;
//! # use tutti_midi_types::convert;
//! assert_eq!(convert::midi1_velocity_to_midi2(127), 65535);
//! assert_eq!(convert::midi1_velocity_to_midi2(64), 0x8000);
//!
//! // Every 7-bit value round-trips exactly.
//! for v in 0u8..128 {
//!     let wide = convert::midi1_velocity_to_midi2(v);
//!     assert_eq!(convert::midi2_velocity_to_midi1(wide), v);
//! }
//! ```
//!
//! **That exactness is the trap.** A path that narrows to 7 bits is
//! *self-consistently* lossy — it round-trips every value it can emit, so a
//! round-trip test passes and the loss never shows. It is only visible on a
//! value that did not start at 7 bits:
//!
//! ```
//! # use tutti_midi_types::prelude::*;
//! # use tutti_midi_types::convert;
//! let authored = 0xABCD_u16;
//! let via_7bit = convert::midi1_velocity_to_midi2(convert::midi2_velocity_to_midi1(authored));
//! assert_ne!(via_7bit, authored); // 43981 in, 43690 out — 128 codes survive of 65536
//! ```
//!
//! So [`MidiEvent::velocity_u7`] is for a MIDI 1.0 *destination* only. Reading a
//! velocity for any other purpose goes through [`MidiEvent::velocity_u16`],
//! which is lossless from either protocol.

// Third-party, re-exported whole so a consumer matching our version needs no
// dependency entry of its own.
pub use midi2;
pub use midly;

// The tutti-types vocabulary this crate's own API is spelled in: every UMP
// constructor takes a `MidiGroup` and a `MidiChannel`, the clip API positions in
// `Beat`/`BeatDuration`, a CC message needs `CCNumber`, and a routing table
// reaches the audio thread through `RtPublish`.
pub use tutti_types::{Beat, BeatDuration, CCNumber, MidiChannel, MidiGroup, RtPublish};

// No `///` on a `pub mod` line: it would shadow the module's own `//!` header
// and re-resolve that text's intra-doc links in this scope rather than the
// module's, breaking every link to a sibling item.
// Private: everything public in these is re-exported at the root below, so the
// module path would be a second name for a type that already has one.
mod clip_file;
mod message;
mod note_id;
mod routing;
mod traits;
mod unit_id;

// Public, each for a stated reason:
//
// - `cc` / `ci` / `ump` are large const-and-function namespaces (cc alone is
//   ~28 CCNumber consts). Flattened into the root they would drown it, and the
//   prefix is what makes `cc::MOD_WHEEL` readable.
// - `cc`, `mpe`, `sync`, `translation` and `ci` all have `pub mod` children that
//   callers reach (`cc::mapping`, `sync::clock`, `translation::scaling`), and a
//   used two-level path is evidence the namespace is doing real work.
pub mod cc;
pub mod ci;
pub mod mpe;
pub mod sync;
pub mod translation;
pub mod ump;

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
    PitchBendSensitivity, ZoneInfo,
};
pub use note_id::{NoteId, PerNoteMap};
// `MAX_TARGETS_PER_ROUTE` is the fan-out ceiling a `MidiRoute` is built against,
// so a caller sizing its own target list names the same bound.
pub use routing::{
    MidiRoute, MidiRoutingSnapshot, MidiRoutingTable, RouteIterator, MAX_TARGETS_PER_ROUTE,
};
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
/// use tutti_midi_types::{MidiChannel, MidiGroup};
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
/// use tutti_midi_types::{Beat, BeatDuration, MidiChannel, MidiGroup};
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
    pub use tutti_types::{Beat, BeatDuration, CCNumber, MidiChannel, MidiGroup, RtPublish};
}
