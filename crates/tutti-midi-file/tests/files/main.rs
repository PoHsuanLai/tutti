//! The file codecs: a Standard MIDI File and a MIDI 2.0 Clip File, read, written and
//! converted to a playable timeline, each against its specification.

mod authoring;
mod clip_model;
mod common;
mod smf_model;
mod smf_wire;
mod support;
mod to_clip;
