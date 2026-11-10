//! MPE zone layout (RP-053): which of the sixteen MIDI 1.0 channels are master
//! and which are members, and the MPE Configuration Message that declares it.
//!
//! Every channel here is a [`MidiChannel`], **0-indexed**: `0` is the channel a
//! device's front panel calls "Ch1", because that is what UMP carries. The one
//! place the distinction bites is the two zone anchors: the lower zone's master
//! is `0` and the upper zone's is `15`.

use tutti_ump::{Address, Message, MidiChannel, MidiGroup, Rpn, Ump, Voice2, decode};

use crate::PitchBendSensitivity;

/// Which of the two RP-053 zones a configuration describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum MpeZone {
    /// Master on channel 0, members counting up from channel 1.
    Lower,
    /// Master on channel 15, members counting down from channel 14.
    Upper,
}

impl MpeZone {
    /// The zone's master channel: `0` for the lower zone, `15` for the upper
    /// one. Zone-wide messages (bend, pressure) arrive here, and so does the
    /// zone's MPE Configuration Message.
    #[inline]
    #[must_use]
    pub const fn master_channel(self) -> MidiChannel {
        match self {
            Self::Lower => MidiChannel::FIRST,
            Self::Upper => MidiChannel::LAST,
        }
    }
}

/// MPE's two default pitch-bend ranges (RP-053 / M1-100-UM, "Pitch Bend
/// Sensitivity"): the Master channel bends the whole zone by 2 semitones, a
/// Member channel bends its one note by 48. A receiver of an MPE
/// Configuration Message resets both to these.
///
/// They are separate state, not one value seen from two angles: RPN 0 arrives
/// per channel, so a controller can legitimately set them independently.
const MASTER_PITCH_BEND: PitchBendSensitivity = PitchBendSensitivity::of(2);
const MEMBER_PITCH_BEND: PitchBendSensitivity = PitchBendSensitivity::of(48);

/// One zone's layout and bend ranges: the whole description of how an MPE
/// zone occupies the sixteen channels.
///
/// The fields are private. [`new`](Self::new) refuses a member count RP-053
/// does not allow, and a persisted configuration (feature `serde`) is read back
/// through the same check, so a value in hand always names channels that exist.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(
    feature = "serde",
    serde(try_from = "MpeZoneConfigRaw", into = "MpeZoneConfigRaw")
)]
pub struct MpeZoneConfig {
    zone: MpeZone,
    member_count: u8,
    master_pitch_bend_range: PitchBendSensitivity,
    member_pitch_bend_range: PitchBendSensitivity,
}

/// The persisted shape of an [`MpeZoneConfig`]: its fields as written, checked
/// by `TryFrom` on the way back in.
#[cfg(feature = "serde")]
#[derive(serde::Serialize, serde::Deserialize)]
struct MpeZoneConfigRaw {
    zone: MpeZone,
    member_count: u8,
    master_pitch_bend_range: PitchBendSensitivity,
    member_pitch_bend_range: PitchBendSensitivity,
}

#[cfg(feature = "serde")]
impl From<MpeZoneConfig> for MpeZoneConfigRaw {
    fn from(c: MpeZoneConfig) -> Self {
        Self {
            zone: c.zone,
            member_count: c.member_count,
            master_pitch_bend_range: c.master_pitch_bend_range,
            member_pitch_bend_range: c.member_pitch_bend_range,
        }
    }
}

#[cfg(feature = "serde")]
impl TryFrom<MpeZoneConfigRaw> for MpeZoneConfig {
    type Error = &'static str;

    fn try_from(raw: MpeZoneConfigRaw) -> Result<Self, Self::Error> {
        Self::new(raw.zone, raw.member_count)
            .map(|c| {
                c.with_master_pitch_bend_range(raw.master_pitch_bend_range)
                    .with_member_pitch_bend_range(raw.member_pitch_bend_range)
            })
            .ok_or("an MPE zone has 1 to 15 member channels")
    }
}

