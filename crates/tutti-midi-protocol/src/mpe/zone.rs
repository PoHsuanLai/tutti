//! MPE zone layout: which of the sixteen MIDI 1.0 channels are master and which
//! are members, under MPE 1.1 (RP-053 / M1-100, which calls the master channel
//! the Master) or the MIDI-CI MPE Profile (M2-120, which calls it the Manager),
//! and the MPE Configuration Message that declares an MPE 1.1 zone.
//!
//! Every channel here is a [`MidiChannel`], **0-indexed**: `0` is the channel a
//! device's front panel calls "Ch1", because that is what UMP carries. The one
//! place the distinction bites is the two MPE 1.1 zone anchors: the lower
//! zone's master is `0` and the upper zone's is `15`.

use tutti_ump::{Address, ControlValue, Message, MidiChannel, MidiGroup, Rpn, Ump, Voice2};

use crate::{ChannelResponse, PitchBendSensitivity};

/// Which MPE specification a zone follows. Its receive rules are this type's,
/// so a zone cannot carry one specification's layout and the other's rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum MpeSpec {
    /// MPE 1.1 (M1-100): zones anchored at channel 1 and channel 16, set up
    /// by the MPE Configuration Message.
    Mpe11,
    /// The MIDI-CI MPE Profile (M2-120): a zone per enabled Profile.
    Profile,
}

/// Which of the two RP-053 zones an MPE 1.1 configuration describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
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

/// Where a zone sits, and the bend ranges it keeps: one of MPE 1.1's two
/// anchors with a range for the master and one for the members, or a Manager
/// Channel the MPE Profile enabled anywhere with the one range of all its
/// channels (M2-120 section 3.5). It fixes the zone's specification and which
/// way its members run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
enum Placement {
    /// MPE 1.1: master on channel 0 or 15, members counting away from it.
    Side {
        side: MpeZone,
        master: PitchBendSensitivity,
        member: PitchBendSensitivity,
    },
    /// The Profile: the Manager on this channel, members counting up from it.
    Managed {
        manager: MidiChannel,
        range: PitchBendSensitivity,
    },
}

/// One zone's layout and bend ranges: the whole description of how an MPE
/// zone occupies the sixteen channels, under either specification.
///
/// Build an MPE 1.1 zone with [`new`](Self::new), [`lower`](Self::lower) or
/// [`upper`](Self::upper) and a Profile zone with [`profile`](Self::profile);
/// the constructor is what records the [specification](Self::spec). The
/// fields are private. A constructor refuses a layout the specification does
/// not allow, and a persisted configuration (feature `serde`) is read back
/// through the same check, so a value in hand always names channels that
/// exist.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(
    feature = "serde",
    serde(try_from = "MpeZoneConfigRaw", into = "MpeZoneConfigRaw")
)]
pub struct MpeZoneConfig {
    placement: Placement,
    member_count: u8,
    response: ChannelResponse,
}

/// The persisted shape of an [`MpeZoneConfig`]: its fields as written, checked
/// by `TryFrom` on the way back in.
#[cfg(feature = "serde")]
#[derive(serde::Serialize, serde::Deserialize)]
struct MpeZoneConfigRaw {
    placement: Placement,
    member_count: u8,
    response: ChannelResponse,
}

#[cfg(feature = "serde")]
impl From<MpeZoneConfig> for MpeZoneConfigRaw {
    fn from(c: MpeZoneConfig) -> Self {
        Self {
            placement: c.placement,
            member_count: c.member_count,
            response: c.response,
        }
    }
}

#[cfg(feature = "serde")]
impl TryFrom<MpeZoneConfigRaw> for MpeZoneConfig {
    type Error = &'static str;

    fn try_from(raw: MpeZoneConfigRaw) -> Result<Self, Self::Error> {
        Self::placed(raw.placement, raw.member_count)
            .map(|c| c.with_channel_response(raw.response))
            .ok_or("an MPE zone has a master, 1 or more members, and ends by channel 16")
    }
}

impl MpeZoneConfig {
    /// The most member channels an MPE 1.1 zone can claim: the sixteen
    /// channels less the zone's own master (RP-053). A Profile zone has at
    /// most the channels above its Manager.
    pub const MAX_MEMBERS: u8 = 15;

