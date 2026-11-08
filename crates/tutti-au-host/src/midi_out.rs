//! MIDI output from the AU: the callback the AU hands generated MIDI to.
//!
//! The registration, the packet-list walk and the sink type are
//! `tutti-au-ffi`'s; they are re-exported here under the paths this crate has
//! always used.

#![cfg(target_os = "macos")]

pub use tutti_au_ffi::{AuMidiOutput, MidiOutSink, MidiOutputInfo};
pub(crate) use tutti_au_ffi::{install_midi_output as install, midi_output_info};
