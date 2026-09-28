//! MIDI the engine *produces* or *rewrites*, as opposed to plumbing it moves.
//!
//! - [`ClockMaster`] generates outbound Beat Clock and MTC from the
//!   transport; [`ClockNode`] ticks it in the graph and sends it out of an
//!   event port.
//! - [`jr_timestamp`] stamps outbound packets with JR Timestamps (M2-104 §7.6),
//!   so a receiver can reconstruct the sender's timing rather than inherit the
//!   transport's jitter.
//! - [`MpeIngest`] rewrites classic-MPE channel-spread into native MIDI-2
//!   per-note messages.
//!
//! [`MpeIngest`] is an input-edge transform ([`MidiInputNode`](crate::MidiInputNode)
//! is its caller); it sits here with the crate's other transforms.

pub mod clock_master;
pub mod clock_node;
pub mod jr_timestamp;
pub mod mpe_ingest;

pub use clock_master::ClockMaster;
pub use clock_node::{ClockNode, CLOCK_EVENT_CAPACITY};
pub use jr_timestamp::{
    JrClock, JrClockEmitter, JrReceiver, JrStamper, JrStream, JR_CLOCK_INTERVAL,
    JR_CLOCK_MAX_INTERVAL,
};
pub use mpe_ingest::MpeIngest;
