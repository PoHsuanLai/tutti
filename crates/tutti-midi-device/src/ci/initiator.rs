//! [`ProfileInitiator`]: the Initiator's side of Profile Configuration with one
//! discovered peer (M2-101 §7, M2-102 §2).
//!
//! A pure state machine in the shape of [`CiDevice`](super::CiDevice): every step
//! returns the [`ProfileAction`]s to carry out, time is an [`Instant`] argument,
//! and nothing here reads a clock or sends a byte. The caller makes one per peer
//! the Management core found ([`CiAction::Found`](super::CiAction::Found)), routes
//! to it every decoded message of that Group, sends each
//! [`ProfileAction::Send`] on that Group, and drops it when the peer is
//! [`Lost`](super::CiAction::Lost) or the device's MUID changes. CI messages carry
//! no Group, so the Group is the caller's.
//!
//! # The rules
//!
//! - The inquiry (M2-102 §2.4, "0x21 Reply to Profile Inquiry"): [`new`](ProfileInitiator::new)
//!   sends a Profile Inquiry to the Function Block (`0x7F`), which asks for every Profile
//!   "regardless of MIDI Channel structure". The peer answers with one Reply per address that
//!   has Profiles, in the order Channels, then Groups (`0x7E`), and then a final Reply on
//!   `0x7F`, "with no Profiles reported to indicate that no further replies will follow"
//!   when it has none. That last Reply is the rule for the set being complete
//!   ([`ProfileAction::ProfilesKnown`]); an address that replied is replaced by its Reply, and
//!   an address that did not is dropped from the table then. Neither spec gives a time limit,
//!   so a peer that never sends the final Reply is given up on after [`PROFILE_TIMEOUT`]
//!   ([`ProfileAction::InquiryTimedOut`]): the table keeps what it heard, and nothing is
//!   pruned, since the set is not known to be complete. A NAK of the Inquiry
//!   ([`ProfileAction::InquiryRefused`]) ends it too: the peer has no Profile Configuration.
//! - The table ([`ProfileTable`]): per address (a Channel, the Group, the Function Block),
//!   the Profiles the peer supports, each enabled or disabled, and the number of Channels it
//!   reported while enabled. The reports update it at any time after, from the peer only:
//!   Added (§7.4, disabled), Removed (§7.5), Enabled (§7.10) and Disabled (§7.11), each a
//!   [`ProfileAction::Changed`] when it changed something. An Enabled or Disabled Report for a
//!   Profile the table lacks adds it: the peer supports it, since it says so.
//! - Set On and Set Off ([`set_on`](ProfileInitiator::set_on),
//!   [`set_off`](ProfileInitiator::set_off)): "This is an acknowledgement upon receipt of a Set
//!   Profile On message" (§7.10), so the request resolves on the Enabled or Disabled Report of
//!   that Profile at that address, whichever state it reports (M2-102 §2.6 and §2.7: a peer that
//!   cannot comply reports the state it kept), on a NAK of that message for that Profile
//!   (Status `0x04` "Profile not supported on the requested Channel, Group, or Function Block"
//!   or any other; the NAK's five detail bytes are the Profile Id, §5.11.3), or on
//!   [`PROFILE_TIMEOUT`]. A Report of another Profile (M2-102 §2.8: a Profile Disabled
//!   Report for the Profile it replaced) is a change and resolves nothing.
//! - Details ([`details`](ProfileInitiator::details)): the Reply that echoes the Profile
//!   and Inquiry Target at that address resolves it; so does a NAK or the timeout.
//! - Profile Specific Data from the peer, to this device or to the Broadcast MUID, is
//!   handed out as it is ([`ProfileAction::SpecificData`]).
//!
//! - Timeout Wait (M2-101 §5.10, Table 14): an ACK with Status Code `0x10` from the peer, to
//!   this device, says "wait" for Status Data times 100 ms (0 to 12.7 s). It names its transaction
//!   by the Original Transaction Sub-ID#2 and the address it is on (§5.10.1, "along with other
//!   fields"), and for a Set or a Details request by the Profile Id in its five detail bytes
//!   (§5.10.3; zeroes from a peer with none to give, as for a NAK). A Profile Inquiry names no
//!   Profile, so its details are not read; it is matched on the Function Block address it was sent
//!   to. The deadline becomes `now + wait` if that is later: a wait asks for more time, never
//!   less, so a wait of 0 leaves the deadline where it is, and each new wait counts from when it
//!   arrives. An ACK for nothing in flight, and an ACK with any other status, resolve and extend
//!   nothing: a success ACK (`0x00`) does not end a Set, because the answer to a Set is the
//!   Enabled or Disabled Report ("This is an acknowledgement upon receipt of a Set Profile On
//!   message", §7.10; M2-102 §2.6, §2.7), and §5.10 reserves the ACK for "new mechanisms" that other
//!   specifications describe, none of which M2-102 does for Profiles.
//!
//! A message from another source, or a Reply, Details Reply, ACK or NAK not addressed to this device,
//! is not this peer's and changes nothing.
//!
//! # The timeout
//!
//! [`PROFILE_TIMEOUT`] is tutti's choice, 3 seconds: M2-101 gives none for Profile
//! Configuration, and the only wait it states is Discovery's "at least 3 seconds" (§5.5.5).

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};
use std::vec::Vec;

