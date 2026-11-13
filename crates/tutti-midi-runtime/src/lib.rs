#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

// NOTE: this crate has no fallible operation and therefore no `Error` type.
// Everything here runs on or feeds the audio thread, where refusal is shaped as
// a value, not an error: a full mailbox drops and reports a count, and
// `MpeIngest::translate` sends nothing for an event it drops. The MIDI parse errors live one crate
// down (`tutti_midi_file::ClipFileError`, `tutti_ump::Midi1Parser`'s refusal, `None`).

mod clock_map;
mod held;
mod ledger;
mod live;
mod outbound;
mod output_plan;
mod schedule;
mod sync;
mod tempo_follower;
mod traits;

pub use clock_map::{ClockMap, ClockReading, GAIN, Observed, RESYNC};
pub use held::HeldState;
pub use ledger::{NoteLedger, Sending};
pub use live::{
    ACTIVE_SENSING_JUDGE, ActiveSensing, ArrivalClock, CHANNELLESS_PORT, Changes, DeviceInlets,
    DeviceInput, DeviceSlot, InletChange, LIVE_INLET_CAPACITY, MAILBOX_CAPACITY,
    MIDI_INPUT_DENSITY, MIDI_INPUT_EVENT_CAPACITY, MIDI_INPUT_PORTS, MidiIngestControls,
    MidiIngestNode, MidiInputPorts, MidiKeys, MidiMailbox, MidiReceiver, MidiSender, MpeIngest,
    NoFreeSlot, RouteRule, SlotCountMismatch, compile_routes,
};
pub use outbound::{
    CLOCK_EVENT_CAPACITY, ClockMaster, ClockNode, JR_CLOCK_INTERVAL, JR_CLOCK_MAX_INTERVAL,
    JrClock, JrClockEmitter, JrReceiver, JrStamper, JrStream,
};
pub use output_plan::{
    MAX_OUTPUT_AHEAD, MAX_OUTPUT_LATENCY, OffsetOutOfRange, Outgoing, OutputLatency, OutputOffset,
    OutputPlan, OutputTerms, Placement, SendAt,
};
pub use schedule::{
    CLIP_EVENT_DENSITY, HARMONY_EVENT_DENSITY, HarmonyControls, HarmonyNode, MidiClipControls,
    MidiClipNode, TimedHarmony, TimedMidiEvent,
};
pub use sync::{
    ClockTransportState, MidiClockDecoder, MtcDecoder, SmpteFrameRate, SmpteTimecode,
    UnknownFrameRate,
};
pub use tempo_follower::{Followed, TempoFollower, TempoSource};
pub use traits::MidiOut;
// The counts a device's inlet keeps, as `DeviceInput::counts` returns them.
pub use tutti_graph::InletCounts;
