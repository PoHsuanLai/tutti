use super::PitchBendSensitivity;
use tutti_types::{MidiChannel, MidiGroup};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MpeZone {
    Lower,
    Upper,
    SingleChannel(u8),
}

/// MPE's two default pitch-bend ranges (RP-053): the Master channel bends the
/// whole zone by ±48 semitones, a Member channel bends its one note by ±2.
///
/// They are separate state, not one value seen from two angles — RPN 0 arrives
/// per channel, so a controller can legitimately set them independently.
const MASTER_PITCH_BEND_SEMITONES: u8 = 48;
const MEMBER_PITCH_BEND_SEMITONES: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MpeZoneConfig {
    pub zone: MpeZone,
    pub master_channel: u8,
    pub member_count: u8,
    /// Bend range for the zone's **master** channel — applies to every note in
    /// the zone at once. RP-053 default ±48 semitones.
    pub master_pitch_bend_range: PitchBendSensitivity,
    /// Bend range for a **member** channel — applies to that channel's single
    /// note. RP-053 default ±2 semitones; using the master's ±48 here bends a
    /// per-note gesture 24× too far.
    pub member_pitch_bend_range: PitchBendSensitivity,
    pub enabled: bool,
}

impl MpeZoneConfig {
    /// Master = Ch1 (0), members = Ch2..Ch(1+count), RP-053 default bend ranges.
    pub fn lower(member_count: u8) -> Self {
        Self {
            zone: MpeZone::Lower,
            master_channel: 0, // Ch1 (0-indexed)
            member_count: member_count.clamp(1, 15),
            master_pitch_bend_range: PitchBendSensitivity::from_semitones(
                MASTER_PITCH_BEND_SEMITONES,
            ),
            member_pitch_bend_range: PitchBendSensitivity::from_semitones(
                MEMBER_PITCH_BEND_SEMITONES,
            ),
            enabled: true,
        }
    }

    /// Master = Ch16 (15), members count down from Ch15, RP-053 default bend ranges.
    pub fn upper(member_count: u8) -> Self {
        Self {
            zone: MpeZone::Upper,
            master_channel: 15, // Ch16 (0-indexed)
            member_count: member_count.clamp(1, 15),
            master_pitch_bend_range: PitchBendSensitivity::from_semitones(
                MASTER_PITCH_BEND_SEMITONES,
            ),
            member_pitch_bend_range: PitchBendSensitivity::from_semitones(
                MEMBER_PITCH_BEND_SEMITONES,
            ),
            enabled: true,
        }
    }

    /// Non-MPE mode: one channel, standard ±2 bend. There are no member
    /// channels, so both ranges are the same ±2.
    pub fn single_channel(channel: u8) -> Self {
        let range = PitchBendSensitivity::from_semitones(MEMBER_PITCH_BEND_SEMITONES);
        Self {
            zone: MpeZone::SingleChannel(channel.min(15)),
            master_channel: channel.min(15),
            member_count: 0,
            master_pitch_bend_range: range,
            member_pitch_bend_range: range,
            enabled: true,
        }
    }

    /// The bend range that applies to `channel` — the master's for the master
    /// channel, the member's for anything else in the zone.
    ///
    /// Use this rather than reading a field directly: picking the wrong one is
    /// a silent 24× pitch error.
    #[inline]
    pub fn pitch_bend_range_for(&self, channel: u8) -> PitchBendSensitivity {
        if self.is_master_channel(channel) {
            self.master_pitch_bend_range
        } else {
            self.member_pitch_bend_range
        }
    }

    /// Override the **master** channel's bend range (RPN 0 on the master).
    pub fn with_master_pitch_bend_range(mut self, semitones: u8) -> Self {
        self.master_pitch_bend_range = PitchBendSensitivity::from_semitones(semitones);
        self
    }