use tutti_ump::{MidiChannel, U7};

use super::{
    Ack, CI_VERSION, CiDeviceId, CiHeader, CiMessage, Decoded, Destination, DetailsData,
    InquiryTarget, Muid, Nak, ProfileChannels, ProfileId, ProfileLists, ProfileState,
    SUB_ID2_PROFILE_DETAILS_INQUIRY, SUB_ID2_PROFILE_INQUIRY, SUB_ID2_SET_PROFILE_OFF,
    SUB_ID2_SET_PROFILE_ON, SpecificData,
};

/// How long a Profile Inquiry, Set Profile On or Off, or Details Inquiry waits for
/// its answer: 3 seconds. See the module docs: tutti's figure, from §5.5.5.
pub const PROFILE_TIMEOUT: Duration = Duration::from_secs(3);

/// The unit of a Timeout Wait's Status Data (§5.10, Table 14).
const WAIT_STEP: Duration = Duration::from_millis(100);

/// Whether the five detail bytes of an ACK or a NAK name `profile` (§5.10.3, §5.11.3), or
/// are zeroes, which a peer with nothing to name sends.
fn details_name(details: [u8; 5], profile: ProfileId) -> bool {
    details == profile.to_bytes() || details == [0; 5]
}

/// Where a Set Profile On is aimed, and how many Channels it asks for (§7.8).
///
/// "When the Profile Destination field is set to address 0x7E or 0x7F, the number of
/// Channels is determined by the width of the Group or Function Block. Set the
/// Number of Channels Requested field to a value of 0x0000": so only a Channel has a
/// count to give, and a Group or Function Block has none to get wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfileTarget {
    /// A Channel, the Manager Channel of a multi-Channel Profile, and the total
    /// number of Channels requested (M2-102 §2.5.1: no more than the peer's
    /// declared maximum).
    Channel {
        /// The Channel.
        channel: MidiChannel,
        /// "Manager + Member Channels".
        channels: ProfileChannels,
    },
    /// The whole Group (`0x7E`).
    Group,
    /// The whole Function Block (`0x7F`).
    FunctionBlock,
}

impl ProfileTarget {
    /// The Device ID the message goes to.
    #[must_use]
    pub const fn address(self) -> CiDeviceId {
        match self {
            Self::Channel { channel, .. } => CiDeviceId::Channel(channel),
            Self::Group => CiDeviceId::Group,
            Self::FunctionBlock => CiDeviceId::FunctionBlock,
        }
    }

