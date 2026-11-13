//! MIDI Polyphonic Expression (MPE, RP-053): the zone layout of an MPE
//! instrument and the MPE Configuration Message that declares it.
//!
//! MPE spreads one instrument's notes across several MIDI 1.0 channels so each
//! note gets its own pitch bend, pressure and timbre, the expression MIDI 1.0
//! otherwise only has per *channel*. A [`MpeZoneConfig`] is one zone's layout,
//! [`MpeZones`] the one or two zones an instrument occupies, and [`MpeMode`]
//! whether MPE is in play at all. The two bend ranges the channel roles carry
//! are [`PitchBendSensitivity`](crate::PitchBendSensitivity)s. Every type
//! refuses what RP-053 does at its constructor, and a persisted one is read
//! back through the same check.
//!
//! The MIDI-CI MPE Profile (M2-120) replaces the Configuration Message with a
//! zone per enabled Profile: the same [`MpeZoneConfig`] and [`MpeZones`], built
//! with [`MpeZoneConfig::profile`], with the Profile's two Specific Data values
//! ([`ChannelResponse`], [`MpeFeatures`]). [`MpeSpec`] names which
//! specification a zone follows.
//!
//! These are values. The state machine that drives a zone from incoming MIDI
//! (which member channel holds which note) is `tutti-midi-runtime`'s
//! `MpeIngest`.

mod profile;
mod zone;

pub use profile::{ChannelResponse, ControllerSupport, MpeFeatures, PitchBendSupport};
pub use zone::{MpeMode, MpeSpec, MpeZone, MpeZoneConfig, MpeZones};