    /// Override the **member** channels' bend range (RPN 0 on a member).
    pub fn with_member_pitch_bend_range(mut self, semitones: u8) -> Self {
        self.member_pitch_bend_range = PitchBendSensitivity::from_semitones(semitones);
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
                // Members: Ch15 (14) down to Ch(16-member_count). Saturating so
                // this does not depend on the constructors' clamp holding.
                let lowest_member = 15u8.saturating_sub(self.member_count);
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
            MpeZone::Upper => 15u8.saturating_sub(self.member_count)..=14,
            MpeZone::SingleChannel(ch) => ch..=ch,
        }
    }

    /// Encode this zone as an **MPE Configuration Message** (MCM): a MIDI 2.0
    /// Registered Controller (RPN) on the zone's *master* channel, bank
    /// [`RPN_BANK_MPE`], index [`RPN_INDEX_MCM`], data = member count. Per
    /// RP-053 / M2-104, the master channel (Ch1 lower / Ch16 upper) is what a
    /// receiver reads to know which zone is being configured; `member_count`
    /// = `0` would disable the zone.
    ///
    /// The 7-bit member count sits in the top 7 bits of the 32-bit data field
    /// (MSB-aligned, the spec's 7→32 convention) so it round-trips exactly.
    pub fn to_mcm(&self) -> crate::ump::MidiEvent {
        crate::ump::MidiEvent::registered_controller(
            MidiGroup::FIRST,
            MidiChannel::new(self.master_channel),
            crate::ump::RPN_BANK_MPE,
            crate::ump::RPN_INDEX_MCM,
            (self.member_count as u32) << 25,
        )
    }

    /// Decode an MCM back into `(master_channel, member_count)`, if `event` is an
    /// MPE Configuration Message (RPN bank `0x00`, index `0x06`). Returns `None`
    /// for any other message. The zone side (lower vs upper) is inferred from the
    /// master channel by the caller (Ch0 → lower, Ch15 → upper).
    ///
    /// A member count is rejected — not clamped — when the zone cannot hold it:
    /// a zone has at most 15 member channels, and RP-053 only permits an MCM on
    /// Ch1 or Ch16. Clamping would silently reconfigure the zone to something
    /// the sender never asked for; `None` lets the caller ignore the message and
    /// keep the zone it had.
    pub fn from_mcm(event: &crate::ump::MidiEvent) -> Option<(u8, u8)> {
        use midi2::channel_voice2::ChannelVoice2;
        use midi2::{Channeled, UmpMessage};
        let UmpMessage::ChannelVoice2(ChannelVoice2::RegisteredController(m)) =
            UmpMessage::try_from(event.data_words()).ok()?
        else {
            return None;
        };
        if u8::from(m.bank()) != crate::ump::RPN_BANK_MPE
            || u8::from(m.index()) != crate::ump::RPN_INDEX_MCM
        {
            return None;
        }
        let master_channel = u8::from(m.channel());
        // RP-053: an MCM is sent on Ch1 (lower zone) or Ch16 (upper zone).
        if master_channel != 0 && master_channel != 15 {
            return None;
        }
        // `0` is the spec's "disable this zone"; above 15 there are not that
        // many channels to give out.
        let member_count = (m.controller_data() >> 25) as u8;
        if member_count > 15 {
            return None;
        }
        Some((master_channel, member_count))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
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
    fn mcm_round_trips_lower_zone() {
        // MpeZoneConfig → MCM RPN → (master_channel, member_count) identity.
        let cfg = MpeZoneConfig::lower(10);
        let (master, members) = MpeZoneConfig::from_mcm(&cfg.to_mcm()).expect("is an MCM");
        assert_eq!(master, 0); // lower zone master = Ch1 (0-indexed)
        assert_eq!(members, 10);
    }

    #[test]
    fn mcm_round_trips_upper_zone() {
        let cfg = MpeZoneConfig::upper(7);
        let (master, members) = MpeZoneConfig::from_mcm(&cfg.to_mcm()).expect("is an MCM");
        assert_eq!(master, 15); // upper zone master = Ch16
        assert_eq!(members, 7);
    }

    #[test]
    fn from_mcm_rejects_non_mcm() {
        // A plain note-on is not an MCM.
        assert!(MpeZoneConfig::from_mcm(&crate::ump::MidiEvent::note_on(
            MidiGroup::FIRST,
            MidiChannel::FIRST,
            60,
            0x8000
        ))
        .is_none());
        // An RPN with a different index is not an MCM.
        let other_rpn = crate::ump::MidiEvent::registered_controller(
            MidiGroup::FIRST,
            MidiChannel::FIRST,
            0x00,
            0x00,
            0,
        );
        assert!(MpeZoneConfig::from_mcm(&other_rpn).is_none());
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
        // Standard non-MPE default: ±2, and with no member channels both roles
        // resolve to the same range.
        assert_eq!(
            config.master_pitch_bend_range,
            PitchBendSensitivity::from_semitones(2)
        );
        assert_eq!(
            config.member_pitch_bend_range,
            PitchBendSensitivity::from_semitones(2)
        );

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
        // Each role is set independently; setting one leaves the other alone.
        let config = MpeZoneConfig::lower(5).with_master_pitch_bend_range(96);
        assert_eq!(
            config.master_pitch_bend_range,
            PitchBendSensitivity::from_semitones(96)
        );
        assert_eq!(
            config.member_pitch_bend_range,
            PitchBendSensitivity::from_semitones(2),
            "member range untouched"
        );
        assert_eq!(config.member_count, 5); // Other fields unchanged

        let config = MpeZoneConfig::lower(5).with_member_pitch_bend_range(12);
        assert_eq!(
            config.member_pitch_bend_range,
            PitchBendSensitivity::from_semitones(12)
        );
        assert_eq!(
            config.master_pitch_bend_range,
            PitchBendSensitivity::from_semitones(48),
            "master range untouched"
        );
    }

    #[test]
    fn zones_default_to_rp053_ranges_per_role() {
        // RP-053: ±48 on the master channel, ±2 on member channels. Using the
        // master's range on a member bends a per-note gesture 24x too far.
        for config in [MpeZoneConfig::lower(7), MpeZoneConfig::upper(7)] {
            assert_eq!(
                config.master_pitch_bend_range,
                PitchBendSensitivity::from_semitones(48)
            );
            assert_eq!(
                config.member_pitch_bend_range,
                PitchBendSensitivity::from_semitones(2)
            );

            // …and resolving by channel picks the right one.
            let master = config.master_channel;
            assert_eq!(
                config.pitch_bend_range_for(master),
                config.master_pitch_bend_range
            );
            let member = *config.member_channel_range().start();
            assert_eq!(
                config.pitch_bend_range_for(member),
                config.member_pitch_bend_range,
                "member channel {member} must use the member range"
            );
        }
    }

    #[test]
    fn from_mcm_rejects_out_of_range_configurations() {
        use crate::ump::MidiEvent;
        // RP-053 puts the MCM on Ch1 or Ch16 only; RPN 0x00/0x06 elsewhere is
        // ordinary parameter traffic and must not be read as configuration.
        let on_ch5 = MidiEvent::registered_controller(
            MidiGroup::FIRST,
            MidiChannel::new(5),
            crate::ump::RPN_BANK_MPE,
            crate::ump::RPN_INDEX_MCM,
            3u32 << 25,
        );
        assert_eq!(MpeZoneConfig::from_mcm(&on_ch5), None);

        // A zone has at most 15 member channels — reject rather than clamp, so a
        // bogus count leaves the existing zone alone.
        let too_many = MidiEvent::registered_controller(
            MidiGroup::FIRST,
            MidiChannel::FIRST,
            crate::ump::RPN_BANK_MPE,
            crate::ump::RPN_INDEX_MCM,
            100u32 << 25,
        );
        assert_eq!(MpeZoneConfig::from_mcm(&too_many), None);

        // The valid case still decodes.
        assert_eq!(
            MpeZoneConfig::from_mcm(&MpeZoneConfig::lower(7).to_mcm()),
            Some((0, 7))
        );
        assert_eq!(
            MpeZoneConfig::from_mcm(&MpeZoneConfig::upper(4).to_mcm()),
            Some((15, 4))
        );
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
