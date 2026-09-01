//! MPE zone layout (RP-053): which of the sixteen MIDI 1.0 channels are master
//! and which are members, and the MPE Configuration Message that declares it.
//!
//! Every channel number here is **0-indexed** — `0` is the channel a device's
//! front panel calls "Ch1" — because that is what UMP carries. The one place the
//! distinction bites is the two zone anchors: the lower zone's master is `0` and
//! the upper zone's is `15`.

use super::PitchBendSensitivity;
use tutti_types::{MidiChannel, MidiGroup};

/// Which of the two RP-053 zones (or neither) a configuration describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum MpeZone {
    /// Master on channel 0, members counting up from channel 1.
    Lower,
    /// Master on channel 15, members counting down from channel 14.
    Upper,
    /// Not MPE at all: one ordinary channel, no member spreading.
    SingleChannel(u8),
}

/// MPE's two default pitch-bend ranges (RP-053): the Master channel bends the
/// whole zone by ±48 semitones, a Member channel bends its one note by ±2.
///
/// They are separate state, not one value seen from two angles — RPN 0 arrives
/// per channel, so a controller can legitimately set them independently.
const MASTER_PITCH_BEND_SEMITONES: u8 = 48;
const MEMBER_PITCH_BEND_SEMITONES: u8 = 2;

/// One zone's layout and bend ranges — the whole description of how an MPE
/// instrument occupies the sixteen channels.
///
/// Prefer the constructors ([`lower`](Self::lower), [`upper`](Self::upper),
/// [`single_channel`](Self::single_channel)) over building this literally: they
/// place the master channel where RP-053 requires and install the two default
/// bend ranges, which differ by a factor of 24.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct MpeZoneConfig {
    /// Which zone this is, and hence which direction members count in.
    pub zone: MpeZone,
    /// The zone's master channel, 0-indexed: `0` for a lower zone, `15` for an
    /// upper one. Zone-wide messages (bend, pressure) arrive here.
    pub master_channel: u8,
    /// How many member channels the zone claims, 1..=15. A count of `0` in an
    /// incoming MCM means "disable the zone" and is not stored here.
    pub member_count: u8,
    /// Bend range for the zone's **master** channel — applies to every note in
    /// the zone at once. RP-053 default ±48 semitones.
    pub master_pitch_bend_range: PitchBendSensitivity,
    /// Bend range for a **member** channel — applies to that channel's single
    /// note. RP-053 default ±2 semitones; using the master's ±48 here bends a
    /// per-note gesture 24× too far.
    pub member_pitch_bend_range: PitchBendSensitivity,
    /// Whether the zone is active. A disabled zone keeps its layout so a host can
    /// restore it without re-deriving the channel assignment.
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

    /// Reports whether `channel` is this zone's master.
    #[inline]
    pub fn is_master_channel(&self, channel: u8) -> bool {
        channel == self.master_channel
    }

    /// Reports whether `channel` is one of this zone's member channels.
    ///
    /// False for the master channel and false for every channel of a
    /// [`MpeZone::SingleChannel`] configuration, which has no members at all.
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

    /// Reports whether `channel` belongs to this zone in either role.
    #[inline]
    pub fn handles_channel(&self, channel: u8) -> bool {
        self.is_master_channel(channel) || self.is_member_channel(channel)
    }

    /// The inclusive span of member channels, in ascending order regardless of
    /// which direction the zone counts.
    ///
    /// A [`MpeZone::SingleChannel`] configuration yields its one channel, so the
    /// range is never empty even where [`is_member_channel`](Self::is_member_channel)
    /// answers `false` for every value in it.
    pub fn member_channel_range(&self) -> core::ops::RangeInclusive<u8> {
        match self.zone {
            MpeZone::Lower => 1..=self.member_count,
            MpeZone::Upper => 15u8.saturating_sub(self.member_count)..=14,
            MpeZone::SingleChannel(ch) => ch..=ch,
        }
    }

    /// Encode this zone as an **MPE Configuration Message** (MCM): a MIDI 2.0
    /// Registered Controller (RPN) on the zone's *master* channel, bank
    /// [`RPN_BANK_MPE`](crate::ump::RPN_BANK_MPE), index
    /// [`RPN_INDEX_MCM`](crate::ump::RPN_INDEX_MCM), data = member count. Per
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

