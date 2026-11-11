#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

// NOTE: this crate has no fallible operation and therefore no `Error` type.
// Everything here runs on or feeds the audio thread, where refusal is shaped as
// a value, not an error: a full mailbox drops and reports a count, and
// `MpeIngest::translate` sends nothing for an event it drops. The MIDI parse errors live one crate
// down (`tutti_midi_file::ClipFileError`, `tutti_ump::Midi1Parser`'s refusal, `None`).

mod clock_map;
mod ledger;
mod live;
mod outbound;
mod schedule;
mod sync;
mod traits;

pub use clock_map::{ClockMap, ClockReading, GAIN, Observed, RESYNC};
pub use ledger::{NoteLedger, Sending};
pub use live::{
    CHANNELLESS_PORT, LIVE_INLET_CAPACITY, MAILBOX_CAPACITY, MIDI_INPUT_DENSITY,
    MIDI_INPUT_EVENT_CAPACITY, MIDI_INPUT_PORTS, MidiInputControls, MidiInputNode, MidiKeys,
    MidiMailbox, MidiReceiver, MidiSender, RouteRule, compile_routes, input_port_of, input_ports,
};
pub use outbound::{
    CLOCK_EVENT_CAPACITY, ClockMaster, ClockNode, JR_CLOCK_INTERVAL, JR_CLOCK_MAX_INTERVAL,
    JrClock, JrClockEmitter, JrReceiver, JrStamper, JrStream, MpeIngest,
};
pub use schedule::{
    CLIP_EVENT_DENSITY, HARMONY_EVENT_DENSITY, HarmonyControls, HarmonyNode, MidiClipControls,
    MidiClipNode, TimedHarmony, TimedMidiEvent,
};
pub use sync::{ClockTransportState, MidiClockDecoder, MtcDecoder, SmpteFrameRate, SmpteTimecode};
pub use traits::{MidiIn, MidiOut};