impl MpeZoneConfig {
    /// The most member channels a zone can claim: the sixteen channels less the
    /// zone's own master (RP-053).
    pub const MAX_MEMBERS: u8 = 15;

    /// A zone of `member_count` members with RP-053's default bend ranges
    /// (master 2 semitones, members 48), or `None` for a count outside
    /// `1..=`[`MAX_MEMBERS`](Self::MAX_MEMBERS).
    ///
    /// `0` is not a zone: in an MPE Configuration Message it means "disable
    /// this zone" ([`from_mcm`](Self::from_mcm) reports it as such), and no
    /// configuration describes a disabled zone.
    #[must_use]
    pub const fn new(zone: MpeZone, member_count: u8) -> Option<Self> {
        if member_count == 0 || member_count > Self::MAX_MEMBERS {
            return None;
        }
        Some(Self {
            zone,
            member_count,
            master_pitch_bend_range: MASTER_PITCH_BEND,
            member_pitch_bend_range: MEMBER_PITCH_BEND,
        })
    }

    /// [`new`](Self::new) for the lower zone: master Ch1 (0), members from Ch2.
    #[must_use]
    pub const fn lower(member_count: u8) -> Option<Self> {
        Self::new(MpeZone::Lower, member_count)
    }

    /// [`new`](Self::new) for the upper zone: master Ch16 (15), members from
    /// Ch15 counting down.
    #[must_use]
    pub const fn upper(member_count: u8) -> Option<Self> {
        Self::new(MpeZone::Upper, member_count)
    }

    /// Which zone this is, and hence which direction members count in.
    #[inline]
    #[must_use]
    pub const fn zone(&self) -> MpeZone {
        self.zone
    }

    /// The zone's master channel ([`MpeZone::master_channel`]).
    #[inline]
    #[must_use]
    pub const fn master_channel(&self) -> MidiChannel {
        self.zone.master_channel()
    }

    /// How many member channels the zone claims, `1..=15`.
    #[inline]
    #[must_use]
    pub const fn member_count(&self) -> u8 {
        self.member_count
    }

    /// Bend range for the zone's **master** channel, which applies to every
    /// note in the zone at once. RP-053 default 2 semitones.
    #[inline]
    #[must_use]
    pub const fn master_pitch_bend_range(&self) -> PitchBendSensitivity {
        self.master_pitch_bend_range
    }

    /// Bend range for a **member** channel, which applies to that channel's
    /// single note. RP-053 default 48 semitones; the master's 2 here would
    /// bend a per-note gesture a 24th as far.
    #[inline]
    #[must_use]
    pub const fn member_pitch_bend_range(&self) -> PitchBendSensitivity {
        self.member_pitch_bend_range
    }

    /// The bend range that applies to `channel`: the master's for the master
    /// channel, the member's for anything else.
    ///
    /// Use this rather than reading a field directly: picking the wrong one is
    /// a silent 24x pitch error.
    #[inline]
    #[must_use]
    pub fn pitch_bend_range_for(&self, channel: MidiChannel) -> PitchBendSensitivity {
        if self.is_master_channel(channel) {
            self.master_pitch_bend_range
        } else {
            self.member_pitch_bend_range
        }
    }

    /// Override the **master** channel's bend range (RPN 0 on the master).
    #[must_use]
    pub const fn with_master_pitch_bend_range(mut self, range: PitchBendSensitivity) -> Self {
        self.master_pitch_bend_range = range;
        self
    }

    /// Override the **member** channels' bend range (RPN 0 on a member).
    #[must_use]
    pub const fn with_member_pitch_bend_range(mut self, range: PitchBendSensitivity) -> Self {
        self.member_pitch_bend_range = range;
        self
    }

    /// Reports whether `channel` is this zone's master.
    #[inline]
    #[must_use]
    pub fn is_master_channel(&self, channel: MidiChannel) -> bool {
        channel == self.master_channel()
    }

