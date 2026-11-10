#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

// Third-party, re-exported whole so a consumer matching our version needs no
// dependency entry of its own.
pub use midi2;
pub use midly;

// The tutti-types vocabulary this crate's own API is spelled in: the clip API
// positions in `Beat`/`BeatDuration` and declares its tempo in `Bpm`, and state
// reaches the audio thread through `ArcLend`. The UMP constructors take
// `tutti_ump`'s `MidiGroup`, `MidiChannel` and `CCNumber`, which callers import
// from there.
pub use tutti_rt::ArcLend;
pub use tutti_types::{Beat, BeatDuration, Bpm};

// No `///` on a `pub mod` line: it would shadow the module's own `//!` header
// and re-resolve that text's intra-doc links in this scope rather than the
// module's, breaking every link to a sibling item.
// Private: everything public in these is re-exported at the root below, so the
// module path would be a second name for a type that already has one.
mod clip_file;
mod fixed;
mod message;
mod note_id;

// Public, each for a stated reason:
//
// - `cc` / `ump` are large const-and-function namespaces (cc alone is
//   ~28 CCNumber consts). Flattened into the root they would drown it, and the
//   prefix is what makes `cc::MOD_WHEEL` readable.
// - `cc`, `mpe`, `sync` and `translation` all have `pub mod` children that
//   callers reach (`cc::mapping`, `sync::clock`, `translation::wire`), and a
//   used two-level path is evidence the namespace is doing real work.
pub mod cc;
pub mod mpe;
pub mod sync;
pub mod translation;
pub mod ump;

pub use clip_file::{
    CLIP_FILE_MAGIC, ClipEvent, ClipFileError, ClipHeader, ClipNote, ParsedClipFile,
    write_clip_file, write_clip_file_from_beats, write_clip_file_with_header,
};
pub use fixed::{q7_9_to_f32, q7_25_to_f32};
pub use message::{
    ControllerNamespace, MidiMessage, NoteAttribute, PerNoteController, UnencodableMessage,
};
pub use mpe::{
    MpeChannelVoiceMap, MpeMode, MpeZone, MpeZoneConfig, NoteRotationAllocator, ZoneInfo,
};
pub use note_id::PerNoteMap;
pub use translation::{
    Midi1Messages, MidiParseError, from_midi1_bytes, normalize, parse_midi1_bytes, to_midi1_bytes,
    to_midi1_messages,
};
pub use ump::{
    Alteration, BarAccents, ChordBass, ChordName, ChordSharpsFlats, ChordType, FlexTextKind,
    KeySharpsFlats, Protocol, Tonic, flex_set_chord_name, flex_set_key_signature,
    flex_set_metronome, flex_set_tempo, flex_set_time_signature, is_sysex8_abort, jr_clock_value,
    jr_timestamp_value, sysex8_fragments, sysex8_header, sysex8_status,
};

