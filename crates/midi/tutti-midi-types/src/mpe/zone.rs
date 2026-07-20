#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MpeZone {
    Lower,
    Upper,
    SingleChannel(u8),
}

/// Per-note pitch bend range as the spec's 7.25 fixed-point semitone value
/// (M2-104 §7.4.13, RPN #00/07 "Sensitivity of Per-Note Pitch Bend"): 7 integer
/// bits of semitones in the high word, 25 fractional bits below. This is the wire
/// form the MPE Configuration Message (RPN 0x0006) carries — modelling it as the
/// fixed-point value, not a bare `u8`, is what lets it round-trip through UMP.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PitchBendSensitivity(u32);

impl PitchBendSensitivity {
    const FRAC_BITS: u32 = 25;

    /// Whole-semitone sensitivity (the common MPE case, e.g. 48).
    #[inline]
    pub const fn from_semitones(semitones: u8) -> Self {
        Self((semitones as u32) << Self::FRAC_BITS)
    }

    /// The raw 32-bit RPN data field.
    #[inline]
    pub const fn to_rpn_bits(self) -> u32 {
        self.0
    }

    /// From the raw 32-bit RPN data field.
    #[inline]
    pub const fn from_rpn_bits(bits: u32) -> Self {
        Self(bits)
    }

    /// Sensitivity in semitones as a float (integer + fractional parts).
    #[inline]
    pub fn as_semitones_f32(self) -> f32 {
        self.0 as f32 / (1u32 << Self::FRAC_BITS) as f32
    }
}

#[derive(Clone, Copy, Debug)]
pub struct MpeZoneConfig {
    pub zone: MpeZone,
    pub master_channel: u8,
    pub member_count: u8,
    pub pitch_bend_range: PitchBendSensitivity,
    pub enabled: bool,
}

impl MpeZoneConfig {
    /// Master = Ch1 (0), members = Ch2..Ch(1+count). Default pitch bend range: 48 semitones.
    pub fn lower(member_count: u8) -> Self {
        let member_count = member_count.clamp(1, 15);
        Self {
            zone: MpeZone::Lower,
            master_channel: 0, // Ch1 (0-indexed)
            member_count,
            pitch_bend_range: PitchBendSensitivity::from_semitones(48),
            enabled: true,
        }
    }

    /// Master = Ch16 (15), members count down from Ch15. Default pitch bend range: 48 semitones.
    pub fn upper(member_count: u8) -> Self {
        let member_count = member_count.clamp(1, 15);
        Self {
            zone: MpeZone::Upper,
            master_channel: 15, // Ch16 (0-indexed)
            member_count,
            pitch_bend_range: PitchBendSensitivity::from_semitones(48),
            enabled: true,
        }
    }

    /// Non-MPE mode. Default pitch bend range: 2 semitones.
    pub fn single_channel(channel: u8) -> Self {
        Self {
            zone: MpeZone::SingleChannel(channel.min(15)),
            master_channel: channel.min(15),
            member_count: 0,
            pitch_bend_range: PitchBendSensitivity::from_semitones(2), // Standard non-MPE default
            enabled: true,
        }
    }

    pub fn with_pitch_bend_range(mut self, semitones: u8) -> Self {
        self.pitch_bend_range = PitchBendSensitivity::from_semitones(semitones);
        self
    }

    #[inline]
    pub fn is_master_channel(&self, channel: u8) -> bool {
        channel == self.master_channel
    }

    #[inline]
    pub fn is_member_channel(&self, channel: u8) -> bool {
        match self.zone {
            MpeZone::Lower => {
                // Members: Ch2 (1) to Ch(1+member_count)
                channel >= 1 && channel <= self.member_count
            }
            MpeZone::Upper => {
                // Members: Ch15 (14) down to Ch(16-member_count)
                let lowest_member = 15 - self.member_count;
                channel >= lowest_member && channel <= 14
            }
            MpeZone::SingleChannel(_) => false,
        }
    }

    #[inline]
    pub fn handles_channel(&self, channel: u8) -> bool {
        self.is_master_channel(channel) || self.is_member_channel(channel)
    }

