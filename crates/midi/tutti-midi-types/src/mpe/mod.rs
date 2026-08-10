//! MIDI Polyphonic Expression (MPE, RP-053): zone layout, per-note channel
//! allocation, and the bend ranges the two channel roles carry.
//!
//! MPE spreads one instrument's notes across several MIDI 1.0 channels so each
//! note gets its own pitch bend, pressure and timbre — the expression MIDI 1.0
//! otherwise only has per *channel*. A [`MpeZoneConfig`] describes that layout,
//! [`MpeChannelVoiceMap`] hands out its member channels, and
//! [`PitchBendSensitivity`] carries the ranges.
//!
//! Types only: nothing here consumes an event stream. The state machine that
//! drives a zone from incoming MIDI lives in `tutti-midi-runtime`.

pub mod sensitivity;
pub mod voice_map;
pub mod zone;

pub use sensitivity::PitchBendSensitivity;
pub use voice_map::{MpeChannelVoiceMap, NoteRotationAllocator, ZoneInfo};
pub use zone::{MpeMode, MpeZone, MpeZoneConfig};
