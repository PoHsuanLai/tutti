#![doc = include_str!("../README.md")]

// --- Framework-free hardware I/O core ---

// No `///` on these: a doc comment on a `pub mod` shadows the module's own
// `//!` and re-resolves its intra-doc links in this scope.
pub mod backend;
pub mod capability;
pub mod endpoints;
pub mod error;
pub mod port;
pub mod session;
pub mod sysex;

// A backend with no OS behind it, for testing the session layer. See the
// module's own docs for why it is exported rather than `#[cfg(test)]`.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use capability::{EndpointId, EndpointInfo, UmpCapability};
pub use endpoints::{InputConnection, MidiEndpoints};
pub use error::{Error, Result};
pub use port::{HardwareMidiInputs, InputProducerHandle, PortInfo, PortType};
pub use session::MidiSession;
pub use sysex::Sysex7ByteAssembler;

#[cfg(target_os = "macos")]
pub use backend::coremidi::{UmpVirtualDestination, UmpVirtualSource};

// --- Re-exports from tutti-midi-types (the pure MIDI vocabulary) ---

pub use tutti_midi_types::Protocol;

pub use tutti_midi_types::{
    midi2, midly, normalize, ControllerNamespace, MidiEvent, MidiIn, MidiMessage, MidiOut,
    NoteAttribute, NoteId, PerNoteController, UmpMessageType, UnencodableMessage,
};

/// MIDI-CI (M2-101) message codec + SysEx7 wire bridge. Re-exported so the app's
/// inbound-decode path can turn a reassembled SysEx7 run into a `CiMessage`
/// (`ci::sysex7_to_ci`) to feed the negotiators.
pub use tutti_midi_types::ci;

/// Stateful MIDI 1.0 → 2.0 translation (RPN/NRPN reassembly). Feed inbound CV1
/// events through [`Midi1ToMidi2Translator`] when a hardware source needs
/// multi-message (N)RPN runs collapsed into single MIDI-2 controller messages;
/// [`normalize`] alone handles only the stateless per-message quirks.
pub use tutti_midi_types::Midi1ToMidi2Translator;

/// MIDI 2.0 Clip File (M2-116) interchange — a portable single-clip UMP stream,
/// distinct from project save and from SMF. The MIDI 1.0 equivalent (`smf`) and
/// the file-level path codec (`clip`) are `tutti-midi-file`'s and are not
/// re-exported here.
pub use tutti_midi_types::{
    read_clip_file, write_clip_file, write_clip_file_from_beats, write_clip_file_with_header,
    ClipEvent, ClipFileError, ClipHeader, ClipNote, ParsedClipFile, CLIP_FILE_MAGIC,
};

pub use tutti_midi_types::mpe::{MpeMode, MpeZone, MpeZoneConfig};

// `MidiChannel` re-exports through `cc::mapping`, and the name resolves to the
// `tutti_types` newtype — not a `u8` alias.
pub use tutti_midi_types::cc::mapping::{CCMapping, CCNumber, CCTarget, MappingId, MidiChannel};

pub use tutti_midi_types::sync::{
    ClockTransportState, MidiClockDecoder, MtcDecoder, SmpteFrameRate, SmpteTimecode,
};

// --- Runtime delivery (the graph's MIDI border) ---
//
// The nodes a port feeds and is fed by (`MidiInputNode`, `MidiOutNode`), the
// ring MIDI crosses threads on (`MidiMailbox`/`MidiSender`/`MidiReceiver`) and
// clip playback live in `tutti-midi-runtime`; surfaced here because delivery
// is what a port feeds, so a hardware consumer needs both. That is the test a
// file codec fails: a `.mid` reader needs no port, and no port needs it.

pub use tutti_midi_runtime::{
    MidiInputNode, MidiMailbox, MidiOutNode, MidiReceiver, MidiSender, Sysex7PacketReassembler,
    TimedMidiEvent,
};

pub use crossbeam_channel;

// --- Standard MIDI File codec: NOT here ---
//
// The file codecs live in `tutti-midi-file` and are deliberately *not*
// re-exported. Reading a `.mid` and talking to a MIDI port are different jobs,
// and a consumer that wants only the former should not link CoreMIDI. Depend
// on `tutti-midi-file` directly for `smf`/`clip`.

/// The common types for a hardware MIDI app, for
/// `use tutti_midi_hardware::prelude::*;`.
///
/// It re-exports [`tutti_midi_types::prelude`] (the wire event + decoded view +
/// clip-file codec + per-note identity) and adds this crate's I/O and delivery:
///
/// - **Hardware I/O** — [`MidiSession`] (enumerate / connect / send).
/// - **Delivery** — [`MidiInputNode`] / [`MidiOutNode`] (the ports as graph
///   nodes), [`MidiMailbox`] / [`MidiSender`] / [`MidiReceiver`] (the lock-free
///   ring), and [`TimedMidiEvent`] (a clip's events).
///
/// The rarer surfaces (sync decoders such as [`MidiClockDecoder`], MPE zone
/// config, the MIDI-CI codec) are not in the prelude; import them from the
/// crate root by name. The SMF / Clip File codecs are not here at all:
/// they are `tutti-midi-file`'s, and this crate does not re-export them.
///
/// ```
/// use tutti_midi_hardware::prelude::*;
///
/// // The types prelude comes along: build + decode an event.
/// let ev = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000);
/// assert!(ev.message().is_note_on());
///
/// // And the delivery types are here too — hand an event across threads.
/// let (tx, rx) = MidiMailbox::pair();
/// tx.queue(&[ev]);
/// let mut buf = [ev; 4];
/// assert_eq!(rx.poll_into(&mut buf), 1);
/// ```
pub mod prelude {
    pub use tutti_midi_types::prelude::*;

    pub use crate::{
        MidiInputNode, MidiMailbox, MidiOutNode, MidiReceiver, MidiSender, TimedMidiEvent,
    };

    pub use crate::MidiSession;
}

// This crate is OS MIDI I/O plus the value types a host drives. The ECS
// bindings that drive them — routing, sequence, scheduled dispatch, clock-out,
// track-out, metadata, negotiation, device management, MPE — live in
// `bevy_tutti::midi`.