    /// An MPE 1.1 zone of `member_count` members, its master channel's bend
    /// range `master` and its members' `member`, with a polyphonic response, or
    /// `None` for a count outside `1..=`[`MAX_MEMBERS`](Self::MAX_MEMBERS).
    /// A receiver of an MPE Configuration Message resets the ranges to
    /// [`PitchBendSensitivity::MPE_MASTER_DEFAULT`] and
    /// [`PitchBendSensitivity::MPE_MEMBER_DEFAULT`].
    ///
    /// `0` is not a zone: in an MPE Configuration Message it means disable
    /// this zone ([`from_mcm`](Self::from_mcm) reports it as such), and no
    /// configuration describes a disabled zone.
    #[must_use]
    pub const fn new(
        zone: MpeZone,
        member_count: u8,
        master: PitchBendSensitivity,
        member: PitchBendSensitivity,
    ) -> Option<Self> {
        Self::placed(
            Placement::Side {
                side: zone,
                master,
                member,
            },
            member_count,
        )
    }

    /// A zone at `placement` with `member_count` members, or `None` when the
    /// layout is not one the specification allows: no member, or members past
    /// channel 16.
    const fn placed(placement: Placement, member_count: u8) -> Option<Self> {
        let first_free = match placement {
            Placement::Side { .. } => 0,
            Placement::Managed { manager, .. } => manager.get(),
        };
        let Some(end) = first_free.checked_add(member_count) else {
            return None;
        };
        if member_count == 0 || end > Self::MAX_MEMBERS {
            return None;
        }
        Some(Self {
            placement,
            member_count,
            response: ChannelResponse::Polyphonic,
        })
    }

    /// [`new`](Self::new) for the lower zone: master Ch1 (0), members from Ch2.
    #[must_use]
    pub const fn lower(
        member_count: u8,
        master: PitchBendSensitivity,
        member: PitchBendSensitivity,
    ) -> Option<Self> {
        Self::new(MpeZone::Lower, member_count, master, member)
    }

    /// [`new`](Self::new) for the upper zone: master Ch16 (15), members from
    /// Ch15 counting down.
    #[must_use]
    pub const fn upper(
        member_count: u8,
        master: PitchBendSensitivity,
        member: PitchBendSensitivity,
    ) -> Option<Self> {
        Self::new(MpeZone::Upper, member_count, master, member)
    }

    /// A MIDI-CI MPE Profile zone (M2-120 sections 2.1 and 2.2) of `channels`
    /// channels, the Manager Channel `manager` and the Members above it (the
    /// Number of Channels of Set Profile On, appendix A.3), with `range` as the
    /// one bend range of the Manager and every Member (section 3.5; the
    /// Profile starts at [`PitchBendSensitivity::MPE_PROFILE_DEFAULT`]), and a
    /// polyphonic response; or `None` for fewer than 2 channels or a layout
    /// that runs past channel 16. Any channel can be a Manager.
    #[must_use]
    pub const fn profile(
        manager: MidiChannel,
        channels: u8,
        range: PitchBendSensitivity,
    ) -> Option<Self> {
        match channels.checked_sub(1) {
            Some(member_count) => Self::placed(Placement::Managed { manager, range }, member_count),
            None => None,
        }
    }

    /// The specification the zone follows, which its constructor fixed: its
    /// layout and its receive rules go together.
    #[inline]
    #[must_use]
    pub const fn spec(&self) -> MpeSpec {
        match self.placement {
            Placement::Side { .. } => MpeSpec::Mpe11,
            Placement::Managed { .. } => MpeSpec::Profile,
        }
    }

    /// Which MPE 1.1 zone this is, and hence which direction members count in;
    /// `None` for a Profile zone, whose members always count up from its
    /// Manager.
    #[inline]
    #[must_use]
    pub const fn zone(&self) -> Option<MpeZone> {
        match self.placement {
            Placement::Side { side, .. } => Some(side),
            Placement::Managed { .. } => None,
        }
    }

    /// The zone's master channel: the MPE 1.1 Master ([`MpeZone::master_channel`])
    /// or the Profile's Manager Channel, the zone's lowest.
    #[inline]
    #[must_use]
    pub const fn master_channel(&self) -> MidiChannel {
        match self.placement {
            Placement::Side { side, .. } => side.master_channel(),
            Placement::Managed { manager, .. } => manager,
        }
    }

    /// How many member channels the zone claims, at least 1.
    #[inline]
    #[must_use]
    pub const fn member_count(&self) -> u8 {
        self.member_count
    }

    /// How many channels the zone has, the master and its members.
    #[inline]
    #[must_use]
    pub const fn channel_count(&self) -> u8 {
        self.member_count + 1
    }