    /// The Number Channels Requested field.
    #[must_use]
    pub const fn channels(self) -> ProfileChannels {
        match self {
            Self::Channel { channels, .. } => channels,
            Self::Group | Self::FunctionBlock => ProfileChannels::BY_ADDRESS,
        }
    }
}

/// A request in flight: what [`ProfileAction::Resolved`] names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// A Set Profile On.
    SetOn {
        /// Where.
        address: CiDeviceId,
        /// What.
        profile: ProfileId,
    },
    /// A Set Profile Off.
    SetOff {
        /// Where.
        address: CiDeviceId,
        /// What.
        profile: ProfileId,
    },
    /// A Profile Details Inquiry.
    Details {
        /// Where.
        address: CiDeviceId,
        /// What.
        profile: ProfileId,
        /// The target asked.
        target: InquiryTarget,
    },
}

impl Request {
    fn address(&self) -> CiDeviceId {
        match self {
            Self::SetOn { address, .. }
            | Self::SetOff { address, .. }
            | Self::Details { address, .. } => *address,
        }
    }

    fn profile(&self) -> ProfileId {
        match self {
            Self::SetOn { profile, .. }
            | Self::SetOff { profile, .. }
            | Self::Details { profile, .. } => *profile,
        }
    }

    fn sub_id2(&self) -> u8 {
        match self {
            Self::SetOn { .. } => SUB_ID2_SET_PROFILE_ON,
            Self::SetOff { .. } => SUB_ID2_SET_PROFILE_OFF,
            Self::Details { .. } => SUB_ID2_PROFILE_DETAILS_INQUIRY,
        }
    }

    fn is_set(&self) -> bool {
        !matches!(self, Self::Details { .. })
    }
}

/// How a [`Request`] ended.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Outcome {
    /// A Set Profile On or Off was answered by a Report: the state the peer says
    /// the Profile is in now (it is the state asked for unless the peer could not
    /// comply) and its Channel count, 0 for a Group or Function Block address or a
    /// Version 1 peer.
    Reported {
        /// Enabled or disabled.
        state: ProfileState,
        /// The Number Channels field of the Report.
        channels: ProfileChannels,
    },
    /// A Details Inquiry was answered with this data.
    Details(DetailsData),
    /// The peer sent a NAK: [`Nak::status_code`] says why ([`Nak::STATUS_PROFILE_NOT_SUPPORTED`]
    /// for a Profile it does not have at that address).
    Refused(Nak),
    /// [`PROFILE_TIMEOUT`] passed with no answer.
    TimedOut,
}

/// What a Report changed ([`ProfileAction::Changed`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProfileChange {
    /// Profile Added Report: newly supported, disabled.
    Added,
    /// Profile Removed Report: no longer supported.
    Removed,
    /// Profile Enabled Report.
    Enabled,
    /// Profile Disabled Report.
    Disabled,
}

/// What the driver does next.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProfileAction {
    /// Send this message to the peer, on the Group of the connection.
    Send(CiMessage),
    /// The final Reply (address `0x7F`) arrived: the table holds every Profile the
    /// peer declared.
    ProfilesKnown,
    /// [`PROFILE_TIMEOUT`] passed before the final Reply: the table holds what was
    /// heard.
    InquiryTimedOut,
    /// The peer NAKed the Profile Inquiry.
    InquiryRefused(Nak),
    /// A Report changed the table.
    Changed {
        /// Where.
        address: CiDeviceId,
        /// Which Profile.
        profile: ProfileId,
        /// What happened to it.
        change: ProfileChange,
    },
    /// A request ended.
    Resolved {
        /// Which.
        request: Request,
        /// How.
        outcome: Outcome,
    },
    /// Profile Specific Data arrived (§7.12), uninterpreted.
    SpecificData {
        /// The Device ID it came on.
        address: CiDeviceId,
        /// The Profile it belongs to.
        profile: ProfileId,
        /// The data.
        data: SpecificData,
    },
}

/// A request that cannot be made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestError {
    /// The same request, or a Set On or Off of the same Profile at the same
    /// address, is still waiting for its answer.
    InFlight,
}

