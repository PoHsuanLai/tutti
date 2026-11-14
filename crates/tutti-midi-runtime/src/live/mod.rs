//! MIDI crossing the graph's edge into it, from a wire or a control thread (out
//! of it is an event tap, `tutti-graph`'s). Inside the graph MIDI travels on event
//! ports; these are the nodes at its border.
//!
//! - [`MidiIngestNode`] reads the event inlets of the hardware devices
//!   ([`DeviceInlets`]), translates, and sends each event out of its channel's
//!   port: routing by channel is which ports a listener is wired to.
//! - [`MpeTransformNode`] is the same transform as a node of one event stream
//!   (a clip's), the one holder of the zones there.
//! - [`MidiKeys`] is the key helpers over `tutti-graph`'s event inlet, which
//!   is how a control thread sends into the graph (a keyboard, a preview).
//! - [`MpeIngest`] is the inbound state that rewrites classic-MPE channel-spread
//!   into native MIDI-2 per-note messages ([`MidiIngestNode`] is its caller).
//! - [`MidiMailbox`] is the ring of the host's own **outbound** path (a host's
//!   events to the session's drain), not a way into the graph.
//! - [`compile_routes`] turns inbound routing rules into which of the input
//!   node's ports each listener takes.

pub mod device_inlets;
pub mod ingest_node;
pub mod keys;
pub mod mailbox;
pub mod mpe_ingest;
pub(crate) mod order;
mod route;
mod sensing;
pub mod transform_node;

pub use device_inlets::{
    ArrivalClock, Changes, DeviceInlets, DeviceInput, DeviceSlot, InletChange, NoFreeSlot,
};
pub use ingest_node::{
    CHANNELLESS_PORT, MIDI_INPUT_DENSITY, MIDI_INPUT_EVENT_CAPACITY, MIDI_INPUT_PORTS,
    MidiIngestControls, MidiIngestNode, MidiInputPorts, SlotCountMismatch,
};
pub use keys::{LIVE_INLET_CAPACITY, MidiKeys};
pub use mailbox::{MAILBOX_CAPACITY, MidiMailbox, MidiReceiver, MidiSender};
pub use mpe_ingest::MpeIngest;
pub use route::{RouteRule, compile_routes};
pub use sensing::{ACTIVE_SENSING_JUDGE, ActiveSensing};
pub use transform_node::{MpeTransformControls, MpeTransformNode};
