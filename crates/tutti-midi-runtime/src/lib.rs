#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

// NOTE: this crate has no fallible operation and therefore no `Error` type.
// Everything here runs on or feeds the audio thread, where refusal is shaped as
// a value, not an error: a full mailbox drops and reports a count, and
// `MpeIngest::translate` yields nothing for an event it drops. The MIDI parse errors live one crate
// down (`tutti_midi_types::ClipFileError`, `MidiParseError`).

mod gather;
mod group;
mod ledger;
mod live;
mod negotiate;
mod outbound;
mod schedule;
mod split;
mod sysex;
mod traits;

pub use gather::gather_midi;
pub use ledger::{NoteLedger, Sending};
pub use live::{
    CHANNELLESS_PORT, MAILBOX_CAPACITY, MIDI_INPUT_DENSITY, MIDI_INPUT_EVENT_CAPACITY,
    MIDI_INPUT_PORTS, MIDI_OUT_CAPACITY, MIDI_QUEUE_CAPACITY, MidiInputControls, MidiInputNode,
    MidiMailbox, MidiOutControls, MidiOutNode, MidiQueueNode, MidiReceiver, MidiSender, Moment,
    RouteRule, TIMED_QUEUE_CAPACITY, TimedQueueNode, TimedSendError, TimedSender, compile_routes,
    input_port_of, input_ports,
};
pub use negotiate::{
    CiInitiator, CiProperty, CiResponder, DeviceIdentity, DiscoveredCiDevice, DiscoveredEndpoint,
    EndpointInquiry, EndpointNegotiator, FunctionBlock, ProfileSwitch,
};
pub use outbound::{
    CLOCK_EVENT_CAPACITY, ClockMaster, ClockNode, JR_CLOCK_INTERVAL, JR_CLOCK_MAX_INTERVAL,
    JrClock, JrClockEmitter, JrReceiver, JrStamper, JrStream, MpeIngest, Translated,
};
pub use schedule::{
    CLIP_EVENT_DENSITY, HARMONY_EVENT_DENSITY, HarmonyControls, HarmonyNode, MidiClipControls,
    MidiClipNode, TimedHarmony, TimedMidiEvent,
};
pub use split::{BlockSplitter, Overrun, Step};
pub use traits::{MidiIn, MidiOut};
// The two capacity ceilings come along: a caller sizing its own SysEx buffer, or
// choosing a non-default limit, has to name the same bound.
pub use sysex::{
    DEFAULT_MAX_SYSEX_BYTES, DEFAULT_MAX_SYSEX8_BYTES, Sysex7PacketReassembler, Sysex8Abort,
    Sysex8Event, Sysex8PacketReassembler,
};

// Value types that live one crate down, re-exported so a consumer of the runtime
// needs one import rather than two.
//
// The MPE mode/zone types come along: `MidiInputNode::with_mpe` takes them.
pub use tutti_midi_types::mpe::{MpeMode, MpeZone, MpeZoneConfig};
