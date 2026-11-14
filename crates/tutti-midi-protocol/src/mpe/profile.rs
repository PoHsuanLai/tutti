//! The two Profile Specific Data values of the MIDI-CI MPE Profile (M2-120):
//! the [`ChannelResponse`] notification and the [`MpeFeatures`] a receiver
//! declares.
//!
//! The zone the Profile enables is an [`MpeZoneConfig`](crate::MpeZoneConfig)
//! built by [`profile`](crate::MpeZoneConfig::profile), and a Group's zones
//! together an [`MpeZones`](crate::MpeZones).

/// How a receiver's Member Channels take notes (M2-120 section 3.6).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ChannelResponse {
    /// A Member Channel plays any number of notes at once.
    #[default]
    Polyphonic,
    /// A Member Channel plays one note at a time, and a new one stops the old.
    Monophonic,
}

impl ChannelResponse {
    /// The data of a Channel Response Type Notification, the one Profile
    /// Specific Data message of the Profile (M2-120 section 3.6.3): one byte.
    #[inline]
    #[must_use]
    pub const fn to_data(self) -> [u8; 1] {
        match self {
            Self::Polyphonic => [0x00],
            Self::Monophonic => [0x01],
        }
    }

    /// Reads the data of a Channel Response Type Notification, or `None` for
    /// anything but its one byte, `0x00` or `0x01`.
    #[must_use]
    pub const fn from_data(data: &[u8]) -> Option<Self> {
        match data {
            [0x00] => Some(Self::Polyphonic),
            [0x01] => Some(Self::Monophonic),
            _ => None,
        }
    }
}

/// What a receiver supports of one of the two controls MPE can send at higher
/// resolution (M2-120 Table 8): Channel Pressure or the Third Dimension of
/// Control.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ControllerSupport {
    /// The control is not used.
    Unsupported,
    /// The Control Change form only: Channel Pressure, or Control Change 74.
    ControlChange,
    /// The bipolar Registered Controller form (M2-120 section 4.8).
    Bipolar,
}

impl ControllerSupport {
    const fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0x00 => Some(Self::Unsupported),
            0x01 => Some(Self::ControlChange),
            0x02 => Some(Self::Bipolar),
            _ => None,
        }
    }

    const fn to_byte(self) -> u8 {
        match self {
            Self::Unsupported => 0x00,
            Self::ControlChange => 0x01,
            Self::Bipolar => 0x02,
        }
    }
}

/// What a receiver supports of Pitch Bend (M2-120 Table 8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum PitchBendSupport {
    /// Pitch Bend is not used.
    Unsupported,
    /// Pitch Bend is used.
    Supported,
}

/// The optional features a receiver declares in its Reply to a Profile
/// Details Inquiry for the Profile's Optional Features target (M2-120 section
/// 3.7, Table 8): the four-byte Inquiry Target Data.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct MpeFeatures {
    /// Whether the receiver responds to the Channel Response Type
    /// Notification ([`ChannelResponse`]).
    pub channel_response_notification: bool,
    /// Pitch Bend.
    pub pitch_bend: PitchBendSupport,
    /// Channel Pressure.
    pub channel_pressure: ControllerSupport,
    /// The Third Dimension of Control.
    pub third_dimension: ControllerSupport,
}

impl MpeFeatures {
    /// The Inquiry Target of the Optional Features Details message.
    pub const INQUIRY_TARGET: u8 = 0x01;

    /// The four bytes of the Inquiry Target Data.
    #[must_use]
    pub fn to_data(self) -> [u8; 4] {
        [
            u8::from(self.channel_response_notification),
            match self.pitch_bend {
                PitchBendSupport::Unsupported => 0x00,
                PitchBendSupport::Supported => 0x01,
            },
            self.channel_pressure.to_byte(),
            self.third_dimension.to_byte(),
        ]
    }

    /// Reads the Inquiry Target Data, or `None` for anything but four bytes
    /// of defined values: a reserved bit in the bitmap or an undefined
    /// enumerator is refused, not read as the nearest feature.
    #[must_use]
    pub const fn from_data(data: &[u8]) -> Option<Self> {
        let [flags, pitch_bend, pressure, third] = *data else {
            return None;
        };
        if flags > 0x01 {
            return None;
        }
        let pitch_bend = match pitch_bend {
            0x00 => PitchBendSupport::Unsupported,
            0x01 => PitchBendSupport::Supported,
            _ => return None,
        };
        let (Some(channel_pressure), Some(third_dimension)) = (
            ControllerSupport::from_byte(pressure),
            ControllerSupport::from_byte(third),
        ) else {
            return None;
        };
        Some(Self {
            channel_response_notification: flags == 0x01,
            pitch_bend,
            channel_pressure,
            third_dimension,
        })
    }
}

#[cfg(test)]
mod tests;