    pub fn member_channel_range(&self) -> core::ops::RangeInclusive<u8> {
        match self.zone {
            MpeZone::Lower => 1..=self.member_count,
            MpeZone::Upper => (15 - self.member_count)..=14,
            MpeZone::SingleChannel(ch) => ch..=ch,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub enum MpeMode {
    #[default]
    Disabled,
    LowerZone(MpeZoneConfig),
    UpperZone(MpeZoneConfig),
    DualZone {
        lower: MpeZoneConfig,
        upper: MpeZoneConfig,
    },
    /// Single-channel **Note Number Rotation**: full 128-note polyphony on one
    /// channel (no zones / member-channel spreading). Each note-on mints a
    /// distinct host-internal note id so same-pitch notes get their own voices.
    /// See [`NoteRotationAllocator`](super::NoteRotationAllocator).
    SingleChannelRotation {
        channel: u8,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pitch_bend_sensitivity_rpn_roundtrip() {
        for s in [2u8, 24, 48, 96] {
            let sens = PitchBendSensitivity::from_semitones(s);
            assert_eq!(
                PitchBendSensitivity::from_rpn_bits(sens.to_rpn_bits()),
                sens
            );
            assert_eq!(sens.as_semitones_f32(), f32::from(s));
        }
    }

    #[test]
    fn test_zone_config_lower() {
        let config = MpeZoneConfig::lower(10);
        assert_eq!(config.master_channel, 0);
        assert_eq!(config.member_count, 10);
        assert!(config.is_master_channel(0));
        assert!(!config.is_master_channel(1));
        assert!(config.is_member_channel(1));
        assert!(config.is_member_channel(10));
        assert!(!config.is_member_channel(11));
        assert!(!config.is_member_channel(0));
    }

    #[test]
    fn test_zone_config_upper() {
        let config = MpeZoneConfig::upper(5);
        assert_eq!(config.master_channel, 15);
        assert_eq!(config.member_count, 5);
        assert!(config.is_master_channel(15));
        assert!(!config.is_master_channel(14));
        // Upper zone: members are 15-member_count to 14 (i.e., 10 to 14)
        assert!(config.is_member_channel(14));
        assert!(config.is_member_channel(10));
        assert!(!config.is_member_channel(9));
        assert!(!config.is_member_channel(15));
    }

    #[test]
    fn test_single_channel_config() {
        let config = MpeZoneConfig::single_channel(5);
        assert_eq!(config.zone, MpeZone::SingleChannel(5));
        assert_eq!(config.master_channel, 5);
        assert_eq!(config.member_count, 0);
        assert_eq!(
            config.pitch_bend_range,
            PitchBendSensitivity::from_semitones(2)
        ); // Standard non-MPE default

        // Master channel is the single channel
        assert!(config.is_master_channel(5));
        assert!(!config.is_master_channel(0));

        // No member channels in single-channel mode
        assert!(!config.is_member_channel(5));
        assert!(!config.is_member_channel(0));

        // handles_channel: only the master
        assert!(config.handles_channel(5));
        assert!(!config.handles_channel(4));
    }

    #[test]
    fn test_single_channel_clamps_to_15() {
        let config = MpeZoneConfig::single_channel(200);
        assert_eq!(config.master_channel, 15);
        assert_eq!(config.zone, MpeZone::SingleChannel(15));
    }

    #[test]
    fn test_with_pitch_bend_range() {
        let config = MpeZoneConfig::lower(5).with_pitch_bend_range(96);
        assert_eq!(
            config.pitch_bend_range,
            PitchBendSensitivity::from_semitones(96)
        );
        assert_eq!(config.member_count, 5); // Other fields unchanged
    }

    #[test]
    fn test_member_channel_range_lower() {
        let config = MpeZoneConfig::lower(5);
        let range = config.member_channel_range();
        assert_eq!(*range.start(), 1);
        assert_eq!(*range.end(), 5);
    }

    #[test]
    fn test_member_channel_range_upper() {
        let config = MpeZoneConfig::upper(5);
        let range = config.member_channel_range();
        // Upper zone: 15-5=10 to 14
        assert_eq!(*range.start(), 10);
        assert_eq!(*range.end(), 14);
    }

    #[test]
    fn test_member_channel_range_single() {
        let config = MpeZoneConfig::single_channel(7);
        let range = config.member_channel_range();
        // Single channel range is just ch..=ch
        assert_eq!(*range.start(), 7);
        assert_eq!(*range.end(), 7);
    }

    #[test]
    fn test_member_count_clamped() {
        // Lower zone: member_count clamped to 1..=15
        let config = MpeZoneConfig::lower(0);
        assert_eq!(config.member_count, 1);
        let config = MpeZoneConfig::lower(20);
        assert_eq!(config.member_count, 15);

        // Upper zone: same clamping
        let config = MpeZoneConfig::upper(0);
        assert_eq!(config.member_count, 1);
        let config = MpeZoneConfig::upper(20);
        assert_eq!(config.member_count, 15);
    }

    #[test]
    fn test_lower_zone_max_members() {
        // 15 members: master=0, members=1-15
        let config = MpeZoneConfig::lower(15);
        assert!(config.is_member_channel(1));
        assert!(config.is_member_channel(15));
        assert!(!config.is_member_channel(0)); // Master, not member
    }

    #[test]
    fn test_upper_zone_max_members() {
        // 15 members: master=15, members=0-14
        let config = MpeZoneConfig::upper(15);
        assert!(config.is_member_channel(0));
        assert!(config.is_member_channel(14));
        assert!(!config.is_member_channel(15)); // Master, not member
    }
}