impl std::fmt::Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a request for that Profile at that address is already in flight")
    }
}

impl std::error::Error for RequestError {}

/// One Profile of the table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProfileEntry {
    state: ProfileState,
    channels: Option<ProfileChannels>,
}

impl ProfileEntry {
    /// Enabled or disabled.
    #[must_use]
    pub const fn state(&self) -> ProfileState {
        self.state
    }

    /// The Number Channels of the last Enabled or Disabled Report of a Version 2
    /// peer, `None` while none arrived (a Reply to Profile Inquiry carries no count).
    #[must_use]
    pub const fn channels(&self) -> Option<ProfileChannels> {
        self.channels
    }
}

/// The Profiles a peer supports, per address.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProfileTable {
    addresses: BTreeMap<CiDeviceId, BTreeMap<ProfileId, ProfileEntry>>,
}

impl ProfileTable {
    /// The addresses with at least one Profile, Channels first, then the Group,
    /// then the Function Block.
    pub fn addresses(&self) -> impl Iterator<Item = CiDeviceId> + '_ {
        self.addresses.keys().copied()
    }

    /// The Profiles at one address, in Id order.
    pub fn profiles(
        &self,
        address: CiDeviceId,
    ) -> impl Iterator<Item = (ProfileId, &ProfileEntry)> + '_ {
        self.addresses
            .get(&address)
            .into_iter()
            .flat_map(|profiles| profiles.iter().map(|(id, entry)| (*id, entry)))
    }

    /// One Profile at one address.
    #[must_use]
    pub fn get(&self, address: CiDeviceId, profile: ProfileId) -> Option<&ProfileEntry> {
        self.addresses.get(&address)?.get(&profile)
    }

    /// Whether the table has no Profile.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.addresses.is_empty()
    }

    /// Sets one entry; whether it differs from what was there.
    fn set(&mut self, address: CiDeviceId, profile: ProfileId, entry: ProfileEntry) -> bool {
        self.addresses
            .entry(address)
            .or_default()
            .insert(profile, entry)
            != Some(entry)
    }

    fn remove(&mut self, address: CiDeviceId, profile: ProfileId) -> bool {
        let Some(profiles) = self.addresses.get_mut(&address) else {
            return false;
        };
        let removed = profiles.remove(&profile).is_some();
        if profiles.is_empty() {
            self.addresses.remove(&address);
        }
        removed
    }

    /// An address as a Reply declares it: the lists replace what was there, and a
    /// Profile that stays enabled keeps its Channel count.
    fn replace(&mut self, address: CiDeviceId, lists: &ProfileLists) {
        let before = self.addresses.remove(&address).unwrap_or_default();
        let mut after = BTreeMap::new();
        for (list, state) in [
            (lists.disabled(), ProfileState::Disabled),
            (lists.enabled(), ProfileState::Enabled),
        ] {
            for profile in list {
                let kept = before
                    .get(profile)
                    .filter(|entry| entry.state == state)
                    .and_then(|entry| entry.channels);
                after.insert(
                    *profile,
                    ProfileEntry {
                        state,
                        channels: kept,
                    },
                );
            }
        }
        if !after.is_empty() {
            self.addresses.insert(address, after);
        }
    }
}

#[derive(Clone, Debug)]
struct Inquiry {
    deadline: Instant,
    /// The addresses that replied.
    replied: BTreeSet<CiDeviceId>,
}

#[derive(Clone, Debug)]
struct Pending {
    request: Request,
    deadline: Instant,
}

/// The Initiator of Profile Configuration with one peer.
#[derive(Clone, Debug)]
pub struct ProfileInitiator {
    own: Muid,
    peer: Muid,
    table: ProfileTable,
    known: bool,
    inquiry: Option<Inquiry>,
    pending: Vec<Pending>,
}

