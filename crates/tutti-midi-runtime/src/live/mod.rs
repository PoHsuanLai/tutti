//! MIDI crossing the graph's edge into it, from a wire or a control thread (out
//! of it is an event tap, `tutti-graph`'s). Inside the graph MIDI travels on event
//! ports; these are the nodes at its border.
//!
//! - [`MidiIngestNode`] reads the event inlets of the hardware devices
//!   ([`DeviceInlets`]), translates, and sends each event out of its channel's
//!   port: routing by channel is which ports a listener is wired to.
//! - [`MidiKeys`] is the key helpers over `tutti-graph`'s event inlet, which
//!   is how a control thread sends into the graph (a keyboard, a preview).
//! - [`MidiMailbox`] is the ring a host's own outbound path pushes into.
//! - [`compile_routes`] turns inbound routing rules into which of the input
//!   node's ports each listener takes.

pub mod device_inlets;
pub mod ingest_node;
pub mod keys;
pub mod mailbox;
pub(crate) mod order;
mod route;

pub use device_inlets::{
    ArrivalClock, DeviceInlets, DeviceInput, DeviceSlot, InletChange, NoFreeSlot,
};
pub use ingest_node::{
    CHANNELLESS_PORT, MIDI_INPUT_DENSITY, MIDI_INPUT_EVENT_CAPACITY, MIDI_INPUT_PORTS,
    MidiIngestControls, MidiIngestNode, MidiInputPorts,
};
pub use keys::{LIVE_INLET_CAPACITY, MidiKeys};
pub use mailbox::{MAILBOX_CAPACITY, MidiMailbox, MidiReceiver, MidiSender};
pub use route::{RouteRule, compile_routes};