    /// Reports whether `channel` is one of this zone's member channels (never
    /// the master).
    #[inline]
    #[must_use]
    pub fn is_member_channel(&self, channel: MidiChannel) -> bool {
        self.members().contains(&channel.get())
    }

    /// Reports whether `channel` belongs to this zone in either role.
    #[inline]
    #[must_use]
    pub fn handles_channel(&self, channel: MidiChannel) -> bool {
        self.is_master_channel(channel) || self.is_member_channel(channel)
    }

    /// The member channels, in ascending order whichever direction the zone
    /// counts: `1..=n` for the lower zone, `15-n..=14` for the upper one.
    pub fn member_channels(&self) -> impl Iterator<Item = MidiChannel> + Clone {
        self.members().filter_map(MidiChannel::new)
    }

    /// The members as raw channel numbers. The two zones' spans are compared
    /// and walked here once.
    const fn members(&self) -> core::ops::RangeInclusive<u8> {
        match self.zone {
            MpeZone::Lower => 1..=self.member_count,
            // `member_count <= 15`, so the subtraction cannot wrap.
            MpeZone::Upper => (15 - self.member_count)..=14,
        }
    }

    /// Whether this zone's members share a channel with `other`'s.
    fn overlaps(&self, other: &Self) -> bool {
        let (a, b) = (self.members(), other.members());
        a.start() <= b.end() && b.start() <= a.end()
    }

    /// Encodes this zone as an **MPE Configuration Message** (MCM): a MIDI 2.0
    /// Registered Controller (RPN) on the zone's *master* channel, address
    /// [`Rpn::MPE_CONFIGURATION`], data = member count. Per RP-053 / M2-104, the
    /// master channel (Ch1 lower / Ch16 upper) is what a receiver reads to know
    /// which zone is being configured.
    ///
    /// The 7-bit member count sits in the top 7 bits of the 32-bit data field
    /// (MSB-aligned, the spec's 7 to 32 convention) so it round-trips exactly.
    #[must_use]
    pub fn to_mcm(&self) -> Ump {
        Message::Midi2(
            Address {
                group: MidiGroup::FIRST,
                channel: self.master_channel(),
            },
            Voice2::Registered {
                bank: Rpn::MPE_CONFIGURATION.bank(),
                index: Rpn::MPE_CONFIGURATION.index(),
                value: u32::from(self.member_count) << 25,
            },
        )
        .into()
    }

    /// Decodes an MCM into the zone it configures and its member count, if
    /// `ump` is an MPE Configuration Message (RPN bank `0x00`, index `0x06`) on
    /// a zone's master channel. `None` for any other message.
    ///
    /// A count of `0` is the spec's "disable this zone" and is returned as `0`,
    /// which [`new`](Self::new) would refuse. A message is rejected, not
    /// clamped, when RP-053 does not allow it: an MCM goes on Ch1 or Ch16 only,
    /// and a zone has at most 15 members. Clamping would silently reconfigure
    /// the zone to something the sender never asked for; `None` lets the caller
    /// keep the zone it had.
    #[must_use]
    pub fn from_mcm(ump: &Ump) -> Option<(MpeZone, u8)> {
        let Ok(Message::Midi2(address, Voice2::Registered { bank, index, value })) = decode(*ump)
        else {
            return None;
        };
        if Rpn::new(bank, index) != Rpn::MPE_CONFIGURATION {
            return None;
        }
        let zone = [MpeZone::Lower, MpeZone::Upper]
            .into_iter()
            .find(|z| z.master_channel() == address.channel)?;
        let member_count = (value >> 25) as u8;
        (member_count <= Self::MAX_MEMBERS).then_some((zone, member_count))
    }
}

