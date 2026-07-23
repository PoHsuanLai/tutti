//! Shared MIDI event collection width for the plugin host boundary.
//!
//! The stack-inline capacity a plugin's per-block MIDI in/out carries before
//! it spills to the heap. Named here (not on `tutti-plugin`'s IPC layer) so the
//! format host crates and the shared [`ProcessOutput`](crate::ProcessOutput)
//! can speak the type without depending on `tutti-plugin`.

use smallvec::SmallVec;

use crate::MidiEvent;

/// Stack-inline capacity for a per-block MIDI event list before it spills to
/// the heap.
pub const MIDI_STACK_CAPACITY: usize = 256;

/// A per-block MIDI event list — stack-inline up to [`MIDI_STACK_CAPACITY`],
/// then heap-backed.
pub type MidiEventVec = SmallVec<[MidiEvent; MIDI_STACK_CAPACITY]>;
