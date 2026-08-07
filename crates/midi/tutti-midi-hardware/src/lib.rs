#![doc = include_str!("../README.md")]

// --- Framework-free hardware I/O core ---

pub mod core;
pub use core::error;
/// MIDI 1.0 SysEx reassembly → UMP SysEx7. OS-free, so every driver edge shares
/// it and it is testable without a device.
pub use core::Sysex7Assembler;
/// The endpoint vocabulary: what a MIDI endpoint is, what it can carry, and the
/// backend seam that enumerates and opens them.
pub use core::{
    EndpointId, EndpointInfo, InputConnection, MidiEndpoints, MidiSession, UmpCapability,
};
pub use core::{Error, Result};
pub use core::{HardwareMidiInputs, InputProducerHandle, PortInfo, PortType};

/// `HardwareMidiInputs` and friends live in [`core::port`]; re-exported at the
/// crate root so a consumer can name the port vocabulary as a group
/// (`hardware::port::*`) without knowing it sits under `core`.
pub use core::port;

#[cfg(target_os = "macos")]
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
// snapshot / clip playback live in `tutti-midi-runtime`; surfaced here because
// delivery is what a port feeds — an inbound event goes straight from a driver
// into the bus, so a hardware consumer needs both. That is the test the dropped
// file re-export failed: a `.mid` reader needs no port, and no port needs it.

pub use tutti_midi_runtime::{
    MidiBus, MidiClipSource, MidiMailbox, MidiReceiver, MidiSender, MidiSnapshot,
    Sysex7Reassembler, TimedClipEvent, TimedMidiEvent,
};

pub use crossbeam_channel;

// --- Standard MIDI File codec: NOT here ---
//
// The file codecs live in `tutti-midi-file` and are deliberately *not*
// re-exported. Reading a `.mid` and talking to a MIDI port are different jobs;
// pairing them once made a consumer that wanted only the former link CoreMIDI.
// This crate used to pass the SMF surface through under its old `-io` name,
// which read as plausible; under `-hardware` it is plainly a category error — a
// file is not a device. Depend on `tutti-midi-file` directly for `smf` / `clip`.

/// The hardware MIDI prelude, for `use tutti_midi_hardware::prelude::*;` —
/// everything a typical app touches, from one import.
///
/// It re-exports [`tutti_midi_types::prelude`] (the wire event + decoded view +
/// clip-file codec + per-note identity) and adds this crate's I/O and delivery:
///
/// - **Hardware I/O** — [`MidiSession`] (enumerate / connect / send).
/// - **Delivery** — [`MidiBus`] / [`MidiSender`] / [`MidiReceiver`] (lock-free
///   fan-out), and beat-scheduled playback ([`MidiClipSource`], [`MidiSnapshot`],
///   [`TimedMidiEvent`]).
///
/// Deliberately excludes the rarer surfaces — UMP-Stream endpoint negotiation,
/// the Bevy ECS layer, sync decoders, MPE zone config — which stay explicit
/// imports (`::MidiClockDecoder`, `::ecs::*`, …). Glob this for the 90% path;
/// import the rest by name. The SMF / Clip File codecs are not here at all:
/// they are `tutti-midi-file`'s, and this crate does not re-export them.
///
/// ```
/// use tutti_midi_hardware::prelude::*;
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

    pub use crate::MidiSession;
}

// This crate is OS MIDI I/O plus the value types a host drives. The ECS
// bindings that drive them — routing, sequence, scheduled dispatch, clock-out,
// track-out, metadata, negotiation, device management, MPE — live in
// `bevy_tutti::midi`.
