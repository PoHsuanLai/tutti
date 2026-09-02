//! The OS edge for MIDI: native UMP through CoreMIDI (macOS) and the ALSA
//! seq-UMP sequencer (Linux).
//!
//! Enumerate endpoints, open them, send MIDI 2.0 out and receive it back. There
//! are **no cargo features** — the crate *is* the OS edge, so there is nothing
//! here to switch off. `backend::active()` is the only `cfg(target_os)` in the
//! crate that decides anything, and it decides once at construction.
//!
//! # What this crate does not do
//!
//! - **No file codecs.** Reading a `.mid` and talking to a MIDI port are
//!   different jobs; `smf` and `clip` live in `tutti-midi-file`, which this
//!   crate does not re-export. Pairing them once made a consumer that wanted
//!   only the former link CoreMIDI.
//! - **No MIDI value types or state machines.** The wire vocabulary is
//!   `tutti-midi-types`' and the routing / allocation / expression machinery is
//!   `tutti-midi-runtime`'s. Both are re-exported below for convenience; neither
//!   is defined here.
//! - **No audio.** Nothing here touches a sample buffer.
//! - **No ECS.** The Bevy systems that drive this — routing, scheduled dispatch,
//!   clock-out, device management, MPE — are `bevy_tutti::midi`'s.
//! - **No MIDI 1.0 transport.** Events reach the wire as UMP words, so
//!   MIDI-2-only messages (per-note controllers, per-note pitch bend, JR
//!   Timestamps) survive rather than vanishing into a `to_midi1_bytes` `None`.
//!
//! # Example: enumerate, connect, send
//!
//! [`MidiSession`] is the whole OS edge. Inbound events never pass *through* it
//! — each opened input gets its own lock-free ring in
//! [`HardwareMidiInputs`], which the audio thread drains; the session only owns
//! the connection, and dropping it closes the port.
//!
//! `no_run`: every path here opens a real device. It is still type-checked, so
//! a wrong method name fails the build.
//!
//! ```no_run
//! use std::sync::Arc;
//! use tutti_midi_hardware::prelude::*;
//! use tutti_midi_hardware::HardwareMidiInputs;
//!
//! // The rings inbound events land in, then a session over this platform's backend.
//! let ports = Arc::new(HardwareMidiInputs::new(1024));
//! let session = MidiSession::new(Arc::clone(&ports));
//!
//! // A fresh snapshot per call — device lists go stale on hot-plug, so nothing
//! // here is cached.
//! for endpoint in session.inputs() {
//!     println!("{}: {:?}", endpoint.name, endpoint.capability);
//! }
//!
//! // Connect by id when it matters; see the matching note below for by-name.
//! if let Some(first) = session.inputs().first() {
//!     session.connect_input(first.id)?;
//! }
//!
//! // Outbound: UMP words on the wire, so a MIDI-2-only message survives.
//! session.connect_output_by_name("iac")?;
//! let note = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000);
//! assert_eq!(session.send(&[note]), 1);
//! # Ok::<(), tutti_midi_hardware::Error>(())
//! ```
//!
//! ## Matching by name picks a device you did not choose
//!
//! Every `*_by_name` method matches **case-insensitive substring, first hit
//! wins**, in the backend's enumeration order — which is not sorted and not
//! stable across a hot-plug. So `"iac"` matches `"IAC Driver Bus 1"`, and a
//! name matching two devices silently takes whichever the OS listed first.
//! Connect by [`EndpointId`] when the choice matters.
//!
//! [`disconnect_input_by_name`](MidiSession::disconnect_input_by_name) is
//! weaker still: it searches a `HashMap` of open connections, so there is no
//! "first" at all and a name matching two open inputs closes an **arbitrary**
//! one. Use [`disconnect_input`](MidiSession::disconnect_input) with an id, or
//! [`disconnect_all_inputs`](MidiSession::disconnect_all_inputs).
//!
//! ## A build may have no backend, and it is not an error
//!
//! ALSA's UMP sequencer API landed in alsa-lib 1.2.10. An older (or absent)
//! alsa-lib compiles the same empty stub Windows gets — zero endpoints, and
//! [`Error::Unsupported`] naming the reason — after a `cargo:warning` from the
//! build script. It degrades rather than failing the build, so the code above
//! compiles everywhere and simply enumerates nothing where there is no backend.
//!
//! The full picture — backend table, quick start, and the two Linux loopback
//! traps — is in the crate README, included below.
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

// --- Runtime delivery (event fan-out + beat-scheduled playback) ---
//
// The lock-free dispatch (`MidiBus`/`MidiSender`/`MidiReceiver`) and the offline
// snapshot / clip playback live in `tutti-midi-runtime`; surfaced here because
// delivery is what a port feeds — an inbound event goes straight from a driver
// into the bus, so a hardware consumer needs both. That is the test a file codec
// fails: a `.mid` reader needs no port, and no port needs it.

pub use tutti_midi_runtime::{
    MidiBus, MidiClipSource, MidiMailbox, MidiReceiver, MidiSender, MidiSnapshot,
    Sysex7PacketReassembler, TimedClipEvent, TimedMidiEvent,
};

pub use crossbeam_channel;

// --- Standard MIDI File codec: NOT here ---
//
// The file codecs live in `tutti-midi-file` and are deliberately *not*
// re-exported. Reading a `.mid` and talking to a MIDI port are different jobs;
// pairing them once made a consumer that wanted only the former link CoreMIDI.
// A file is not a device. Depend on `tutti-midi-file` directly for `smf`/`clip`.

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
