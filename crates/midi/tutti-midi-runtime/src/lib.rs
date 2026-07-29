//! MIDI runtime state machines for the Tutti audio engine.
//!
//! Pure MIDI types live in [`tutti_midi_types`]. Hardware I/O lives in
//! `tutti-midi-io`. This crate owns the *runtime state* that connects them:
//!
//! - [`MidiMailbox`] / [`MidiSender`] / [`MidiReceiver`] — lock-free
//!   per-unit MIDI inboxes; nodes own a receiver, callers push via senders
//! - [`MidiBus`] — fan-out mapping [`tutti_midi_types::MidiUnitId`] to
//!   [`MidiSender`], dispatching queued events to the right inbox; the
//!   `tutti` engine installs one as its audio-thread dispatch target
//! - [`MidiSnapshot`] — non-destructive event storage for offline export
//! - [`MidiSnapshotReader`] — [`tutti_midi_types::MidiIn`] implementation that
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

pub mod capability_inquiry;
pub mod clip_player;
pub mod clock_master;
pub mod endpoint;
pub mod jr_timestamp;
pub mod mpe_ingest;
pub mod port;
pub mod pre_block;
pub mod registry;
pub mod routing_table;
pub mod snapshot;
pub mod snapshot_reader;
pub mod sysex8_reassembler;
pub mod sysex_reassembler;

pub use capability_inquiry::{CiInitiator, CiProperty, CiResponder, DiscoveredCiDevice};
pub use clip_player::{MidiClipSource, TimedClipEvent};
pub use clock_master::ClockMaster;
pub use endpoint::{
    DeviceIdentity, DiscoveredEndpoint, EndpointInquiry, EndpointNegotiator, FunctionBlock,
};
pub use jr_timestamp::{
    JrClock, JrClockEmitter, JrReceiver, JrStamper, JrStream, JR_CLOCK_INTERVAL,
    JR_CLOCK_MAX_INTERVAL,
};
pub use port::MidiInPort;
pub use pre_block::{BlockClock, MidiPreBlock};
pub use registry::{MidiBus, MidiMailbox, MidiReceiver, MidiSender};
pub use routing_table::MidiRoutingTable;
pub use snapshot::{MidiSnapshot, TimedMidiEvent};
pub use snapshot_reader::MidiSnapshotReader;
pub use sysex8_reassembler::{Sysex8Abort, Sysex8Event, Sysex8Reassembler};
pub use sysex_reassembler::Sysex7Reassembler;

// MPE mode/zone value types live in tutti-midi-types; re-exported here for
// source compatibility (the runtime's own MPE state machine is gone — MPE is now
// an input-edge transform, `MpeIngest`).
pub use mpe_ingest::MpeIngest;
pub use tutti_midi_types::mpe::{MpeMode, MpeZone, MpeZoneConfig};
