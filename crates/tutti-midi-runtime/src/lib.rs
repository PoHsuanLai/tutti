#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

// NOTE: this crate has no fallible operation and therefore no `Error` type.
// Everything here runs on or feeds the audio thread, where refusal is shaped as
// a value, not an error: a full mailbox drops and reports a count, and
// `MpeIngest::translate` sends nothing for an event it drops. The MIDI parse errors live one crate
// down (`tutti_midi_types::ClipFileError`, `MidiParseError`).

mod ledger;
mod live;
mod outbound;
mod schedule;
mod traits;

pub use ledger::{NoteLedger, Sending};
pub use live::{
    CHANNELLESS_PORT, MAILBOX_CAPACITY, MIDI_INPUT_DENSITY, MIDI_INPUT_EVENT_CAPACITY,
    MIDI_INPUT_PORTS, MIDI_OUT_CAPACITY, MIDI_QUEUE_CAPACITY, MidiInputControls, MidiInputNode,
    MidiMailbox, MidiOutControls, MidiOutNode, MidiQueueNode, MidiReceiver, MidiSender, RouteRule,
    TIMED_QUEUE_CAPACITY, TimedQueueNode, TimedSendError, TimedSender, compile_routes,
    input_port_of, input_ports,
};
pub use outbound::{
    CLOCK_EVENT_CAPACITY, ClockMaster, ClockNode, JR_CLOCK_INTERVAL, JR_CLOCK_MAX_INTERVAL,
    JrClock, JrClockEmitter, JrReceiver, JrStamper, JrStream, MpeIngest,
};
pub use schedule::{
    CLIP_EVENT_DENSITY, HARMONY_EVENT_DENSITY, HarmonyControls, HarmonyNode, MidiClipControls,
    MidiClipNode, TimedHarmony, TimedMidiEvent,
};
pub use traits::{MidiIn, MidiOut};

// Value types that live one crate down, re-exported so a consumer of the runtime
// needs one import rather than two.
//
// The MPE mode/zone types come along: `MidiInputNode::with_mpe` takes them.
pub use tutti_midi_types::mpe::{MpeMode, MpeZone, MpeZoneConfig};
