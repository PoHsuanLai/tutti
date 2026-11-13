//! The Profiles of a MIDI-CI peer, as the session holds and drives them: what
//! the peer supports ([`PeerProfiles`]), the handles an app asks to switch one
//! from ([`ListedProfile`]), the request itself ([`ProfileSwitch`]) and what
//! happened ([`ProfilesChange`]).
//!
//! The session is the Initiator (M2-101 §7): it asks every peer that declared
//! Profile Configuration for its Profiles when it is found, and keeps the answer
//! up to date from the peer's own reports. It never lists a Profile the peer
//! did not, and a request is made only from a handle the peer's own list gave,
//! so an app cannot name a Profile or an address the peer lacks. The handle
//! is a snapshot: the peer may remove the Profile before the request is sent,
//! and that is refused ([`ProfileError::NotListed`]).
//!
//! What the spec's request fields allow is in the handle's type. The Number
//! Channels Requested of a Set Profile On (M2-101 §7.8) is a count of Channels
//! only where the Profile is on one Channel: a [`ChannelProfile`] takes a
//! [`ChannelCount`], and a [`WholeProfile`] (on the Group or Function Block)
//! has no count to get wrong, since there the field is the width of what is
//! addressed. A count of 0 on a Channel is unwritable. The limit a peer
//! declares in a Profile Details reply (M2-102 §2.5.1) is not known to this
//! module: it never sends that inquiry, so a count above it is the app's to
//! avoid, and the peer's Disabled Report is how a refusal arrives.

use std::sync::Arc;

use tutti_midi_device::{
    CiDeviceId, Muid, Outcome, ProfileChange, ProfileChannels, ProfileEntry, ProfileId,
    ProfileState, ProfileTable, ProfileTarget, Request, RequestError,
};
use tutti_ump::{MidiChannel, MidiGroup};

/// The Channels a Profile is asked to use when it is enabled on a Channel: the
/// Manager and its Members, at least 1 and at most 14 bits (M2-101 §7.8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChannelCount(ProfileChannels);

impl ChannelCount {
    /// One Channel: a Profile on a single Channel.
    pub const ONE: Self = Self(ProfileChannels::of(1));

    /// A count, or `None` for 0 or more than [`ProfileChannels::MAX`].
    #[must_use]
    pub const fn new(count: u16) -> Option<Self> {
        match ProfileChannels::new(count) {
            Some(channels) if count != 0 => Some(Self(channels)),
            _ => None,
        }
    }

    /// The count.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0.get()
    }
}

/// What a peer's Profiles are at one address, as of when it was read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Listed {
    group: MidiGroup,
    muid: Muid,
    profile: ProfileId,
    entry: ProfileEntry,
}

/// A Profile the peer lists on one Channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChannelProfile {
    listed: Listed,
    channel: MidiChannel,
}

/// Where a [`WholeProfile`] is: the whole Group or the whole Function Block.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Whole {
    /// The Group, not channelized (`0x7E`).
    Group,
    /// The Function Block, not channelized (`0x7F`).
    FunctionBlock,
}

/// A Profile the peer lists on the whole Group or Function Block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WholeProfile {
    listed: Listed,
    whole: Whole,
}

/// A Profile a peer lists, at the address it lists it: the handle a
/// [`ProfileSwitch`] is made from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListedProfile {
    /// On one Channel: enabling it says how many Channels it is to use.
    Channel(ChannelProfile),
    /// On the Group or Function Block: enabling it has no count.
    Whole(WholeProfile),
}

/// What a switch does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Switch {
    On(ProfileTarget),
    Off(CiDeviceId),
}

/// A request that a peer enable or disable one Profile it lists
/// ([`MidiSession::set_profile`](crate::MidiSession::set_profile)). Made only
/// from a [`ListedProfile`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProfileSwitch {
    pub(crate) group: MidiGroup,
    pub(crate) muid: Muid,
    pub(crate) profile: ProfileId,
    pub(crate) switch: Switch,
}

impl Listed {
    const fn on(self, target: ProfileTarget) -> ProfileSwitch {
        ProfileSwitch {
            group: self.group,
            muid: self.muid,
            profile: self.profile,
            switch: Switch::On(target),
        }
    }

    const fn off(self, address: CiDeviceId) -> ProfileSwitch {
        ProfileSwitch {
            group: self.group,
            muid: self.muid,
            profile: self.profile,
            switch: Switch::Off(address),
        }
    }
}

impl ChannelProfile {
    /// Asks the peer to enable the Profile on this Channel, as the Manager of
    /// `channels` Channels in all.
    #[must_use]
    pub const fn enable(&self, channels: ChannelCount) -> ProfileSwitch {
        self.listed.on(ProfileTarget::Channel {
            channel: self.channel,
            channels: channels.0,
        })
    }

    /// Asks the peer to disable the Profile on this Channel.
    #[must_use]
    pub const fn disable(&self) -> ProfileSwitch {
        self.listed.off(CiDeviceId::Channel(self.channel))
    }

    /// The Channel.
    #[must_use]
    pub const fn channel(&self) -> MidiChannel {
        self.channel
    }
}

impl WholeProfile {
    /// Asks the peer to enable the Profile on the whole Group or Function Block.
    #[must_use]
    pub const fn enable(&self) -> ProfileSwitch {
        self.listed.on(match self.whole {
            Whole::Group => ProfileTarget::Group,
            Whole::FunctionBlock => ProfileTarget::FunctionBlock,
        })
    }

    /// Asks the peer to disable the Profile on the whole Group or Function Block.
    #[must_use]
    pub const fn disable(&self) -> ProfileSwitch {
        self.listed.off(match self.whole {
            Whole::Group => CiDeviceId::Group,
            Whole::FunctionBlock => CiDeviceId::FunctionBlock,
        })
    }

