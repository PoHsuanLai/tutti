//! Shared MIDI event collection width for the plugin host boundary.
//!
//! The cap on a plugin's per-block MIDI in/out. Named here (not on
//! `tutti-plugin`'s IPC layer) so the format host crates and the shared
//! [`ProcessOutput`](crate::ProcessOutput) can speak the type without
//! depending on `tutti-plugin`.

use arrayvec::ArrayVec;
use tutti_rt::RtVec;

use tutti_graph::Offset;
use tutti_ump::Ump;

/// One MIDI packet a plugin reported, with the offset it reported it at.
///
/// The offset is raw: a position the plugin gave in the block it processed,
/// unchecked against any block. The types stop at this IPC boundary
/// (CLAUDE.md, "Where the types stop"): it becomes an [`Offset`] where the
/// host clamps it into its block. The same shape carries a block's input
/// across the plugin IPC, where the server turns it into `(Offset, Ump)`.
///
/// It names no `tutti-graph` type, and nothing the wire serialises may: the
/// IPC layer depends on `tutti-ump` for the packet and on nothing of the graph.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WireMidi {
    /// Frames from the start of the block, as the plugin reported them.
    pub offset: u32,
    /// The packet.
    pub ump: Ump,
}

impl WireMidi {
    /// A packet at a checked block `offset`.
    #[must_use]
    pub fn at(offset: Offset, ump: Ump) -> WireMidi {
        WireMidi {
            offset: offset.get(),
            ump,
        }
    }
}

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
pub type MidiEventVec = ArrayVec<WireMidi, MIDI_STACK_CAPACITY>;

/// Cap for a MIDI event pool filled on the audio thread.
///
/// Smaller than [`MIDI_STACK_CAPACITY`] because the two answer different
/// questions: that one bounds a block's MIDI across the plugin boundary (a
/// *wire message*'s worth), while this one bounds one format host's per-block
/// output pool. 64 events in one block is already far above what a plugin
/// emits in normal use.
pub const RT_MIDI_CAPACITY: usize = 64;

/// A per-block MIDI event pool filled inside `process` and lent back out as
/// `&[WireMidi]`.
///
/// Prefer this over a bare `Vec` for anything a plugin's output drains
/// into: the host does not control how many events a plugin emits, so an
/// unbounded pool puts a `malloc` in the audio callback at the plugin's
/// discretion. [`RtVec`] refuses instead, and reports it through
/// `is_overflowed()`.
pub type RtMidiEvents = RtVec<WireMidi, RT_MIDI_CAPACITY>;
