//! Hardware and file MIDI I/O for the Tutti engine.
//!
//! [`core`] is the hardware edge: [`MidiIo`], the `midir`/`coremidi` drivers,
//! and the audio-thread ring buffers. Plus [`smf`], the Standard MIDI File
//! codec, and passthrough re-exports of the pure MIDI vocabulary from
//! [`tutti_midi_types`]. The whole surface re-exports at the crate root, so
//! consumers write `tutti_midi_io::MidiIo`.

// --- Framework-free hardware I/O core ---

pub mod core;
pub use core::error;
/// MIDI 1.0 SysEx reassembly → UMP SysEx7. OS-free, so every driver edge shares
/// it and it is testable without a device.
pub use core::Sysex7Assembler;
/// The endpoint vocabulary: what a MIDI endpoint is, what it can carry, and the
/// backend seam that enumerates and opens them.
pub use core::{EndpointId, EndpointInfo, InputConnection, MidiEndpoints, UmpCapability};
pub use core::{Error, Result};
// OS hardware orchestrator, the device descriptor, and the record its observer
// channel carries — only present under `midi-hardware` (they own the `midir`
// edge).
pub use core::{HardwareMidiInputs, InputProducerHandle, PortInfo, PortType};
#[cfg(feature = "midi-hardware")]
pub use core::{MidiDevice, MidiInputRecord, MidiIo};

/// `HardwareMidiInputs` and friends live in [`core::port`]; kept as a crate-root
/// module path for the `tutti_midi_io::port::*` spelling consumers already use.
pub use core::port;

#[cfg(all(target_os = "macos", feature = "midi-hardware"))]
pub use core::{UmpVirtualDestination, UmpVirtualSource};

// --- Re-exports from tutti-midi-types (the pure MIDI vocabulary) ---

pub use tutti_midi_types::Protocol;

pub use tutti_midi_types::{
    midi2, midly, normalize, ControllerNamespace, MidiEvent, MidiIn, MidiMessage, MidiOut,
    MidiUnitId, NoteAttribute, NoteId, PerNoteController, UmpMessageType, UnencodableMessage,
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
/// distinct from project save (Loro) and from SMF. See [`crate::smf`] for the
/// MIDI 1.0 equivalent, and [`crate::clip`] for the file-level (path) codec.
pub use tutti_midi_types::{
    read_clip_file, write_clip_file, write_clip_file_from_beats, write_clip_file_with_header,
    ClipEvent, ClipFileError, ClipHeader, ClipNote, ParsedClipFile, CLIP_FILE_MAGIC,
};

pub use tutti_midi_types::mpe::{MpeMode, MpeZone, MpeZoneConfig};

// `MidiChannel` re-exports through `cc::mapping` as it always has, but the
// name now resolves to the real `tutti_types` newtype rather than the
// `pub type MidiChannel = u8` alias that used to live there.
pub use tutti_midi_types::cc::mapping::{CCMapping, CCNumber, CCTarget, MappingId, MidiChannel};

pub use tutti_midi_types::sync::{
    ClockTransportState, MidiClockDecoder, MtcDecoder, SmpteFrameRate, SmpteTimecode,
};

// --- Runtime delivery (event fan-out + beat-scheduled playback) ---
//
// The lock-free dispatch (`MidiBus`/`MidiSender`/`MidiReceiver`) and the offline
// snapshot / clip playback live in `tutti-midi-runtime`; surface them here so an
// app depends on this one umbrella crate rather than reaching into the runtime.

pub use tutti_midi_runtime::{
    MidiBus, MidiClipSource, MidiMailbox, MidiReceiver, MidiSender, MidiSnapshot,
    Sysex7Reassembler, TimedClipEvent, TimedMidiEvent,
};

pub use crossbeam_channel;

// --- Standard MIDI File codec ---

/// MIDI 2.0 Clip File (M2-116) file I/O — read/write a clip by path, and
/// identify which MIDI format a file holds ([`MidiFileKind`]) by magic rather
/// than by extension. The byte-level codec lives in [`tutti_midi_types`].
pub mod clip;
pub use clip::{read_clip_file_from_path, write_clip_file_to_path, MidiFileKind};

/// Standard MIDI File (SMF) read/write — parse a `.mid` into beat-positioned
/// events ([`ParsedMidiFile`]) or per-track paired notes ([`smf::tracks`]), and
/// encode events back out ([`encode_midi_file`]).
pub mod smf;
pub use smf::{
    encode_midi_file, write_midi_file, MidiWriteOptions, ParsedMidiFile, SmfMessage, SmfNote,
    SmfTimedEvent, SmfTrack,
};

/// The umbrella MIDI prelude, for `use tutti_midi_io::prelude::*;` — everything a
/// typical app touches, from one import.
///
/// It re-exports [`tutti_midi_types::prelude`] (the wire event + decoded view +
/// clip-file codec + per-note identity) and adds this crate's I/O and delivery:
///
/// - **Hardware I/O** — [`MidiIo`] (connect / send / observe) and [`MidiDevice`].
/// - **Delivery** — [`MidiBus`] / [`MidiSender`] / [`MidiReceiver`] (lock-free
///   fan-out), and beat-scheduled playback ([`MidiClipSource`], [`MidiSnapshot`],
///   [`TimedMidiEvent`]).
///
/// Deliberately excludes the rarer surfaces — SMF codec internals, UMP-Stream
/// endpoint negotiation, the Bevy ECS layer, sync decoders, MPE zone config —
/// which stay explicit imports (`tutti_midi_io::smf`, `::MidiClockDecoder`,
/// `::ecs::*`, …). Glob this for the 90% path; import the rest by name.
///
/// ```
/// use tutti_midi_io::prelude::*;
///
/// // The types prelude comes along: build + decode an event.
/// let ev = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000);
/// assert!(ev.message().is_note_on());
///
/// // And the delivery types are here too — fan an event to a unit's inbox.
/// let (tx, rx) = MidiMailbox::pair(MidiUnitId::new(1));
/// let bus = MidiBus::new();
/// bus.insert(tx);
/// bus.queue(MidiUnitId::new(1), &[ev]);
/// let mut buf = [ev; 4];
/// assert_eq!(rx.poll_into(&mut buf), 1);
/// ```
pub mod prelude {
    pub use tutti_midi_types::prelude::*;

    pub use crate::{
        MidiBus, MidiClipSource, MidiMailbox, MidiReceiver, MidiSender, MidiSnapshot,
        TimedMidiEvent,
    };

    // The OS orchestrator + device descriptor only exist under `midi-hardware`.
    #[cfg(feature = "midi-hardware")]
    pub use crate::{MidiDevice, MidiIo};
}

// This crate is OS MIDI I/O plus the value types a host drives. The ECS
// bindings that drive them — routing, sequence, scheduled dispatch, clock-out,
// track-out, metadata, negotiation, device management, MPE — live in
// `bevy_tutti::midi`.
