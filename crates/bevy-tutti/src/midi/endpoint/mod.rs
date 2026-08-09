//! How a node becomes reachable, in both directions.
//!
//! The two shared RT collection points — [`bus`] is what the inbound phase
//! queues into, [`out_sink`] what an emitting node pushes into for the
//! outbound phase — plus [`registration`], which keeps the bus in step with
//! the graph, and [`target`], which resolves an entity to the `MidiInPort`
//! its audio node owns.
//!
//! Nothing here decides *routing* (that is [`inbound`](crate::midi::inbound)),
//! touches an OS port (that is [`hardware`](crate::midi::hardware)), or knows
//! the clock: an address is not a destination.

pub mod bus;
pub mod out_sink;
pub mod registration;
pub mod target;
