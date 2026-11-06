//! MIDI that arrives from a position on a timeline, rather than off a wire:
//! event source nodes walking each block's transport (the shared walk,
//! `walk.rs`).
//!
//! - [`MidiClipNode`] plays a clip's [`TimedMidiEvent`]s out of an event port.
//! - [`HarmonyNode`] sends a sequencer's chord and scale lanes.

pub mod clip_node;
pub mod harmony_node;
pub mod timed;
mod walk;

pub use clip_node::{CLIP_EVENT_DENSITY, MidiClipControls, MidiClipNode};
pub use harmony_node::{HARMONY_EVENT_CAPACITY, HarmonyControls, HarmonyNode, TimedHarmony};
pub use timed::TimedMidiEvent;
