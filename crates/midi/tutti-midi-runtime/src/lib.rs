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
//!   immutable [`tutti_midi_types::MidiRoutingSnapshot`] values via [`arc_swap::ArcSwap`]
//! - Engine-produced MIDI *out* (e.g. the [`ClockMaster`]'s Beat Clock / MTC)
//!   rides the *same* [`MidiMailbox`] mailbox as MIDI in: the producer holds a
//!   [`MidiSender`] (lock-free `&self` push via [`tutti_midi_types::MidiOut`]),
//!   an off-RT pump drains the paired [`MidiReceiver`] to a hardware-out port
//! - [`MpeProcessor`] / [`PerNoteExpression`] — MPE state machine mapping
//!   channel voice messages to per-note expression

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
pub mod sysex_reassembler;

pub mod mpe;

pub use capability_inquiry::{CiInitiator, CiProperty, CiResponder, DiscoveredCiDevice};
pub use clip_player::{MidiClipSource, TimedClipEvent};
pub use clock_master::ClockMaster;
pub use endpoint::{
    DeviceIdentity, DiscoveredEndpoint, EndpointInquiry, EndpointNegotiator, FunctionBlock,
};
pub use jr_timestamp::{JrClock, JrReceiver, JrStamper};
pub use port::MidiInPort;
pub use pre_block::{BlockClock, MidiPreBlock};
pub use registry::{MidiBus, MidiMailbox, MidiReceiver, MidiSender};
pub use routing_table::MidiRoutingTable;
pub use snapshot::{MidiSnapshot, TimedMidiEvent};
pub use snapshot_reader::MidiSnapshotReader;
pub use sysex_reassembler::Sysex7Reassembler;

pub use mpe::{MpeMode, MpeProcessor, MpeZone, MpeZoneConfig, PerNoteExpression};
pub use mpe_ingest::MpeIngest;
