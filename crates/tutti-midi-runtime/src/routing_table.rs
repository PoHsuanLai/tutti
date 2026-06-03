//! UI-thread writer for MIDI routing configuration.
//!
//! [`MidiRoutingTable`] now lives in `tutti-midi-types` alongside its
//! immutable [`MidiRoutingSnapshot`] counterpart (they are the mutable and
//! published halves of the same routing concept). Re-exported here for source
//! compatibility — downstream code referencing
//! `tutti_midi_runtime::routing_table::MidiRoutingTable` keeps resolving.

pub use tutti_midi_types::MidiRoutingTable;