    /// How the member channels take notes: MPE 1.1's MIDI Mode 3 (polyphonic)
    /// or Mode 4 (M1-100 section 2.2.4.3), or the Profile's Channel Response
    /// (M2-120 section 3.6). For an MPE 1.1 zone it is the mode the zone
    /// starts in; a Mono Mode On or Poly Mode On from the wire changes it.
    #[inline]
    #[must_use]
    pub const fn channel_response(&self) -> ChannelResponse {
        self.response
    }

    /// The bend range that applies to `channel`. An MPE 1.1 zone answers per
    /// role: the master's range applies to every note in the zone at once
    /// (RP-053 default 2 semitones), a member's to that channel's single note
    /// (default 48; the master's 2 would bend a per-note gesture a 24th as
    /// far). A Profile zone answers its one shared range for every channel
    /// (M2-120 section 3.5).
    ///
    /// Use this rather than assuming a role's range: picking the wrong one is
    /// a silent 24x pitch error.
    #[inline]
    #[must_use]
    pub fn pitch_bend_range_for(&self, channel: MidiChannel) -> PitchBendSensitivity {
        match self.placement {
            Placement::Side { master, member, .. } => {
                if self.is_master_channel(channel) {
                    master
                } else {
                    member
                }
            }
            Placement::Managed { range, .. } => range,
        }
    }

    /// The zone with `response` as how its member channels take notes
    /// ([`channel_response`](Self::channel_response)).
    #[must_use]
    pub const fn with_channel_response(mut self, response: ChannelResponse) -> Self {
        self.response = response;
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

    /// The members as raw channel numbers. The zones' spans are compared and
    /// walked here once.
    const fn members(&self) -> core::ops::RangeInclusive<u8> {
        match self.placement {
            Placement::Side {
                side: MpeZone::Lower,
                ..
            } => 1..=self.member_count,
            // `member_count <= 15`, so the subtraction cannot wrap.
            Placement::Side {
                side: MpeZone::Upper,
                ..
            } => (15 - self.member_count)..=14,
            // `placed` keeps `manager + member_count <= 15`.
            Placement::Managed { manager, .. } => {
                (manager.get() + 1)..=(manager.get() + self.member_count)
            }
        }
    }

    /// The lowest and highest channel the zone occupies, master included.
    const fn span(&self) -> (u8, u8) {
        let (first, last) = (*self.members().start(), *self.members().end());
        let master = self.master_channel().get();
        (
            if master < first { master } else { first },
            if master > last { master } else { last },
        )
    }

    /// Whether this zone shares a channel with `other`.
    fn overlaps(&self, other: &Self) -> bool {
        let ((a_first, a_last), (b_first, b_last)) = (self.span(), other.span());
        a_first <= b_last && b_first <= a_last
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
                value: ControlValue::new(u32::from(self.member_count) << 25),
            },
        )
        .into()
    }

    /// Decodes an MCM into the zone it configures and its member count, if
    /// `ump` is an MPE Configuration Message (RPN bank `0x00`, index `0x06`) on
    /// a zone's master channel. `None` for any other message.
    ///
    /// A count of `0` is the spec's disable-this-zone value and is returned as `0`,
    /// which [`new`](Self::new) would refuse. A message is rejected, not
    /// clamped, when RP-053 does not allow it: an MCM goes on Ch1 or Ch16 only,
    /// and a zone has at most 15 members. Clamping would silently reconfigure
    /// the zone to something the sender never asked for; `None` lets the caller
    /// keep the zone it had.
    #[must_use]
    pub fn from_mcm(ump: &Ump) -> Option<(MpeZone, u8)> {
        let Ok(Message::Midi2(address, Voice2::Registered { bank, index, value })) =
            (*ump).decode()
        else {
            return None;
        };
        if Rpn::new(bank, index) != Rpn::MPE_CONFIGURATION {
            return None;
        }
        let zone = [MpeZone::Lower, MpeZone::Upper]
            .into_iter()
            .find(|z| z.master_channel() == address.channel)?;
        let member_count = (value.get() >> 25) as u8;
        (member_count <= Self::MAX_MEMBERS).then_some((zone, member_count))
    }
}

/// The zones of an instrument or of one Group: all of one specification, none
/// sharing a channel.
///
/// [`MpeZoneConfig`] describes *a* zone; this is the one or more an instrument
/// holds, checked together. MPE 1.1 has a lower and an upper zone, so at most
/// two (RP-053 makes the sender keep them apart; here the type does), and
/// sixteen channels less two masters leave 14 members to share. The Profile
/// has up to [`MAX_ZONES`](Self::MAX_ZONES), a Manager with at least one
/// Member each (M2-120 section 3.4). The two limits follow from the layouts: a
/// third MPE 1.1 zone would be a second lower or upper zone, and a ninth Profile
/// zone cannot fit in the sixteen channels.
///
/// [`new`](Self::new) and [`with`](Self::with) refuse a zone of the other
/// specification, a zone that shares a channel with one held, and, for
/// `new`, a zone in the wrong role. The zones are kept in order of their
/// master, so equal sets compare equal whatever order they were added in.
/// There is no empty set: no zones is no `MpeZones` ([`without`](Self::without)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(
    feature = "serde",
    serde(try_from = "Vec<MpeZoneConfig>", into = "Vec<MpeZoneConfig>")
)]
pub struct MpeZones {
    zones: [Option<MpeZoneConfig>; Self::MAX_ZONES],
}

