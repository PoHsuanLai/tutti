//! What a plugin may ask about the track it sits on: the host's answer, one
//! format-neutral value.
//!
//! [`TrackInfo`] is the value; [`Control::set_track_info`](crate::Control::set_track_info)
//! hands a new one to the plugin's host. A format with a place for it (CLAP's
//! `track-info`) answers the plugin's reads from it and tells the plugin when
//! it changes; a format without keeps the default and drops it.

use tutti_types::ChannelLayout;

/// An 8-bit-per-channel colour with opacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TrackColor {
    /// Red.
    pub red: u8,
    /// Green.
    pub green: u8,
    /// Blue.
    pub blue: u8,
    /// Opacity; 255 is opaque.
    pub alpha: u8,
}

/// What kind of track a plugin sits on.
///
/// Exhaustive: the four kinds a mixer has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum TrackKind {
    /// An ordinary track.
    #[default]
    Track,
    /// A return (send) track.
    Return,
    /// A group bus.
    Bus,
    /// The master bus.
    Master,
}

/// What the host says of the track a plugin is on. Every field is optional
/// information: a field the host leaves unset is one the plugin is told is
/// unknown.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TrackInfo {
    /// The track's name.
    pub name: Option<String>,
    /// The track's colour.
    pub color: Option<TrackColor>,
    /// The width of the track's audio, mono and stereo being named.
    pub audio: Option<ChannelLayout>,
    /// The kind of track.
    pub kind: TrackKind,
}
