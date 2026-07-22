//! MIDI runtime state machines for the Tutti audio engine.
//!
//! Pure MIDI types live in [`tutti_midi_types`]. Hardware I/O lives in
//! `tutti-midi-io`. This crate owns the *runtime state* that connects them:
//!
//! - [`MidiEventSlot`] / [`MidiSender`] / [`MidiReceiver`] — lock-free
//!   per-unit MIDI inboxes; nodes own a receiver, callers push via senders
//! - [`MidiBus`] — fan-out mapping [`tutti_midi_types::MidiUnitId`] to
//!   [`MidiSender`], dispatching queued events to the right inbox; the
//!   `tutti` engine installs one as its audio-thread dispatch target
//! - [`MidiSnapshot`] — non-destructive event storage for offline export
//! - [`MidiSnapshotReader`] — [`tutti_midi_types::MidiIn`] implementation that
//!   reads a snapshot on an offline timeline
//! - [`MidiRoutingTable`] — UI-thread writer for routing rules, publishing
//!   immutable [`tutti_midi_types::MidiRoutingSnapshot`] values via [`arc_swap::ArcSwap`]
//! - [`midi_output_channel`] — a lock-free [`MidiOutputProducer`] /
//!   [`MidiOutputConsumer`] ring for carrying engine-produced MIDI out
//!   (e.g. the [`ClockMaster`]'s Beat Clock / MTC) to a hardware-out pump
//! - [`MpeProcessor`] / [`PerNoteExpression`] — MPE state machine mapping
//!   channel voice messages to per-note expression

pub use tutti_midi_types;

pub mod clip_player;
pub mod clock_master;
pub mod endpoint;
pub mod jr_timestamp;
pub mod output_collector;
pub mod registry;
pub mod routing_table;
pub mod snapshot;
pub mod snapshot_reader;

pub mod mpe;

pub use clip_player::{CompositeMidiSource, MidiClipSource, TimedClipEvent};
pub use clock_master::ClockMaster;
pub use endpoint::{DeviceIdentity, EndpointNegotiator, FunctionBlock};
pub use jr_timestamp::{JrClock, JrReceiver, JrStamper};
pub use output_collector::{
    midi_output_channel, midi_output_channel_with_capacity, MidiOutputConsumer, MidiOutputProducer,
};
pub use registry::{MidiBus, MidiEventSlot, MidiReceiver, MidiSender};
pub use routing_table::MidiRoutingTable;
pub use snapshot::{MidiSnapshot, TimedMidiEvent};
pub use snapshot_reader::MidiSnapshotReader;

pub use mpe::{MpeMode, MpeProcessor, MpeZone, MpeZoneConfig, PerNoteExpression};
