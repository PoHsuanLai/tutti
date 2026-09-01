//! MIDI runtime state machines for the Tutti audio engine.
//!
//! Pure MIDI types live in [`tutti_midi_types`]; hardware I/O lives in
//! `tutti-midi-hardware`. This crate owns the *runtime state* that connects
//! them:
//!
//! - [`MidiMailbox`] / [`MidiSender`] / [`MidiReceiver`] — lock-free per-unit
//!   MIDI inboxes; nodes own a receiver, callers push via senders
//! - [`MidiInPort`] — a unit's whole MIDI endpoint in one borrow: routing
//!   address, push mailbox, and the source-install slot
//! - [`MidiBus`] — fan-out mapping [`tutti_midi_types::MidiUnitId`] to
//!   [`MidiSender`]; the engine installs one as its audio-thread dispatch target
//! - [`MidiSnapshot`] / [`MidiSnapshotReader`] / [`MidiClipSource`] —
//!   non-destructive event storage and the readers that play it back
//! - [`MidiRoutingTable`] — the UI-thread writer that publishes immutable
//!   [`MidiRoutingSnapshot`](tutti_midi_types::MidiRoutingSnapshot) values
//!   through `RtPublish`
//! - [`ClockMaster`] and the rest of engine-produced MIDI *out* (Beat Clock,
//!   MTC, JR timestamps), riding the *same* [`MidiMailbox`] as MIDI in
//! - [`MpeIngest`] — the input-edge transform rewriting classic-MPE
//!   channel-spread into native MIDI-2 per-note messages (per M2-104, MPE is an
//!   ingestion concern; synth voices track per-note expression themselves)
//!
//! Refusal is shaped as a *value* throughout, which is why there is no `Error`
//! type: a full mailbox drops and reports a count, and [`MpeIngest::translate`]
//! returns `Option`.
//!
//! The three worked examples — an event reaching an inbox, a snapshot on the
//! transport, and MPE folded to per-note messages — plus the
//! [`MpeIngest::set_mode`] caveat are in the crate README, included below.
#![doc = include_str!("../README.md")]

// NOTE: this crate has no fallible operation and therefore no `Error` type.
// Everything here runs on or feeds the audio thread, where refusal is shaped as
// a value, not an error: a full mailbox drops and reports a count, and
// `MpeIngest::translate` returns `Option`. The MIDI parse errors live one crate
// down (`tutti_midi_types::ClipFileError`, `MidiParseError`).

mod block;
mod negotiate;
mod outbound;
mod schedule;
mod sysex;

pub use block::{
    BlockClock, MidiBus, MidiInPort, MidiMailbox, MidiOutSink, MidiPostBlock, MidiPreBlock,
    MidiReceiver, MidiSender, MpeModeRequest, MIDI_OUT_LATENCY_BLOCKS,
};
pub use negotiate::{
    CiInitiator, CiProperty, CiResponder, DeviceIdentity, DiscoveredCiDevice, DiscoveredEndpoint,
    EndpointInquiry, EndpointNegotiator, FunctionBlock,
};
pub use outbound::{
    ClockMaster, JrClock, JrClockEmitter, JrReceiver, JrStamper, JrStream, MpeIngest,
    JR_CLOCK_INTERVAL, JR_CLOCK_MAX_INTERVAL,
};
pub use schedule::{
    MidiClipSource, MidiSnapshot, MidiSnapshotReader, TimedClipEvent, TimedMidiEvent,
};
// The two capacity ceilings come along: a caller sizing its own SysEx buffer, or
// choosing a non-default limit, has to name the same bound.
pub use sysex::{
    Sysex7PacketReassembler, Sysex8Abort, Sysex8Event, Sysex8PacketReassembler,
    DEFAULT_MAX_SYSEX8_BYTES, DEFAULT_MAX_SYSEX_BYTES,
};

// Value types that live one crate down, re-exported so a consumer of the runtime
// needs one import rather than two.
//
// `MidiRoutingTable` sits in `tutti-midi-types` beside the immutable
// `MidiRoutingSnapshot` it publishes — the mutable and published halves of one
// concept. It reached consumers through a local `routing_table` module that did
// nothing but re-export it; the module's only reference repo-wide was its own
// doc comment, so it is gone and this is direct.
//
// The MPE mode/zone types are here for the same reason. The runtime's own MPE
// state machine is not — MPE is an input-edge transform now, `MpeIngest`.
pub use tutti_midi_types::mpe::{MpeMode, MpeZone, MpeZoneConfig};
pub use tutti_midi_types::MidiRoutingTable;
