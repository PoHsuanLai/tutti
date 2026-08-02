//! Shared MIDI event collection width for the plugin host boundary.
//!
//! The stack-inline capacity a plugin's per-block MIDI in/out carries before
//! it spills to the heap. Named here (not on `tutti-plugin`'s IPC layer) so the
//! format host crates and the shared [`ProcessOutput`](crate::ProcessOutput)
//! can speak the type without depending on `tutti-plugin`.

use smallvec::SmallVec;
use tutti_types::RtVec;

use crate::MidiEvent;

/// Stack-inline capacity for a per-block MIDI event list before it spills to
/// the heap.
pub const MIDI_STACK_CAPACITY: usize = 256;

/// A per-block MIDI event list — stack-inline up to [`MIDI_STACK_CAPACITY`],
/// then heap-backed.
///
/// This is the **off-RT / IPC** vocabulary: it serializes, and it is allowed to
/// grow. For a pool filled inside `process`, use [`RtMidiEvents`] instead,
/// which cannot.
pub type MidiEventVec = SmallVec<[MidiEvent; MIDI_STACK_CAPACITY]>;

/// Cap for a MIDI event pool filled on the audio thread.
///
/// Smaller than [`MIDI_STACK_CAPACITY`] because the two answer different
/// questions: that one asks how much a *wire message* carries before it spills
/// (a spill there is merely slow), while this one is a hard per-block ceiling
/// on the audio thread (there is no spill — events past it are dropped). 64
/// events in one block is already far above what a plugin emits in normal use.
pub const RT_MIDI_CAPACITY: usize = 64;

/// A per-block MIDI event pool filled inside `process` and lent back out as
/// `&[MidiEvent]`.
///
/// Prefer this over a bare `SmallVec` for anything a plugin's output drains
/// into: the host does not control how many events a plugin emits, so an
/// unbounded pool puts a `malloc` in the audio callback at the plugin's
/// discretion. [`RtVec`] refuses instead, and reports it through
/// `overflowed()`.
pub type RtMidiEvents = RtVec<MidiEvent, RT_MIDI_CAPACITY>;