impl ProfileInitiator {
    /// An Initiator for `peer`, from this device's MUID `own`, and the actions that
    /// start it at `now`: a Profile Inquiry to the Function Block.
    #[must_use]
    pub fn new(own: Muid, peer: Muid, now: Instant) -> (Self, Vec<ProfileAction>) {
        let mut initiator = Self {
            own,
            peer,
            table: ProfileTable::default(),
            known: false,
            inquiry: None,
            pending: Vec::new(),
        };
        let actions = initiator.inquire(now);
        (initiator, actions)
    }

    /// The peer's MUID.
    #[must_use]
    pub fn peer(&self) -> Muid {
        self.peer
    }

    /// The Profiles the peer is known to support.
    #[must_use]
    pub fn table(&self) -> &ProfileTable {
        &self.table
    }

    /// Whether an inquiry has ended with the final Reply, so the table is the peer's
    /// whole declaration (and the Reports since).
    #[must_use]
    pub fn is_known(&self) -> bool {
        self.known
    }

    /// Asks again, to the Function Block: a new inquiry starts (one already waiting
    /// is replaced). The table is kept until the final Reply replaces it.
    pub fn inquire(&mut self, now: Instant) -> Vec<ProfileAction> {
        self.inquiry = Some(Inquiry {
            deadline: now + PROFILE_TIMEOUT,
            replied: BTreeSet::new(),
        });
        vec![self.send(CiDeviceId::FunctionBlock, |header, to| {
            CiMessage::ProfileInquiry { header, to }
        })]
    }

    /// Asks the peer to enable `profile` at `target`; the answer is a
    /// [`ProfileAction::Resolved`].
    ///
    /// # Errors
    ///
    /// [`RequestError::InFlight`] when a Set On or Off of that Profile at that
    /// address is waiting.
    pub fn set_on(
        &mut self,
        target: ProfileTarget,
        profile: ProfileId,
        now: Instant,
    ) -> Result<Vec<ProfileAction>, RequestError> {
        let address = target.address();
        let channels = target.channels();
        self.begin(Request::SetOn { address, profile }, now)?;
        Ok(vec![self.send(address, |header, to| {
            CiMessage::SetProfileOn {
                header,
                to,
                profile,
                channels,
            }
        })])
    }

    /// Asks the peer to disable `profile` at `address`.
    ///
    /// # Errors
    ///
    /// [`RequestError::InFlight`] as for [`set_on`](Self::set_on).
    pub fn set_off(
        &mut self,
        address: CiDeviceId,
        profile: ProfileId,
        now: Instant,
    ) -> Result<Vec<ProfileAction>, RequestError> {
        self.begin(Request::SetOff { address, profile }, now)?;
        Ok(vec![self.send(address, |header, to| {
            CiMessage::SetProfileOff {
                header,
                to,
                profile,
            }
        })])
    }

    /// Asks the peer for a detail of `profile` at `address`.
    ///
    /// # Errors
    ///
    /// [`RequestError::InFlight`] when the same inquiry is waiting.
    pub fn details(
        &mut self,
        address: CiDeviceId,
        profile: ProfileId,
        target: InquiryTarget,
        now: Instant,
    ) -> Result<Vec<ProfileAction>, RequestError> {
        self.begin(
            Request::Details {
                address,
                profile,
                target,
            },
            now,
        )?;
        Ok(vec![self.send(address, |header, to| {
            CiMessage::ProfileDetailsInquiry {
                header,
                to,
                profile,
                target,
            }
        })])
    }