/// The zone arrangement of an instrument: one zone or both.
///
/// [`MpeZoneConfig`] describes *a* zone; this is the one or two of them an
/// instrument holds, checked together. [`new`](Self::new) refuses a zone in
/// the wrong role, no zone at all, and two zones whose members share a channel
/// (RP-053 makes the sender keep them apart; here the type does). Sixteen
/// channels less two masters leave 14 members to share, so a pair of zones
/// totalling more than 14 members is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(
    feature = "serde",
    serde(try_from = "MpeZonesRaw", into = "MpeZonesRaw")
)]
pub struct MpeZones {
    lower: Option<MpeZoneConfig>,
    upper: Option<MpeZoneConfig>,
}

/// The persisted shape of [`MpeZones`], checked by `TryFrom`.
#[cfg(feature = "serde")]
#[derive(serde::Serialize, serde::Deserialize)]
struct MpeZonesRaw {
    lower: Option<MpeZoneConfig>,
    upper: Option<MpeZoneConfig>,
}

#[cfg(feature = "serde")]
impl From<MpeZones> for MpeZonesRaw {
    fn from(z: MpeZones) -> Self {
        Self {
            lower: z.lower,
            upper: z.upper,
        }
    }
}

#[cfg(feature = "serde")]
impl TryFrom<MpeZonesRaw> for MpeZones {
    type Error = &'static str;

    fn try_from(raw: MpeZonesRaw) -> Result<Self, Self::Error> {
        Self::new(raw.lower, raw.upper)
            .ok_or("MPE zones: none, a zone in the wrong role, or overlapping members")
    }
}

impl MpeZones {
    /// The arrangement of `lower` and `upper`, or `None` when both are absent,
    /// `lower` is not a lower zone or `upper` not an upper one, or the two
    /// zones' members overlap.
    #[must_use]
    pub fn new(lower: Option<MpeZoneConfig>, upper: Option<MpeZoneConfig>) -> Option<Self> {
        if lower.is_none() && upper.is_none() {
            return None;
        }
        if lower.is_some_and(|c| c.zone() != MpeZone::Lower)
            || upper.is_some_and(|c| c.zone() != MpeZone::Upper)
        {
            return None;
        }
        if let (Some(l), Some(u)) = (&lower, &upper)
            && l.overlaps(u)
        {
            return None;
        }
        Some(Self { lower, upper })
    }

    /// The zone anchored at channel 0, if the instrument has one.
    #[inline]
    #[must_use]
    pub const fn lower(&self) -> Option<&MpeZoneConfig> {
        self.lower.as_ref()
    }

    /// The zone anchored at channel 15, if the instrument has one.
    #[inline]
    #[must_use]
    pub const fn upper(&self) -> Option<&MpeZoneConfig> {
        self.upper.as_ref()
    }

    /// The zones the instrument has, lower first.
    pub fn zones(&self) -> impl Iterator<Item = &MpeZoneConfig> {
        self.lower.iter().chain(self.upper.iter())
    }

    /// The zone that owns `channel` (as its master or one of its members), if
    /// any. The two zones share no channel, so there is at most one.
    #[must_use]
    pub fn zone_of(&self, channel: MidiChannel) -> Option<&MpeZoneConfig> {
        self.zones().find(|z| z.handles_channel(channel))
    }
}

/// How an instrument occupies the sixteen channels as a whole.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum MpeMode {
    /// No MPE. Channels behave as ordinary MIDI 1.0 channels.
    #[default]
    Disabled,
    /// One or two MPE zones.
    Zones(MpeZones),
    /// Single-channel **Note Number Rotation** (M2-104 Appendix C.3): all notes
    /// on the one channel, with no zones and no member-channel spreading.
    /// Rotation is a *sender* scheme and a receiver need not know it is used,
    /// so a receiver keeps the one channel's messages and ignores the rest.
    SingleChannelRotation {
        /// The one channel every note is sent on.
        channel: MidiChannel,
    },
}

#[cfg(test)]
mod tests;
