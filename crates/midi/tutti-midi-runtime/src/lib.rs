//! MIDI runtime state machines for the Tutti audio engine.
//!
//! Pure MIDI types live in [`tutti_midi_types`]; hardware I/O lives in
//! `tutti-midi-hardware`. This crate owns the *runtime state* that connects
//! them:
//!
//! - MIDI as graph nodes (doc 013, rewrite item 5), sending on event ports:
//!   [`MidiInputNode`] (a wire, one port per channel: routing is wiring),
//!   [`MidiQueueNode`] (what a control thread sends), [`MidiClipNode`] and
//!   [`HarmonyNode`] (a timeline), [`ClockNode`] (Beat Clock and MTC), and
//!   [`MidiOutNode`], the sink handing MIDI back to a control thread
//! - [`MidiMailbox`] / [`MidiSender`] / [`MidiReceiver`] — the lock-free ring
//!   MIDI crosses a thread boundary on
//! - [`ClockMaster`] and the rest of engine-produced MIDI *out* (Beat Clock,
//!   MTC, JR timestamps)
//! - [`MpeIngest`] — the input-edge transform rewriting classic-MPE
//!   channel-spread into native MIDI-2 per-note messages (per M2-104, MPE is an
//!   ingestion concern; synth voices track per-note expression themselves)
//!
//! Refusal is shaped as a *value* throughout, which is why there is no `Error`
//! type: a full mailbox drops and reports a count, and [`MpeIngest::translate`]
//! returns `Option`.
//!
//! Worked examples are in the crate README, included below.
#![doc = include_str!("../README.md")]

// NOTE: this crate has no fallible operation and therefore no `Error` type.
// Everything here runs on or feeds the audio thread, where refusal is shaped as
// a value, not an error: a full mailbox drops and reports a count, and
// `MpeIngest::translate` returns `Option`. The MIDI parse errors live one crate
// down (`tutti_midi_types::ClipFileError`, `MidiParseError`).

mod live;
mod negotiate;
mod outbound;
mod schedule;
mod sysex;

pub use live::{
    input_port_of, input_ports, MidiInputControls, MidiInputNode, MidiMailbox, MidiOutControls,
    MidiOutNode, MidiQueueNode, MidiReceiver, MidiSender, CHANNELLESS_PORT, MAILBOX_CAPACITY,
    MIDI_INPUT_EVENT_CAPACITY, MIDI_INPUT_PORTS, MIDI_OUT_CAPACITY, MIDI_QUEUE_CAPACITY,
};
pub use negotiate::{
    CiInitiator, CiProperty, CiResponder, DeviceIdentity, DiscoveredCiDevice, DiscoveredEndpoint,
    EndpointInquiry, EndpointNegotiator, FunctionBlock,
};
pub use outbound::{
    ClockMaster, ClockNode, JrClock, JrClockEmitter, JrReceiver, JrStamper, JrStream, MpeIngest,
    CLOCK_EVENT_CAPACITY, JR_CLOCK_INTERVAL, JR_CLOCK_MAX_INTERVAL,
};
pub use schedule::{
    HarmonyControls, HarmonyNode, MidiClipControls, MidiClipNode, TimedHarmony, TimedMidiEvent,
    CLIP_EVENT_CAPACITY, HARMONY_EVENT_CAPACITY,
};
// The two capacity ceilings come along: a caller sizing its own SysEx buffer, or
// choosing a non-default limit, has to name the same bound.
pub use sysex::{
    Sysex7PacketReassembler, Sysex8Abort, Sysex8Event, Sysex8PacketReassembler,
    DEFAULT_MAX_SYSEX8_BYTES, DEFAULT_MAX_SYSEX_BYTES,
};

// Value types that live one crate down, re-exported so a consumer of the runtime
// needs one import rather than two.
//
// The MPE mode/zone types come along: `MidiInputNode::with_mpe` takes them.
pub use tutti_midi_types::mpe::{MpeMode, MpeZone, MpeZoneConfig};
