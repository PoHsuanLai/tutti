//! MIDI that arrives from a position on a timeline, rather than off a wire.
//!
//! Both types here implement [`MidiUnitIn`](tutti_midi_types::MidiUnitIn) and
//! answer the same question — *what does this unit hear during the beat range
//! this block covers?* — for the live and offline cases:
//!
//! - [`MidiClipSource`] reads a sorted event list against a live [`Timeline`],
//!   stamping each event's `frame_offset` from its beat. Installed onto a
//!   [`MidiInPort`](crate::MidiInPort), where it *layers* over the live mailbox
//!   so a clip plays without silencing the keyboard.
//! - [`MidiSnapshotReader`] reads a [`MidiSnapshot`] against an offline
//!   timeline, for export. One snapshot holds every unit's stream behind a
//!   per-unit cursor, which is what makes it a `MidiUnitIn` rather than a
//!   `MidiIn` — it has no "everything pending" to answer.
//!
//! [`MidiSnapshot`] is the storage both share a vocabulary with: its
//! [`TimedMidiEvent`] is the same type a clip's event list holds, so events move
//! between the two without repacking.
//!
//! [`Timeline`]: tutti_core::transport::Timeline

pub mod clip_node;
pub mod clip_player;
pub mod snapshot;
pub mod snapshot_reader;

pub use clip_node::{MidiClipControls, MidiClipNode, CLIP_EVENT_CAPACITY};
pub use clip_player::{MidiClipSource, TimedClipEvent};
pub use snapshot::{MidiSnapshot, TimedMidiEvent};
pub use snapshot_reader::MidiSnapshotReader;
