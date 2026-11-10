#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]

// No `///` on a `pub mod` line: it would shadow the module's own `//!` header
// and re-resolve that text's intra-doc links in this scope rather than the
// module's, breaking every link to a sibling item.
// Private: everything public in it is re-exported at the root below, so the
// module path would be a second name for a type that already has one.
mod message;

// Public for `translation::wire` and its siblings, which callers reach by path.
pub mod translation;

pub use message::{
    ControllerNamespace, MidiMessage, NoteAttribute, PerNoteController, UnencodableMessage,
};
pub use translation::{
    Midi1Messages, MidiParseError, from_midi1_bytes, normalize, parse_midi1_bytes, to_midi1_bytes,
    to_midi1_messages,
};

/// The common MIDI-types surface, for `use tutti_midi_types::prelude::*;`.
///
/// The decoded view ([`MidiMessage`] via [`MidiMessage::from_ump`]) and the
/// [`normalize`] seam. The wire packet is `tutti_ump::Ump`, imported from there.
///
/// ```
/// use tutti_midi_types::prelude::*;
/// use tutti_ump::{Address, Attribute, Message, MidiChannel, MidiGroup, NoteNumber, Ump, Voice2};
///
/// // Build a note, decode it back.
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
pub mod prelude {
    pub use crate::{
        ControllerNamespace, MidiMessage, NoteAttribute, PerNoteController, normalize,
    };
}
