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
mod message;

// Public, each for a stated reason:
//
// - `cc` / `ump` are large const-and-function namespaces (cc alone is
//   ~28 CCNumber consts). Flattened into the root they would drown it, and the
//   prefix is what makes `cc::MOD_WHEEL` readable.
// - `cc`, `mpe` and `translation` all have `pub mod` children that
//   callers reach (`cc::mapping`, `translation::wire`), and a
//   used two-level path is evidence the namespace is doing real work.
pub mod cc;
pub mod mpe;
pub mod translation;
pub mod ump;

pub use message::{
    ControllerNamespace, MidiMessage, NoteAttribute, PerNoteController, UnencodableMessage,
};
pub use mpe::{
    MpeChannelVoiceMap, MpeMode, MpeZone, MpeZoneConfig, NoteRotationAllocator, PerNoteBendRanges,
    ZoneInfo,
};
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
/// ([`MidiMessage`] via [`MidiMessage::from_ump`]) and the [`normalize`] seam. The wire
/// packet is `tutti_ump::Ump`, imported from there.
///
/// Deliberately *narrow* — the advanced surfaces (UMP-Stream endpoint
/// negotiation, Flex Data, RPN/NRPN translation state, MPE zone config) stay
/// explicit imports so a glob import doesn't flood scope. Reach for them by path when you need them.
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
/// ```
///
/// [`MidiIo`]: https://docs.rs/tutti-midi-hardware
pub mod prelude {
    pub use crate::{
        ControllerNamespace, MidiMessage, NoteAttribute, PerNoteController, Protocol, normalize,
    };
    pub use tutti_rt::ArcLend;
    pub use tutti_types::{Beat, BeatDuration, Bpm};
}
