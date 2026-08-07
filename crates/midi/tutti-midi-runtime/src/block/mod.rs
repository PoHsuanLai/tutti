//! What the audio callback runs once per block, and the fabric it delivers over.
//!
//! The callback's shape is fixed and the ordering is the design:
//!
//! ```text
//! pre_block.run(frames)  →  engine.process(..)  →  post_block.run()
//! ```
//!
//! [`MidiPreBlock`] polls the hardware edge and routes what arrived into unit
//! inboxes *before* anything renders; [`MidiPostBlock`] fans out what the graph
//! emitted *after* everything has. Running the outbound half last is what makes
//! delivery independent of the order nodes happen to be scheduled in — an event
//! emitted mid-render would otherwise reach a node polled later this block but
//! not one polled earlier.
//!
//! The two phases sit on either side of the render, so they are separate types
//! rather than one object with two methods: a single `MidiBlock` would let a
//! caller run them in the wrong order, or forget the render between them.
//!
//! # The delivery fabric
//!
//! [`MidiBus`] is the address→inbox map both phases route through, and
//! [`MidiInPort`] is what a receiving node owns: a routing address, a push
//! mailbox, and a slot for a layered pull source. Three of this crate's five
//! internal dependency edges are inside this module, which is why these four
//! files belong together.

pub mod port;
pub mod post_block;
pub mod pre_block;
pub mod registry;

pub use port::MidiInPort;
pub use post_block::{MidiOutSink, MidiPostBlock, MIDI_OUT_LATENCY_BLOCKS};
pub use pre_block::{BlockClock, MidiPreBlock, MpeModeRequest};
pub use registry::{MidiBus, MidiMailbox, MidiReceiver, MidiSender};
