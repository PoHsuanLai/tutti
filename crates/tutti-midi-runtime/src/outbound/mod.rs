//! MIDI the engine *produces*, as opposed to plumbing it moves.
//!
//! - [`ClockMaster`] (and its [`ClockControls`]) generates outbound Beat Clock and MTC from the
//!   transport; [`ClockNode`] ticks it in the graph and sends it out of an
//!   event port.
//! - [`MachineControlSender`] turns what the transport did into MIDI Machine
//!   Control commands.
//! - [`jr_timestamp`] stamps outbound packets with JR Timestamps (M2-104 §7.6),
//!   so a receiver can reconstruct the sender's timing rather than inherit the
//!   transport's jitter.

pub mod clock_master;
pub mod clock_node;
pub mod jr_timestamp;
pub mod machine_control_out;

pub use clock_master::{ClockControls, ClockMaster};
pub use clock_node::{CLOCK_EVENT_CAPACITY, ClockNode};
pub use jr_timestamp::{
    JR_CLOCK_INTERVAL, JR_CLOCK_MAX_INTERVAL, JrClock, JrClockEmitter, JrReceiver, JrStamper,
    JrStream,
};
pub use machine_control_out::MachineControlSender;
