//! MIDI runtime state machines for the Tutti audio engine.
//!
//! Pure MIDI types live in [`tutti_midi_types`]. Hardware I/O lives in
//! `tutti-midi-hardware`. This crate owns the *runtime state* that connects them:
//!
//! - [`MidiMailbox`] / [`MidiSender`] / [`MidiReceiver`] — lock-free
//!   per-unit MIDI inboxes; nodes own a receiver, callers push via senders
//! - [`MidiBus`] — fan-out mapping [`tutti_midi_types::MidiUnitId`] to
//!   [`MidiSender`], dispatching queued events to the right inbox; the
//!   `tutti` engine installs one as its audio-thread dispatch target
//! - [`MidiSnapshot`] — non-destructive event storage for offline export
//! - [`MidiSnapshotReader`] — [`tutti_midi_types::MidiUnitIn`] implementation that
//!   reads a snapshot on an offline timeline
//! - [`MidiRoutingTable`] — UI-thread writer for routing rules, publishing
//!   immutable [`tutti_midi_types::MidiRoutingSnapshot`] values via `RtPublish`
//! - Engine-produced MIDI *out* (e.g. the [`ClockMaster`]'s Beat Clock / MTC)
//!   rides the *same* [`MidiMailbox`] mailbox as MIDI in: the producer holds a
//!   [`MidiSender`] (lock-free `&self` push via [`tutti_midi_types::MidiOut`]),
//!   an off-RT pump drains the paired [`MidiReceiver`] to a hardware-out port
//! - [`MpeIngest`] — input-edge transform rewriting classic-MPE channel-spread
//!   into native MIDI-2 per-note messages (per M2-104, MPE is an ingestion
//!   concern; synth voices track per-note expression themselves)

pub use tutti_midi_types;

pub mod block;
pub mod negotiate;
pub mod outbound;
pub mod schedule;
pub mod sysex;

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
pub use sysex::{Sysex7PacketReassembler, Sysex8Abort, Sysex8Event, Sysex8PacketReassembler};

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