#[cfg(feature = "serde")]
impl From<MpeZones> for Vec<MpeZoneConfig> {
    fn from(zones: MpeZones) -> Self {
        zones.zones().copied().collect()
    }
}

#[cfg(feature = "serde")]
impl TryFrom<Vec<MpeZoneConfig>> for MpeZones {
    type Error = &'static str;

    fn try_from(zones: Vec<MpeZoneConfig>) -> Result<Self, Self::Error> {
        let mut zones = zones.into_iter();
        let first = zones.next().ok_or("no MPE zone")?;
        zones
            .try_fold(Self::single(first), |set, zone| set.with(zone))
            .ok_or("MPE zones overlap or follow different specifications")
    }
}

impl MpeZones {
    /// The most zones any specification has: the Profile's eight, every zone
    /// with two channels at least.
    pub const MAX_ZONES: usize = 8;

    /// The set of `zone` alone.
    #[must_use]
    pub const fn single(zone: MpeZoneConfig) -> Self {
        let mut zones = [None; Self::MAX_ZONES];
        zones[0] = Some(zone);
        Self { zones }
    }

    /// The MPE 1.1 arrangement of `lower` and `upper`, or `None` when both are
    /// absent, `lower` is not a lower zone or `upper` not an upper one, or the
    /// two zones' members overlap.
    #[must_use]
    pub fn new(lower: Option<MpeZoneConfig>, upper: Option<MpeZoneConfig>) -> Option<Self> {
        if lower.is_some_and(|c| c.zone() != Some(MpeZone::Lower))
            || upper.is_some_and(|c| c.zone() != Some(MpeZone::Upper))
        {
            return None;
        }
        let mut zones = lower.into_iter().chain(upper);
        let first = zones.next()?;
        zones.try_fold(Self::single(first), |set, zone| set.with(zone))
    }

    /// The set with `zone` added, or `None` when it follows another
    /// specification than the zones held or shares a channel with one of them.
    #[must_use]
    pub fn with(self, zone: MpeZoneConfig) -> Option<Self> {
        if self
            .zones()
            .any(|held| held.spec() != zone.spec() || held.overlaps(&zone))
        {
            return None;
        }
        Some(Self::sorted(self.zones().copied().chain([zone])))
    }

    /// The set without the zone whose master is `master`, or `None` when that
    /// was the last. A `master` that names no zone leaves the set as it is.
    #[must_use]
    pub fn without(self, master: MidiChannel) -> Option<Self> {
        let mut rest = self
            .zones()
            .copied()
            .filter(|zone| zone.master_channel() != master)
            .peekable();
        rest.peek()?;
        Some(Self::sorted(rest))
    }

    /// Builds the set of zones already known to agree and not to overlap,
    /// ordered by master.
    fn sorted(zones: impl Iterator<Item = MpeZoneConfig>) -> Self {
        let mut held = [None; Self::MAX_ZONES];
        let mut count = 0;
        for zone in zones {
            held[count] = Some(zone);
            count += 1;
        }
        held[..count].sort_by_key(|zone| zone.map(|z| z.master_channel().get()));
        Self { zones: held }
    }

    /// The MPE 1.1 zone anchored at channel 0, if the set has one.
    #[must_use]
    pub fn lower(&self) -> Option<&MpeZoneConfig> {
        self.zones()
            .find(|zone| zone.zone() == Some(MpeZone::Lower))
    }

    /// The MPE 1.1 zone anchored at channel 15, if the set has one.
    #[must_use]
    pub fn upper(&self) -> Option<&MpeZoneConfig> {
        self.zones()
            .find(|zone| zone.zone() == Some(MpeZone::Upper))
    }

    /// The zones, lowest master first.
    pub fn zones(&self) -> impl Iterator<Item = &MpeZoneConfig> + Clone {
        self.zones.iter().flatten()
    }

    /// The zone that owns `channel` (as its master or one of its members), if
    /// any. The zones share no channel, so there is at most one.
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
