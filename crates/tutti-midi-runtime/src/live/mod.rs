//! MIDI crossing the graph's edge: in from a wire or a control thread, out to
//! one. Inside the graph MIDI travels on event
//! ports; these are the nodes at its border.
//!
//! - [`MidiInputNode`] polls a [`MidiIn`](crate::MidiIn) (the
//!   hardware ports), translates, and sends each event out of its channel's
//!   port: routing by channel is which ports a listener is wired to.
//! - [`MidiKeys`] is the key helpers over `tutti-graph`'s event inlet, which
//!   is how a control thread sends into the graph (a keyboard, a preview).
//! - [`MidiOutNode`] hands what reaches it to a control thread (a hardware
//!   pump).
//! - [`MidiMailbox`] is the ring that node hands it over on, and the one a
//!   host's own outbound path pushes into.
//! - [`compile_routes`] turns inbound routing rules into which of the input
//!   node's ports each listener takes.

pub mod input_node;
pub mod keys;
pub mod mailbox;
pub(crate) mod order;
pub mod out_node;
mod route;

pub use input_node::{
    CHANNELLESS_PORT, MIDI_INPUT_DENSITY, MIDI_INPUT_EVENT_CAPACITY, MIDI_INPUT_PORTS,
    MidiInputControls, MidiInputNode, input_port_of, input_ports,
};
pub use keys::{LIVE_INLET_CAPACITY, MidiKeys};
pub use mailbox::{MAILBOX_CAPACITY, MidiMailbox, MidiReceiver, MidiSender};
pub use out_node::{MIDI_OUT_CAPACITY, MidiOutControls, MidiOutNode};
pub use route::{RouteRule, compile_routes};
