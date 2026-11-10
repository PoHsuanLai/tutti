//! MIDI Polyphonic Expression (MPE, RP-053): zone layout, per-note channel
//! allocation, and the bend ranges the two channel roles carry.
//!
//! MPE spreads one instrument's notes across several MIDI 1.0 channels so each
//! note gets its own pitch bend, pressure and timbre — the expression MIDI 1.0
//! otherwise only has per *channel*. A [`MpeZoneConfig`] describes that layout,
//! [`MpeChannelVoiceMap`] hands out its member channels, and
//! [`PitchBendSensitivity`](tutti_midi_protocol::PitchBendSensitivity) carries the
//! ranges. The per-note range a stream sets on each channel is
//! `tutti_plugin_types::PerNoteBendRanges`, read by the plugin hosts.
//!
//! Types and that one fold: the state machine that drives a zone from
//! incoming MIDI lives in `tutti-midi-runtime`.

pub mod voice_map;
pub mod zone;

pub use voice_map::{MpeChannelVoiceMap, NoteRotationAllocator, ZoneInfo};
pub use zone::{MpeMode, MpeZone, MpeZoneConfig};
