//! Shared MIDI event collection width for the plugin host boundary.
//!
//! The cap on a plugin's per-block MIDI in/out. Named here (not on
//! `tutti-plugin`'s IPC layer) so the format host crates and the shared
//! [`ProcessOutput`](crate::ProcessOutput) can speak the type without
//! depending on `tutti-plugin`.

use arrayvec::ArrayVec;
use tutti_types::RtVec;

use crate::MidiEvent;

/// The most events a [`MidiEventVec`] holds: the cap on one block's MIDI in
/// either direction across the plugin boundary.
pub const MIDI_STACK_CAPACITY: usize = 256;

/// A per-block MIDI event list, inline and **capped** at
/// [`MIDI_STACK_CAPACITY`].
///
/// An [`ArrayVec`]: the storage is the value itself, so building, moving and
/// dropping one never touches the heap, which is what lets a block's MIDI move
/// by value through a queue and be dropped on the audio thread. It cannot
/// grow: write with `try_push` (or [`ProcessOutput::emit_midi`]) and drop
/// what does not fit. `push`, `extend` and `collect` **panic** past the cap,
/// so do not use them on events whose count the host does not control.
///
/// [`ProcessOutput::emit_midi`]: crate::ProcessOutput::emit_midi
///
/// For a pool filled inside `process` that reports its overflow, use
/// [`RtMidiEvents`].
pub type MidiEventVec = ArrayVec<MidiEvent, MIDI_STACK_CAPACITY>;

/// Cap for a MIDI event pool filled on the audio thread.
///
/// Smaller than [`MIDI_STACK_CAPACITY`] because the two answer different
/// questions: that one bounds a block's MIDI across the plugin boundary (a
/// *wire message*'s worth), while this one bounds one format host's per-block
/// output pool. 64 events in one block is already far above what a plugin
/// emits in normal use.
pub const RT_MIDI_CAPACITY: usize = 64;

/// A per-block MIDI event pool filled inside `process` and lent back out as
/// `&[MidiEvent]`.
///
/// Prefer this over a bare `Vec` for anything a plugin's output drains
/// into: the host does not control how many events a plugin emits, so an
/// unbounded pool puts a `malloc` in the audio callback at the plugin's
/// discretion. [`RtVec`] refuses instead, and reports it through
/// `overflowed()`.
pub type RtMidiEvents = RtVec<MidiEvent, RT_MIDI_CAPACITY>;