/// The common MIDI-types surface, for `use tutti_midi_types::prelude::*;`.
///
/// Pulls in what building and decoding MIDI needs: the decoded view
/// ([`MidiMessage`] via [`MidiMessage::from_ump`]), the [`normalize`] seam, and the MIDI 2.0 Clip File codec
/// (beat-domain [`write_clip_file_from_beats`] / [`ParsedClipFile::from_bytes`] /
/// [`ParsedClipFile`]). The wire packet is `tutti_ump::Ump`, imported from there.
///
/// Deliberately *narrow* — the advanced surfaces (UMP-Stream endpoint
/// negotiation, Flex Data, RPN/NRPN translation state, MPE zone config, the sync
/// decoders) stay explicit imports so a glob
/// import doesn't flood scope. Reach for them by path when you need them.
///
/// For hardware and file I/O on top of these types, use
/// `tutti_midi_hardware::prelude::*`, which re-exports this prelude plus [`MidiIo`]
/// and the runtime delivery types.
///
/// ```
/// use tutti_midi_types::prelude::*;
/// use tutti_ump::{Address, Attribute, Message, MidiChannel, MidiGroup, NoteNumber, Ump, Voice2};
///
/// // Build a note, decode it back — no midi2 imports, no width juggling.
/// let ump = Ump::from(Message::Midi2(
///     Address { group: MidiGroup::FIRST, channel: MidiChannel::FIRST },
///     Voice2::NoteOn {
///         note: NoteNumber::new(60).unwrap(),
///         velocity: 0x8000,
///         attribute: Attribute::None,
///     },
/// ));
/// let msg = MidiMessage::from_ump(ump);
/// assert!(msg.is_note_on());
/// assert_eq!(msg.note(), Some(60));
///
/// // Round-trip a phrase through a MIDI 2.0 Clip File, in beats.
/// let bytes = write_clip_file_from_beats(96, [(Beat(0.0), ump)]);
/// let clip = ParsedClipFile::from_bytes(&bytes).unwrap();
/// assert_eq!(clip.iter().count(), 1);
///
/// // Velocity survives at full MIDI 2.0 width — the point of the format.
/// let (_, first) = clip.iter().next().unwrap();
/// assert_eq!(MidiMessage::from_ump(first).velocity(), Some(0x8000));
/// ```
///
/// A clip written with a [`ClipHeader`] declares its own tempo and time
/// signature (M2-116 §7.1.1/§7.1.2), so an importer can place it in real time
/// rather than assuming the project's:
///
/// ```
/// use tutti_fixtures::midi::note_on;
/// use tutti_midi_types::{
///     ParsedClipFile, write_clip_file_with_header, ClipEvent, ClipHeader,
///     CLIP_FILE_MAGIC,
/// };
/// use tutti_midi_types::Bpm;
/// use tutti_ump::{MidiChannel, MidiGroup};
///
/// let bytes = write_clip_file_with_header(
///     480,
///     ClipHeader { tempo_bpm: Bpm::of(174.0), time_signature: (7, 8) },
///     &[ClipEvent::new(0, note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000))],
/// );
///
/// // Self-identifying: a file can be recognised before it is parsed.
/// assert_eq!(&bytes[..8], &CLIP_FILE_MAGIC);
///
/// let clip = ParsedClipFile::from_bytes(&bytes).unwrap();
/// assert_eq!(clip.time_signature(), Some((7, 8)));
/// assert!(!clip.tempo_bpm().unwrap().differs_from(Bpm::of(174.0), 0.05));
/// ```
///
/// [`ParsedClipFile::notes`] pairs the event stream into whole notes, so an
/// importer gets durations without reimplementing note matching — and without
/// narrowing velocity to 7 bits on the way:
///
/// ```
/// use tutti_fixtures::midi::{note_off, note_on};
/// use tutti_midi_types::{ParsedClipFile, write_clip_file_from_beats};
/// use tutti_midi_types::{Beat, BeatDuration};
/// use tutti_ump::{MidiChannel, MidiGroup};
///
/// let bytes = write_clip_file_from_beats(96, [
///     (Beat(0.0), note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0xABCD)),
///     (Beat(1.5), note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0)),
/// ]);
///
/// let notes = ParsedClipFile::from_bytes(&bytes).unwrap().notes();
/// assert_eq!(notes.len(), 1);
/// assert_eq!(notes[0].duration_beats, BeatDuration(1.5));
/// assert_eq!(notes[0].velocity, 0xABCD);
/// ```
///
/// [`MidiIo`]: https://docs.rs/tutti-midi-hardware
pub mod prelude {
    pub use crate::{
        ClipEvent, ClipFileError, ControllerNamespace, MidiMessage, NoteAttribute, ParsedClipFile,
        PerNoteController, Protocol, normalize, write_clip_file, write_clip_file_from_beats,
    };
    pub use tutti_rt::ArcLend;
    pub use tutti_types::{Beat, BeatDuration, Bpm};
}
