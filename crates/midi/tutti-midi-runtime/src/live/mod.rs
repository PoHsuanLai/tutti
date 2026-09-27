//! MIDI crossing the graph's edge: in from a wire or a control thread, out to
//! one. Inside the graph MIDI travels on event
//! ports; these are the nodes at its border.
//!
//! - [`MidiInputNode`] polls a [`MidiIn`](tutti_midi_types::MidiIn) (the
//!   hardware ports), translates, and sends each event out of its channel's
//!   port: routing by channel is which ports a listener is wired to.
//! - [`MidiQueueNode`] sends what a control thread pushes (a keyboard).
//! - [`MidiOutNode`] hands what reaches it to a control thread (a hardware
//!   pump).
//! - [`MidiMailbox`] is the ring the last two cross the thread boundary on.

pub mod input_node;
pub mod mailbox;
pub(crate) mod order;
pub mod out_node;
pub mod queue_node;

pub use input_node::{
    input_port_of, input_ports, MidiInputControls, MidiInputNode, CHANNELLESS_PORT,
    MIDI_INPUT_EVENT_CAPACITY, MIDI_INPUT_PORTS,
};
pub use mailbox::{MidiMailbox, MidiReceiver, MidiSender, MAILBOX_CAPACITY};
pub use out_node::{MidiOutControls, MidiOutNode, MIDI_OUT_CAPACITY};
pub use queue_node::{MidiQueueNode, MIDI_QUEUE_CAPACITY};