    /// When the Initiator next needs [`on_timeout`](Self::on_timeout), or `None`
    /// with nothing waiting.
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        self.inquiry
            .iter()
            .map(|inquiry| inquiry.deadline)
            .chain(self.pending.iter().map(|pending| pending.deadline))
            .min()
    }

    /// Time has passed: everything whose wait is over ends. Calling it early is
    /// harmless.
    pub fn on_timeout(&mut self, now: Instant) -> Vec<ProfileAction> {
        let mut actions = Vec::new();
        if self
            .inquiry
            .as_ref()
            .is_some_and(|inquiry| now >= inquiry.deadline)
        {
            self.inquiry = None;
            actions.push(ProfileAction::InquiryTimedOut);
        }
        let (expired, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|pending| now >= pending.deadline);
        self.pending = waiting;
        actions.extend(expired.into_iter().map(|pending| ProfileAction::Resolved {
            request: pending.request,
            outcome: Outcome::TimedOut,
        }));
        actions
    }

    /// A CI message of the connection's Group arrived.
    pub fn on_message(&mut self, decoded: &Decoded, now: Instant) -> Vec<ProfileAction> {
        let mut actions = Vec::new();
        let Decoded::Message(message) = decoded else {
            return actions;
        };
        let from_peer = message.header().source == self.peer;
        let to_us = message.destination() == Destination::Muid(self.own);
        let address = message.header().device_id;
        match message {
            CiMessage::ProfileReply { profiles, .. } if from_peer && to_us => {
                self.on_reply(address, profiles, &mut actions);
            }
            CiMessage::ProfileEnabled {
                header,
                profile,
                channels,
            } if from_peer => self.on_state(
                address,
                *profile,
                ProfileState::Enabled,
                *channels,
                header,
                &mut actions,
            ),
            CiMessage::ProfileDisabled {
                header,
                profile,
                channels,
            } if from_peer => self.on_state(
                address,
                *profile,
                ProfileState::Disabled,
                *channels,
                header,
                &mut actions,
            ),
            CiMessage::ProfileAdded { profile, .. } if from_peer => {
                let entry = ProfileEntry {
                    state: ProfileState::Disabled,
                    channels: None,
                };
                // A Profile already listed keeps its state: the Report says only
                // that it is supported.
                if self.table.get(address, *profile).is_none() {
                    self.table.set(address, *profile, entry);
                    actions.push(ProfileAction::Changed {
                        address,
                        profile: *profile,
                        change: ProfileChange::Added,
                    });
                }
            }
            CiMessage::ProfileRemoved { profile, .. } if from_peer => {
                if self.table.remove(address, *profile) {
                    actions.push(ProfileAction::Changed {
                        address,
                        profile: *profile,
                        change: ProfileChange::Removed,
                    });
                }
            }
            CiMessage::ProfileDetailsReply {
                profile,
                target,
                data,
                ..
            } if from_peer && to_us => {
                let at = self.pending.iter().position(|pending| {
                    pending.request
                        == Request::Details {
                            address,
                            profile: *profile,
                            target: *target,
                        }
                });
                if let Some(at) = at {
                    let pending = self.pending.remove(at);
                    actions.push(ProfileAction::Resolved {
                        request: pending.request,
                        outcome: Outcome::Details(data.clone()),
                    });
                }
            }
            CiMessage::ProfileSpecificData {
                to, profile, data, ..
            } if from_peer && (*to == Destination::Broadcast || to_us) => {
                actions.push(ProfileAction::SpecificData {
                    address,
                    profile: *profile,
                    data: data.clone(),
                });
            }
            CiMessage::Ack { ack, .. }
                if from_peer && to_us && ack.status_code == Ack::STATUS_TIMEOUT_WAIT =>
            {
                self.on_wait(address, ack, now);
            }
            CiMessage::Nak { nak, .. } if from_peer && to_us => {
                self.on_nak(address, nak, &mut actions);
            }
            _ => {}
        }
        actions
    }

    fn on_reply(
        &mut self,
        address: CiDeviceId,
        lists: &ProfileLists,
        actions: &mut Vec<ProfileAction>,
    ) {
        // A Reply is only an answer to an Inquiry.
        let Some(inquiry) = self.inquiry.as_mut() else {
            return;
        };
        inquiry.replied.insert(address);
        self.table.replace(address, lists);
        if address == CiDeviceId::FunctionBlock {
            // "As a final reply ... to indicate that no further replies will follow."
            let replied = std::mem::take(&mut inquiry.replied);
            self.inquiry = None;
            self.table.addresses.retain(|at, _| replied.contains(at));
            self.known = true;
            actions.push(ProfileAction::ProfilesKnown);
        }
    }

    fn on_state(
        &mut self,
        address: CiDeviceId,
        profile: ProfileId,
        state: ProfileState,
        channels: ProfileChannels,
        header: &CiHeader,
        actions: &mut Vec<ProfileAction>,
    ) {
        let entry = ProfileEntry {
            state,
            // A Version 1 sender has no count to report.
            channels: (header.ci_version.get() >= 2).then_some(channels),
        };
        if self.table.set(address, profile, entry) {
            actions.push(ProfileAction::Changed {
                address,
                profile,
                change: match state {
                    ProfileState::Enabled => ProfileChange::Enabled,
                    ProfileState::Disabled => ProfileChange::Disabled,
                },
            });
        }
        let at = self.pending.iter().position(|p| {
            p.request.is_set() && p.request.address() == address && p.request.profile() == profile
        });
        if let Some(at) = at {
            let pending = self.pending.remove(at);
            actions.push(ProfileAction::Resolved {
                request: pending.request,
                outcome: Outcome::Reported { state, channels },
            });
        }
    }

    /// A Timeout Wait: the transaction it names waits until `now` plus the time asked,
    /// when that is later than it already would.
    fn on_wait(&mut self, address: CiDeviceId, ack: &Ack, now: Instant) {
        let until = now + WAIT_STEP * u32::from(ack.status_data.get());
        let original = ack.ack_sub_id2.get();
        if original == SUB_ID2_PROFILE_INQUIRY {
            if address == CiDeviceId::FunctionBlock
                && let Some(inquiry) = self.inquiry.as_mut()
            {
                inquiry.deadline = inquiry.deadline.max(until);
            }
            return;
        }
        let details = ack.details.map(U7::get);
        if let Some(pending) = self.pending.iter_mut().find(|p| {
            p.request.sub_id2() == original
                && p.request.address() == address
                && details_name(details, p.request.profile())
        }) {
            pending.deadline = pending.deadline.max(until);
        }
    }

    fn on_nak(&mut self, address: CiDeviceId, nak: &Nak, actions: &mut Vec<ProfileAction>) {
        let original = nak.nak_sub_id2.get();
        if original == SUB_ID2_PROFILE_INQUIRY {
            if self.inquiry.take().is_some() {
                actions.push(ProfileAction::InquiryRefused(nak.clone()));
            }
            return;
        }
        // The details are the Profile Id (§5.11.3); a peer with none to give sends
        // zeroes.
        let details = nak.details.map(U7::get);
        let at = self.pending.iter().position(|p| {
            p.request.sub_id2() == original
                && p.request.address() == address
                && details_name(details, p.request.profile())
        });
        if let Some(at) = at {
            let pending = self.pending.remove(at);
            actions.push(ProfileAction::Resolved {
                request: pending.request,
                outcome: Outcome::Refused(nak.clone()),
            });
        }
    }

    fn begin(&mut self, request: Request, now: Instant) -> Result<(), RequestError> {
        let clash = self.pending.iter().any(|pending| {
            pending.request == request
                || (request.is_set()
                    && pending.request.is_set()
                    && pending.request.address() == request.address()
                    && pending.request.profile() == request.profile())
        });
        if clash {
            return Err(RequestError::InFlight);
        }
        self.pending.push(Pending {
            request,
            deadline: now + PROFILE_TIMEOUT,
        });
        Ok(())
    }

    fn send(
        &self,
        address: CiDeviceId,
        message: impl FnOnce(CiHeader, Muid) -> CiMessage,
    ) -> ProfileAction {
        ProfileAction::Send(message(
            CiHeader {
                device_id: address,
                ci_version: CI_VERSION,
                source: self.own,
            },
            self.peer,
        ))
    }
}

#[cfg(test)]
mod tests;