/// How an instrument occupies the sixteen channels as a whole — the zone
/// arrangement, rather than one zone's layout.
///
/// [`MpeZoneConfig`] describes *a* zone; this says how many there are and
/// whether MPE is in play at all.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum MpeMode {
    /// No MPE. Channels behave as ordinary MIDI 1.0 channels.
    #[default]
    Disabled,
    /// One zone anchored at channel 0, counting up.
    LowerZone(MpeZoneConfig),
    /// One zone anchored at channel 15, counting down.
    UpperZone(MpeZoneConfig),
    /// Both zones at once — two instruments sharing the sixteen channels.
    ///
    /// The two member spans must not overlap; RP-053 makes that the sender's
    /// responsibility, and nothing here checks it.
    DualZone {
        /// The zone anchored at channel 0.
        lower: MpeZoneConfig,
        /// The zone anchored at channel 15.
        upper: MpeZoneConfig,
    },
    /// Single-channel **Note Number Rotation**: full 128-note polyphony on one
    /// channel (no zones / member-channel spreading). Each note-on mints a
    /// distinct host-internal note id so same-pitch notes get their own voices.
    /// See [`NoteRotationAllocator`](super::NoteRotationAllocator).
    SingleChannelRotation {
        /// The one channel every note is sent on, 0-indexed.
        channel: u8,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// RP-053's channel layout for every zone shape: which channel is the
    /// master, which span the members occupy, and that the two roles never
    /// overlap.
    ///
    /// One table rather than a test per constructor: the property is the same
    /// sentence in each row, and the rows are what differ. `member_channel_range`
    /// is checked against the same span `is_member_channel` reports, so the
    /// accessor and the predicate cannot drift apart.
    #[test]
    fn zone_layout_places_master_and_members_per_rp053() {
        // (config, master channel, inclusive member span)
        let cases = [
            (MpeZoneConfig::lower(10), 0u8, (1u8, 10u8)),
            (MpeZoneConfig::upper(5), 15, (10, 14)),
            // The maximum zone: 15 members leave exactly the master channel out.
            (MpeZoneConfig::lower(15), 0, (1, 15)),
            (MpeZoneConfig::upper(15), 15, (0, 14)),
            // A single-channel (non-MPE) zone has no member channels at all, so
            // the range degenerates to the master's own channel.
            (MpeZoneConfig::single_channel(7), 7, (7, 7)),
        ];

        for (config, master, (first, last)) in cases {
            let label = format!("{:?}", config.zone);
            assert_eq!(config.master_channel, master, "master of {label}");
            assert!(config.is_master_channel(master), "{label} claims its master");

            let range = config.member_channel_range();
            assert_eq!((*range.start(), *range.end()), (first, last), "span of {label}");

            if config.member_count == 0 {
                // A single-channel zone's master is not also a member — the
                // range collapsing onto it must not make it one.
                assert!(!config.is_member_channel(master), "{label} has no members");
                continue;
            }

            assert!(config.is_member_channel(first), "{label} first member");
            assert!(config.is_member_channel(last), "{label} last member");
            // The master is never a member, and neither is a channel just
            // outside either end of the span.
            assert!(!config.is_member_channel(master), "{label} master is not a member");
            assert!(!config.is_master_channel(first), "{label} first member is not master");
            if let Some(before) = first.checked_sub(1) {
                assert!(!config.is_member_channel(before), "{label} below the span");
            }
            if last < 15 {
                assert!(!config.is_member_channel(last + 1), "{label} above the span");
            }
        }
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

}

#[cfg(all(test, feature = "serde"))]
mod serde_tests {
    use super::*;

    /// **An MPE setup survives being written and read back.**
    ///
    /// The claim the `serde` feature exists for: before it, zone configuration
    /// lived only in a Bevy resource whose own doc said it "is a Bevy resource,
    /// not document state" — so plugging in a controller, configuring the zones
    /// and saving lost the setup.
    #[test]
    fn an_mpe_mode_round_trips_through_a_real_format() {
        for mode in [
            MpeMode::Disabled,
            MpeMode::LowerZone(MpeZoneConfig::lower(10)),
            MpeMode::UpperZone(MpeZoneConfig::upper(7)),
            MpeMode::DualZone {
                lower: MpeZoneConfig::lower(5),
                upper: MpeZoneConfig::upper(5),
            },
            MpeMode::SingleChannelRotation { channel: 3 },
        ] {
            let json = serde_json::to_string(&mode).expect("serializes");
            let back: MpeMode = serde_json::from_str(&json).expect("deserializes");
            assert_eq!(back, mode, "round trip changed the mode: {json}");
        }
    }

    /// **Pitch-bend sensitivity round-trips its exact fixed-point bits.**
    ///
    /// `PitchBendSensitivity` is 7.25 fixed point — the RPN wire form. Persisting
    /// it as `f32` semitones would be lossy for any fractional range, so the
    /// newtype is serialized whole rather than converted. A non-whole-semitone
    /// value is the case that would expose a conversion.
    #[test]
    fn pitch_bend_sensitivity_keeps_its_exact_bits() {
        // 2.5 semitones: representable in 7.25 fixed point, not in whole
        // semitones — so a `to_semitones`/`from_semitones` round trip loses it.
        let fractional = PitchBendSensitivity::from_rpn_bits(
            PitchBendSensitivity::from_semitones(2).to_rpn_bits() + (1 << 24),
        );
        let cfg = MpeZoneConfig {
            member_pitch_bend_range: fractional,
            ..MpeZoneConfig::lower(4)
        };

        let back: MpeZoneConfig =
            serde_json::from_str(&serde_json::to_string(&cfg).expect("serializes"))
                .expect("deserializes");
        assert_eq!(
            back.member_pitch_bend_range.to_rpn_bits(),
            fractional.to_rpn_bits(),
            "the raw 7.25 bits must survive, not a semitone approximation"
        );
        assert_eq!(back, cfg);
    }

    /// The master and member ranges stay distinct.
    ///
    /// RP-053 gives them different defaults (±48 and ±2) and a controller can set
    /// them independently, so a round trip that collapsed them into one value
    /// would bend a per-note gesture 24× too far — audible, but only on hardware.
    #[test]
    fn the_two_bend_ranges_do_not_collapse_into_one() {
        let cfg = MpeZoneConfig::lower(10);
        assert_ne!(
            cfg.master_pitch_bend_range, cfg.member_pitch_bend_range,
            "the fixture must actually differ, or this proves nothing"
        );
        let back: MpeZoneConfig =
            serde_json::from_str(&serde_json::to_string(&cfg).expect("serializes"))
                .expect("deserializes");
        assert_eq!(back.master_pitch_bend_range, cfg.master_pitch_bend_range);
        assert_eq!(back.member_pitch_bend_range, cfg.member_pitch_bend_range);
        assert_ne!(back.master_pitch_bend_range, back.member_pitch_bend_range);
    }
}