    /// Whether it is the Group's or the Function Block's.
    #[must_use]
    pub const fn whole(&self) -> Whole {
        self.whole
    }
}

impl ListedProfile {
    const fn listed(&self) -> &Listed {
        match self {
            Self::Channel(p) => &p.listed,
            Self::Whole(p) => &p.listed,
        }
    }

    /// The Profile.
    #[must_use]
    pub const fn profile(&self) -> ProfileId {
        self.listed().profile
    }

    /// Whether the peer had it enabled when this was read.
    #[must_use]
    pub const fn state(&self) -> ProfileState {
        self.listed().entry.state()
    }

    /// The Channel count of the peer's last Enabled or Disabled Report of it, if
    /// a Version 2 peer sent one.
    #[must_use]
    pub const fn channels(&self) -> Option<ProfileChannels> {
        self.listed().entry.channels()
    }

    /// Asks the peer to disable the Profile at its address.
    #[must_use]
    pub const fn disable(&self) -> ProfileSwitch {
        match self {
            Self::Channel(p) => p.disable(),
            Self::Whole(p) => p.disable(),
        }
    }
}

/// The Profiles one found peer supports (M2-101 §7), as the session last read
/// them. Shared by pointer, so a held one stays as it was.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerProfiles {
    pub(crate) group: MidiGroup,
    pub(crate) muid: Muid,
    pub(crate) known: bool,
    pub(crate) table: ProfileTable,
}

impl PeerProfiles {
    /// The Group of the connection the peer answered on.
    #[must_use]
    pub const fn group(&self) -> MidiGroup {
        self.group
    }

    /// The peer's MUID.
    #[must_use]
    pub const fn muid(&self) -> Muid {
        self.muid
    }

    /// Whether the peer's Profile Inquiry ended with its final Reply, so the
    /// table is its whole declaration; `false` while the inquiry is out or
    /// after it timed out or was refused.
    #[must_use]
    pub const fn is_known(&self) -> bool {
        self.known
    }

    /// The table: per address, each Profile and its state.
    #[must_use]
    pub const fn table(&self) -> &ProfileTable {
        &self.table
    }

    /// Every Profile the peer lists, as handles to switch them from.
    pub fn listed(&self) -> impl Iterator<Item = ListedProfile> + '_ {
        self.table.addresses().flat_map(move |address| {
            self.table.profiles(address).map(move |(profile, entry)| {
                let listed = Listed {
                    group: self.group,
                    muid: self.muid,
                    profile,
                    entry: *entry,
                };
                match address {
                    CiDeviceId::Channel(channel) => {
                        ListedProfile::Channel(ChannelProfile { listed, channel })
                    }
                    CiDeviceId::Group => ListedProfile::Whole(WholeProfile {
                        listed,
                        whole: Whole::Group,
                    }),
                    CiDeviceId::FunctionBlock => ListedProfile::Whole(WholeProfile {
                        listed,
                        whole: Whole::FunctionBlock,
                    }),
                }
            })
        })
    }
}

/// A snapshot of every peer's Profiles on one input.
pub(crate) type Snapshot = Arc<[PeerProfiles]>;

/// A change of a peer's Profiles, once each from
/// [`MidiSession::poll`](crate::MidiSession::poll). Every variant names the
/// peer by the Group it was found on and its MUID.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProfilesChange {
    /// The peer's final Reply to Profile Inquiry arrived: its table is whole.
    Known {
        /// The Group.
        group: MidiGroup,
        /// The peer.
        muid: Muid,
    },
    /// A Report (Added, Removed, Enabled or Disabled) changed one Profile.
    Changed {
        /// The Group.
        group: MidiGroup,
        /// The peer.
        muid: Muid,
        /// Where the Profile is.
        address: CiDeviceId,
        /// Which.
        profile: ProfileId,
        /// What happened to it.
        change: ProfileChange,
    },
    /// The peer did not finish answering Profile Inquiry in time: the table
    /// keeps what it said.
    InquiryTimedOut {
        /// The Group.
        group: MidiGroup,
        /// The peer.
        muid: Muid,
    },
    /// The peer NAKed the Profile Inquiry: it has no Profile Configuration to
    /// ask about.
    InquiryRefused {
        /// The Group.
        group: MidiGroup,
        /// The peer.
        muid: Muid,
    },
    /// A [`ProfileSwitch`] ended: the peer reported the Profile's state, NAKed
    /// it, or did not answer in time.
    Resolved {
        /// The Group.
        group: MidiGroup,
        /// The peer.
        muid: Muid,
        /// What was asked.
        request: Request,
        /// How it ended.
        outcome: Outcome,
    },
    /// A [`ProfileSwitch`] was not sent: one for that Profile at that address
    /// was already waiting.
    NotSent {
        /// The Group.
        group: MidiGroup,
        /// The peer.
        muid: Muid,
        /// The Profile.
        profile: ProfileId,
        /// Why.
        error: RequestError,
    },
    /// The peer is gone (lost, or MIDI-CI ended): its Profiles and the
    /// Initiator that kept them went with it.
    Dropped {
        /// The Group.
        group: MidiGroup,
        /// The peer.
        muid: Muid,
    },
}

/// A [`ProfileSwitch`] that cannot be made.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ProfileError {
    /// The device is not an open input.
    #[error("the device is not an open input")]
    NotOpen,
    /// The peer is not one found on that input with Profile Configuration
    /// (it was lost, or never declared the category).
    #[error("no such MIDI-CI peer with Profile Configuration on that input")]
    NoSuchPeer,
    /// The peer does not list that Profile at that address now (the handle is
    /// older than a Removed Report).
    #[error("the peer does not list that Profile at that address")]
    NotListed,
}
