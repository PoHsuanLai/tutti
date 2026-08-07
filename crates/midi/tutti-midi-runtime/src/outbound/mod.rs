//! MIDI the engine *produces* or *rewrites*, as opposed to plumbing it moves.
//!
//! - [`ClockMaster`] generates outbound Beat Clock and MTC from the transport.
//!   Its output rides its own mailbox rather than the routing table, because
//!   System Real-Time is not addressed to a unit — there is no id to route it
//!   by.
//! - [`jr_timestamp`] stamps outbound packets with JR Timestamps (M2-104 §7.6),
//!   so a receiver can reconstruct the sender's timing rather than inherit the
//!   transport's jitter.
//! - [`MpeIngest`] rewrites classic-MPE channel-spread into native MIDI-2
//!   per-note messages.
//!
//! # `MpeIngest` sits here uneasily, and that is deliberate
//!
//! It is an *input*-edge transform: [`MidiPreBlock`](crate::MidiPreBlock) is its
//! only caller, applying it to events arriving from hardware. By direction it
//! belongs with the inbound phase; by *kind* it belongs here, because these
//! three are the crate's transforms and everything in [`block`](crate::block) is
//! plumbing.
//!
//! Grouping by kind won because the alternative splits a natural trio to put one
//! member next to its caller — which is proximity, not duty. If a second
//! input-edge transform ever appears, that calculus changes and the honest move
//! is a `transform/` group holding both, rather than quietly relocating this one.

pub mod clock_master;
pub mod jr_timestamp;
pub mod mpe_ingest;

pub use clock_master::ClockMaster;
pub use jr_timestamp::{
    JrClock, JrClockEmitter, JrReceiver, JrStamper, JrStream, JR_CLOCK_INTERVAL,
    JR_CLOCK_MAX_INTERVAL,
};
pub use mpe_ingest::MpeIngest;
