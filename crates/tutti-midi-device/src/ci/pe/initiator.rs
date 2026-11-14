//! [`PeInitiator`]: the Initiator's side of Property Exchange with one discovered
//! peer (M2-101 §8, M2-103 §5, §9 to §12, §15).
//!
//! A pure state machine in the shape of [`ProfileInitiator`](crate::ProfileInitiator):
//! every step returns the [`PeAction`]s to carry out, time is an [`Instant`]
//! argument, and nothing here reads a clock or sends a byte. The caller makes one
//! per peer whose Discovery Reply declared Property Exchange, with the Receivable
//! Maximum SysEx Message Size of that Reply, routes to it every decoded message of
//! the connection's Group, sends each [`PeAction::Send`], calls
//! [`on_timeout`](PeInitiator::on_timeout) at [`deadline`](PeInitiator::deadline),
//! and drops it when the peer is lost or the device's MUID changes: an Invalidate
//! MUID ends every Subscription without a message (M2-103 §11.5).
//!
//! # The rules
//!
//! - Capabilities (M2-101 §8.5, M2-103 §3.2): [`new`](PeInitiator::new) sends
//!   Inquiry: Property Exchange Capabilities, and nothing else may be asked before
//!   the Reply, which gives the number of requests the peer takes at once and the
//!   Property Exchange version it follows ([`PeAction::Capabilities`]). A request
//!   before it is refused ([`PeError::CapabilitiesPending`]); a peer that does not
//!   answer within [`PE_TIMEOUT`] or refuses with a NAK is
//!   [`PeAction::CapabilitiesFailed`], and [`ask_capabilities`](PeInitiator::ask_capabilities)
//!   asks again.
//! - Request IDs (M2-103 §5.3): [`start`](PeInitiator::start) gives each request one of
//!   the 128 IDs, none in use for another request of this peer, and never more
//!   requests at once than the peer's count ([`PeError::TooManyRequests`]). The ID comes
//!   free when the request ends, by a reply, a NAK or a timeout; the next ID is the
//!   one after the last given, so an ID is reused as late as the pool allows.
//! - Get (M2-103 §9, §15.2): the Inquiry is one chunk of Header Data. The reply is
//!   reassembled, with an ACK for each chunk when the request asked for Flow
//!   Control, and its Property Data decoded in the `mutualEncoding` the reply names
//!   (ASCII when it names none, §6.1.6).
//! - Set (§10, §15.3): the Property Data is encoded, split to the peer's receivable
//!   size and sent. Under Flow Control each chunk waits for the ACK that asks for
//!   the next, and the last needs none, since the reply acknowledges it. A NAK that
//!   asks for the most recent chunk again (§15.4), or no ACK within [`PE_TIMEOUT`],
//!   sends that chunk again; the third repeat that fails ends the transaction with
//!   a Terminate NAK (§15.5).
//! - Subscribe (§11): Start is a Get-shaped inquiry; the reply names the Subscription
//!   Id, and the table ([`subscriptions`](PeInitiator::subscriptions)) maps it to the
//!   Resource. The Responder's Subscription messages are reassembled, answered with
//!   a Reply to Subscription (§11.2) and given out as a [`PeNotification`]; End by
//!   either device removes the entry.
//! - Time (§12.2): a reply, or the chunk after one, comes within [`PE_TIMEOUT`]
//!   (3 seconds); an ACK with Timeout Wait adds its wait to the time that is left
//!   (§12.2.1). A request that is not answered in time ends [`PeOutcome::TimedOut`]
//!   and the Initiator sends the NAK that says so (§12.2.3). A time is over at its
//!   deadline: a message arriving at the instant is late.
//! - Notify (M2-101 §8.13, M2-103 §12): the superseded message, sub-ID#2 `0x3F`, still
//!   read for a peer that sends it. It names a transaction by Request ID alone, so it
//!   acts on the request with that ID and on a Subscription message of the peer's
//!   with it, and nothing is sent back. Status 144 ends the request
//!   ([`PeOutcome::Terminated`], by the Responder) and drops the message; 408 ends it
//!   as the NAK of a timeout ([`PeOutcome::Nak`]) and drops it; 100 holds it one more
//!   [`PE_TIMEOUT`], as a Timeout Wait whose period the message does not name. A
//!   Notify with another status, or for no request in flight, changes nothing.
//! - Status (§7.4.1, Table 15): a reply ends the request with its status, as
//!   [`PeOutcome::Success`] for 200 and 201, [`PeOutcome::Retry`] for 341 to 343, which
//!   the table says to retry and this machine leaves to the caller, and
//!   [`PeOutcome::Failed`] for 4xx and 5xx. A NAK with Terminate ends the request
//!   [`PeOutcome::Terminated`] (§12.1).
//!
//! A message from another source, or not addressed to this device, changes nothing.
//! A chunk that could not be read never reaches the machine, and its request times
//! out.

use std::time::Instant;

use super::body::{BodyLimit, PeBodyError, decode_body, encode_body};
use super::capabilities::SimultaneousRequests;
use super::capabilities::{PeCapabilities, PeCiMessage, SUB_ID2_PE_CAPABILITIES_INQUIRY};
use super::chunk::{PeChunk, PeChunkError, PeRequestId, split_message};
use super::flow::{PE_TIMEOUT, PeAnswer, PeMessageKind, PeSignal, PeTerminator, PeTransaction};
use super::header::{
    PeEncoding, PeEndedBy, PeHeader, PePage, PeReply, PeRequest, PeSetScope, PeStatus,
    PeStatusClass, PeSubscription, PeUpdateCommand,
};
use super::limits::{MediaType, PeMessage, ResourceId, ResourceName, SubscribeId};
use super::reassemble::{
    PeFlowControl, PeReassembler, PeReassemblyError, PeReassemblyLimits, PeReceived, PeStep,
};
use crate::{CiHeader, CiMessage, Decoded, Destination, MaxSysexSize, Muid, Nak, TimeoutWait};

/// The most times a chunk is sent again, whether for a NAK or for the lack of an
/// ACK, before the transaction ends (M2-103 §15.5: it fails after three).
const MAX_RESENDS: u8 = 3;

/// The extra time a Notify with the Timeout Wait status adds: the message names
/// no period, and one more [`PE_TIMEOUT`] keeps the transaction open as long as
/// a request that was waiting for the first (3 seconds, 30 tenths).
const NOTIFY_WAIT: TimeoutWait = TimeoutWait::of(30);

/// What an Initiator holds, fixed for the peer it talks to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeSettings {
    /// What this device reports to the peer in the Capabilities Inquiry: how
    /// many requests of the peer's it takes at once (M2-103 §5.3).
    pub capabilities: PeCapabilities,
    /// The peer's Receivable Maximum SysEx Message Size, from its Discovery
    /// Reply: every chunk sent fits it (M2-101 §8.3).
    pub peer_max_sysex: MaxSysexSize,
    /// What the replies and Subscription messages being received may buffer.
    pub reassembly: PeReassemblyLimits,
    /// The most a decoded body may be: a few bytes of zlib inflate to a great
    /// many.
    pub body_limit: BodyLimit,
}

/// A Get inquiry, and the Start of a Subscription, which has the same header
/// (M2-103 §11.1).
#[derive(Clone, Debug, PartialEq)]
pub struct PeGet {
    /// The Resource.
    pub resource: ResourceName,
    /// The entry of the Resource, for one that has several (§8.5).
    pub res_id: Option<ResourceId>,
    /// The pagination of a List Resource (§8.6.2).
    pub page: Option<PePage>,
    /// The encoding wanted of the reply's Property Data. ASCII, which the header
    /// leaves out, is the default (§6.1.6).
    pub encoding: PeEncoding,
    /// Whether the reply is sent under Flow Control (§15.2).
    pub flow_control: PeFlowControl,
}

impl PeGet {
    /// A request for `resource` with no other property: ASCII, one entry, no Flow
    /// Control.
    #[must_use]
    pub fn new(resource: ResourceName) -> Self {
        Self {
            resource,
            res_id: None,
            page: None,
            encoding: PeEncoding::Ascii,
            flow_control: PeFlowControl::Off,
        }
    }

    fn request(&self) -> PeRequest {
        let mut request = PeRequest::new(self.resource.clone());
        request.res_id = self.res_id.clone();
        request.page = self.page;
        request.mutual_encoding = (self.encoding != PeEncoding::Ascii).then_some(self.encoding);
        request.flow_control = self.flow_control;
        request
    }
}

/// A Set inquiry (M2-103 §10).
#[derive(Clone, Debug, PartialEq)]
pub struct PeSet {
    /// The Resource.
    pub resource: ResourceName,
    /// The entry of the Resource, for one that has several (§8.5).
    pub res_id: Option<ResourceId>,
    /// Whether it replaces the whole Property Data or is a Partial Set (§10.2).
    pub scope: PeSetScope,
    /// The encoding the Property Data is sent in.
    pub encoding: PeEncoding,
    /// Whether the chunks are sent under Flow Control (§15.3).
    pub flow_control: PeFlowControl,
    /// The media type of Property Data that is not JSON (§7.5).
    pub media_type: Option<MediaType>,
    /// The Property Data, before its encoding.
    pub body: Vec<u8>,
}

impl PeSet {
    /// A Full Set of `body` to `resource`, in ASCII, with no Flow Control.
    #[must_use]
    pub fn new(resource: ResourceName, body: Vec<u8>) -> Self {
        Self {
            resource,
            res_id: None,
            scope: PeSetScope::Whole,
            encoding: PeEncoding::Ascii,
            flow_control: PeFlowControl::Off,
            media_type: None,
            body,
        }
    }

    fn request(&self) -> PeRequest {
        let mut request = PeRequest::new(self.resource.clone());
        request.res_id = self.res_id.clone();
        request.mutual_encoding = (self.encoding != PeEncoding::Ascii).then_some(self.encoding);
        request.flow_control = self.flow_control;
        request.set_scope = self.scope;
        request.media_type = self.media_type.clone();
        request
    }
}

/// What an app asks of the peer.
#[derive(Clone, Debug, PartialEq)]
pub enum PeCall {
    /// Inquiry: Get Property Data.
    Get(PeGet),
    /// Inquiry: Set Property Data.
    Set(PeSet),
    /// Start a Subscription to a Resource.
    Subscribe(PeGet),
    /// End a Subscription the table holds.
    Unsubscribe(SubscribeId),
}

/// Which [`PeCall`] a request was.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PeKind {
    /// A Get.
    Get,
    /// A Set.
    Set,
    /// A Start Subscription.
    Subscribe,
    /// An End Subscription.
    Unsubscribe,
}

/// A request that cannot be made.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PeError {
    /// The Capabilities Reply has not come: nothing may be asked before it
    /// (M2-103 §3.2).
    #[error("the peer's Property Exchange Capabilities are not known yet")]
    CapabilitiesPending,
    /// The Capabilities Inquiry timed out or was refused.
    #[error("the peer did not give its Property Exchange Capabilities")]
    CapabilitiesUnavailable,
    /// The peer's count of simultaneous requests are all in use (M2-103 §5.3).
    #[error("the peer takes {limit} requests at once and that many are in flight")]
    TooManyRequests {
        /// The count the peer reported.
        limit: SimultaneousRequests,
    },
    /// The message cannot be split into chunks.
    #[error(transparent)]
    Chunk(#[from] PeChunkError),
    /// The Property Data cannot be encoded.
    #[error(transparent)]
    Body(#[from] PeBodyError),
    /// The Subscription is not in the table.
    #[error("no subscription {0}")]
    UnknownSubscription(SubscribeId),
}

/// The Property Exchange reply to a request: its status, its header and its
/// Property Data decoded.
#[derive(Clone, Debug, PartialEq)]
pub struct PeResponse {
    /// The Reply's Header Data.
    pub header: PeReply,
    /// The Property Data, decoded; empty for a reply that has none.
    pub body: Vec<u8>,
}

impl PeResponse {
    /// The status (§7.4.1).
    #[must_use]
    pub fn status(&self) -> PeStatus {
        self.header.status
    }
}

/// Why a reply could not be used.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PeUnreadable {
    /// Its chunks were refused.
    #[error(transparent)]
    Chunks(#[from] PeReassemblyError),
    /// The sender abandoned the message with a final chunk numbered 0 (M2-101
    /// §8.3).
    #[error("the reply was abandoned by its sender")]
    Abandoned,
    /// The Header Data of the reply is not a Reply's.
    #[error("the Header Data is not a reply")]
    NotAReply,
    /// The status is one a Notify message uses (§7.4.1, §12), not a Reply.
    #[error("{0:?} is not a Reply status")]
    NotAReplyStatus(PeStatus),
    /// The Property Data does not decode in the encoding the reply names.
    #[error(transparent)]
    Body(#[from] PeBodyError),
    /// The reply that starts a Subscription is a success and names no Subscription
    /// Id (§11.1).
    #[error("the reply to Start Subscription has no subscribeId")]
    NoSubscribeId,
}

/// How a request ended.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum PeOutcome {
    /// The reply has a success status, 200 or 201 (§7.4.1).
    Success(PeResponse),
    /// The reply has a status the spec says to retry, 341 to 343: the machine does
    /// not, and the caller decides.
    Retry(PeResponse),
    /// The reply has a status of a failure, 400 to 500.
    Failed(PeResponse),
    /// The peer ended the request with a NAK that terminates it (§12.1), or this
    /// Initiator did after a chunk it could not get acknowledged.
    Terminated(PeTerminator),
    /// The peer ended the request with the NAK of a failure other than a
    /// terminate: out of sequence, malformed or a timeout.
    Nak(PeSignal),
    /// No reply came within the time (§12.2).
    TimedOut,
    /// What came cannot be used.
    Unreadable(PeUnreadable),
}

/// A Subscription the Initiator holds (M2-103 §11).
#[derive(Clone, Debug, PartialEq)]
pub struct PeSubscribed {
    /// The Resource.
    pub resource: ResourceName,
    /// The entry of the Resource.
    pub res_id: Option<ResourceId>,
}

/// What a Subscription update says.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum PeChange {
    /// `full`: the whole Property Data, decoded.
    Full(Vec<u8>),
    /// `partial`: a change to a subset, decoded, in the format of a Partial Set.
    Partial(Vec<u8>),
    /// `notify`: the Responder asks for a Get of the Resource.
    Notify,
    /// A `full` or `partial` whose Property Data does not decode. The reply was
    /// still a success, and the Initiator Gets the Resource to stay in step
    /// (§11.2).
    Unreadable(PeBodyError),
}

/// A message the Responder sent about a Subscription.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum PeNotification {
    /// An update (§11.1).
    Changed {
        /// The Subscription.
        subscribe_id: SubscribeId,
        /// Its Resource.
        resource: ResourceName,
        /// What changed.
        change: PeChange,
        /// The media type of Property Data that is not JSON (§7.5).
        media_type: Option<MediaType>,
    },
    /// The Responder ended the Subscription, which is no longer in the table.
    Ended {
        /// The Subscription.
        subscribe_id: SubscribeId,
        /// Its Resource.
        resource: ResourceName,
    },
}

/// Why the Capabilities exchange failed ([`PeAction::CapabilitiesFailed`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PeCapabilitiesFailure {
    /// No Reply within [`PE_TIMEOUT`].
    TimedOut,
    /// The peer sent a NAK of the Inquiry.
    Refused(Nak),
}

/// What the driver does next.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum PeAction {
    /// Send this message to the peer, on the Group of the connection.
    Send(CiMessage),
    /// The peer's Capabilities arrived: requests may be made.
    Capabilities(PeCapabilities),
    /// The Capabilities exchange failed.
    CapabilitiesFailed(PeCapabilitiesFailure),
    /// A request ended.
    Finished {
        /// Its Request ID, which [`start`](PeInitiator::start) returned and which
        /// may be given to another request now.
        request_id: PeRequestId,
        /// Which call it was.
        kind: PeKind,
        /// How it ended.
        outcome: PeOutcome,
    },
    /// The Responder sent a Subscription message.
    Notification(PeNotification),
}

#[derive(Clone, Copy, Debug)]
enum Phase {
    Asking { deadline: Instant },
    Ready(PeCapabilities),
    Unavailable,
}

/// The Request IDs of one peer: a set of the 128 that are in use, and where the
/// next search starts.
#[derive(Clone, Debug, Default)]
struct RequestIds {
    used: u128,
    next: u8,
}

impl RequestIds {
    const COUNT: u8 = 128;

    /// An ID not in use, searching on from the one after the last given, unless
    /// `limit` IDs are in use already.
    fn allocate(&mut self, limit: SimultaneousRequests) -> Option<PeRequestId> {
        if self.used.count_ones() >= u32::from(limit.get()) {
            return None;
        }
        (0..Self::COUNT)
            .map(|step| (self.next + step) % Self::COUNT)
            .find(|n| self.used & (1u128 << n) == 0)
            .and_then(|n| {
                self.used |= 1u128 << n;
                self.next = (n + 1) % Self::COUNT;
                PeRequestId::new(n)
            })
    }

    fn release(&mut self, id: PeRequestId) {
        self.used &= !(1u128 << id.get());
    }
}

#[derive(Clone, Debug)]
enum Work {
    Get {
        flow: PeFlowControl,
    },
    Set {
        chunks: Vec<PeChunk>,
        /// How many chunks have been sent: the last is the one in flight.
        sent: usize,
        flow: PeFlowControl,
        resends: u8,
    },
    Subscribe {
        resource: ResourceName,
        res_id: Option<ResourceId>,
        flow: PeFlowControl,
    },
    Unsubscribe,
}

impl Work {
    fn kind(&self) -> PeKind {
        match self {
            Self::Get { .. } => PeKind::Get,
            Self::Set { .. } => PeKind::Set,
            Self::Subscribe { .. } => PeKind::Subscribe,
            Self::Unsubscribe => PeKind::Unsubscribe,
        }
    }

    fn transaction(&self) -> PeTransaction {
        match self.kind() {
            PeKind::Get => PeTransaction::Get,
            PeKind::Set => PeTransaction::Set,
            PeKind::Subscribe | PeKind::Unsubscribe => PeTransaction::Subscription,
        }
    }

    fn reply_kind(&self) -> PeMessageKind {
        PeMessageKind {
            transaction: self.transaction(),
            reply: true,
        }
    }

    fn flow(&self) -> PeFlowControl {
        match self {
            Self::Get { flow } | Self::Subscribe { flow, .. } | Self::Set { flow, .. } => *flow,
            Self::Unsubscribe => PeFlowControl::Off,
        }
    }
}

#[derive(Clone, Debug)]
struct Pending {
    id: PeRequestId,
    work: Work,
    deadline: Instant,
}

/// The Initiator of Property Exchange with one peer.
#[derive(Debug)]
pub struct PeInitiator {
    own: Muid,
    peer: Muid,
    settings: PeSettings,
    phase: Phase,
    ids: RequestIds,
    pending: Vec<Pending>,
    reassembler: PeReassembler,
    subscriptions: Vec<(SubscribeId, PeSubscribed)>,
}

impl PeInitiator {
    /// An Initiator for `peer`, from this device's MUID `own`, and the actions that
    /// start it at `now`: the Capabilities Inquiry.
    #[must_use]
    pub fn new(own: Muid, peer: Muid, settings: PeSettings, now: Instant) -> (Self, Vec<PeAction>) {
        let mut initiator = Self {
            own,
            peer,
            settings,
            phase: Phase::Unavailable,
            ids: RequestIds::default(),
            pending: Vec::new(),
            reassembler: PeReassembler::new(settings.reassembly),
            subscriptions: Vec::new(),
        };
        let actions = initiator.ask_capabilities(now);
        (initiator, actions)
    }

    /// The peer's MUID.
    #[must_use]
    pub fn peer(&self) -> Muid {
        self.peer
    }

    /// What the peer reported, once its Reply has come.
    #[must_use]
    pub fn capabilities(&self) -> Option<&PeCapabilities> {
        match &self.phase {
            Phase::Ready(capabilities) => Some(capabilities),
            _ => None,
        }
    }

    /// The Subscriptions that are active, with their Resources.
    pub fn subscriptions(&self) -> impl Iterator<Item = (&SubscribeId, &PeSubscribed)> {
        self.subscriptions.iter().map(|(id, sub)| (id, sub))
    }

    /// How many requests are in flight.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.pending.len()
    }

    /// Sends the Capabilities Inquiry again and waits for the Reply, which
    /// replaces what the peer reported before.
    pub fn ask_capabilities(&mut self, now: Instant) -> Vec<PeAction> {
        self.phase = Phase::Asking {
            deadline: now + PE_TIMEOUT,
        };
        vec![PeAction::Send(CiMessage::PropertyExchange {
            header: self.header(),
            to: self.peer,
            message: PeCiMessage::CapabilitiesInquiry(self.settings.capabilities),
        })]
    }

    /// Makes a request. Returns its Request ID, which names it in
    /// [`PeAction::Finished`], and the actions that send it.
    ///
    /// # Errors
    ///
    /// [`PeError::CapabilitiesPending`] or [`PeError::CapabilitiesUnavailable`]
    /// before the peer's Capabilities are known, [`PeError::TooManyRequests`] when
    /// the peer's count of requests at once are in flight,
    /// [`PeError::UnknownSubscription`] for an End of a Subscription not in the
    /// table, and [`PeError::Chunk`] and [`PeError::Body`] for a message that
    /// cannot be sent to this peer. A refused request uses no Request ID.
    pub fn start(
        &mut self,
        call: PeCall,
        now: Instant,
    ) -> Result<(PeRequestId, Vec<PeAction>), PeError> {
        let capabilities = match self.phase {
            Phase::Ready(capabilities) => capabilities,
            Phase::Asking { .. } => return Err(PeError::CapabilitiesPending),
            Phase::Unavailable => return Err(PeError::CapabilitiesUnavailable),
        };
        let unsubscribed = match &call {
            PeCall::Unsubscribe(id) => Some(
                self.subscriptions
                    .iter()
                    .position(|(held, _)| held == id)
                    .ok_or_else(|| PeError::UnknownSubscription(id.clone()))?,
            ),
            _ => None,
        };
        let id = self
            .ids
            .allocate(capabilities.requests)
            .ok_or(PeError::TooManyRequests {
                limit: capabilities.requests,
            })?;
        let (work, chunks) = match self.build(id, call) {
            Ok(built) => built,
            Err(error) => {
                self.ids.release(id);
                return Err(error);
            }
        };
        if let Some(at) = unsubscribed {
            self.subscriptions.remove(at);
        }
        let kind = PeMessageKind {
            transaction: work.transaction(),
            reply: false,
        };
        let first_only = matches!(
            &work,
            Work::Set {
                flow: PeFlowControl::On,
                ..
            }
        );
        let actions = chunks
            .iter()
            .take(if first_only { 1 } else { chunks.len() })
            .map(|chunk| self.send_chunk(kind, chunk.clone()))
            .collect();
        let work = match work {
            Work::Set { flow, resends, .. } => Work::Set {
                sent: if first_only { 1 } else { chunks.len() },
                chunks,
                flow,
                resends,
            },
            other => other,
        };
        self.pending.push(Pending {
            id,
            work,
            deadline: now + PE_TIMEOUT,
        });
        Ok((id, actions))
    }

    /// The work of a call and the chunks of its inquiry.
    fn build(&self, id: PeRequestId, call: PeCall) -> Result<(Work, Vec<PeChunk>), PeError> {
        let max = self.settings.peer_max_sysex;
        let (header, body, work) = match call {
            PeCall::Get(get) => (
                PeHeader::Request(get.request()),
                Vec::new(),
                Work::Get {
                    flow: get.flow_control,
                },
            ),
            PeCall::Set(set) => (
                PeHeader::Request(set.request()),
                encode_body(set.encoding, &set.body)?,
                Work::Set {
                    chunks: Vec::new(),
                    sent: 0,
                    flow: set.flow_control,
                    resends: 0,
                },
            ),
            PeCall::Subscribe(get) => (
                PeHeader::Subscription(PeSubscription::Start(get.request())),
                Vec::new(),
                Work::Subscribe {
                    resource: get.resource.clone(),
                    res_id: get.res_id.clone(),
                    flow: get.flow_control,
                },
            ),
            PeCall::Unsubscribe(subscribe_id) => (
                PeHeader::Subscription(PeSubscription::End {
                    subscribe_id,
                    ended_by: Some(PeEndedBy::Initiator),
                    extra: Default::default(),
                }),
                Vec::new(),
                Work::Unsubscribe,
            ),
        };
        Ok((work, split_message(id, &header, &body, max)?))
    }

    /// When the Initiator next needs [`on_timeout`](Self::on_timeout), or `None`
    /// with nothing waiting.
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        let asking = match self.phase {
            Phase::Asking { deadline } => Some(deadline),
            _ => None,
        };
        asking
            .into_iter()
            .chain(self.pending.iter().map(|pending| pending.deadline))
            .chain(self.reassembler.next_deadline())
            .min()
    }

    /// Time has passed: everything whose wait is over ends. Calling it early is
    /// harmless.
    pub fn on_timeout(&mut self, now: Instant) -> Vec<PeAction> {
        let mut actions = Vec::new();
        if matches!(self.phase, Phase::Asking { deadline } if now >= deadline) {
            self.phase = Phase::Unavailable;
            actions.push(PeAction::CapabilitiesFailed(
                PeCapabilitiesFailure::TimedOut,
            ));
        }
        // A message whose next chunk is late: the reassembler dropped it.
        for expired in self.reassembler.expire(now) {
            actions.push(self.signal_action(expired.kind.transaction, expired.send));
            if expired.kind.reply
                && let Some(at) = self.find(expired.send.request_id(), expired.kind)
            {
                self.conclude(at, PeOutcome::TimedOut, &mut actions);
            }
        }
        while let Some(at) = self.pending.iter().position(|p| now >= p.deadline) {
            self.on_pending_timeout(at, now, &mut actions);
        }
        actions
    }

    fn on_pending_timeout(&mut self, at: usize, now: Instant, actions: &mut Vec<PeAction>) {
        let id = self.pending[at].id;
        let transaction = self.pending[at].work.transaction();
        // Under Flow Control a chunk with no ACK is sent again (§15.5).
        if let Work::Set {
            chunks,
            sent,
            flow: PeFlowControl::On,
            resends,
        } = &mut self.pending[at].work
            && *sent < chunks.len()
        {
            if *resends < MAX_RESENDS {
                *resends += 1;
                let chunk = chunks[*sent - 1].clone();
                self.pending[at].deadline = now + PE_TIMEOUT;
                let action = self.send_chunk(PeMessageKind::SET_INQUIRY, chunk);
                actions.push(action);
            } else {
                let signal = PeSignal::Terminate {
                    request_id: id,
                    by: PeTerminator::Initiator,
                };
                actions.push(self.signal_action(transaction, signal));
                self.conclude(at, PeOutcome::TimedOut, actions);
            }
            return;
        }
        let signal = PeSignal::TimedOut { request_id: id };
        actions.push(self.signal_action(transaction, signal));
        self.conclude(at, PeOutcome::TimedOut, actions);
    }

    /// A CI message of the connection's Group arrived.
    pub fn on_message(&mut self, decoded: &Decoded, now: Instant) -> Vec<PeAction> {
        let mut actions = Vec::new();
        let Decoded::Message(message) = decoded else {
            return actions;
        };
        if message.header().source != self.peer
            || message.destination() != Destination::Muid(self.own)
        {
            return actions;
        }
        match message {
            CiMessage::PropertyExchange { message, .. } => match message {
                PeCiMessage::CapabilitiesReply(capabilities) => {
                    self.phase = Phase::Ready(*capabilities);
                    actions.push(PeAction::Capabilities(*capabilities));
                }
                PeCiMessage::Chunk { kind, chunk } => {
                    self.on_chunk(*kind, chunk.clone(), now, &mut actions);
                }
                PeCiMessage::Notify(chunk) => self.on_notify(chunk, &mut actions),
                PeCiMessage::CapabilitiesInquiry(_) => {}
            },
            CiMessage::Ack { ack, .. } => {
                if let Some((transaction, signal)) = PeSignal::from_ack(ack) {
                    self.on_signal(transaction, signal, now, &mut actions);
                }
            }
            CiMessage::Nak { nak, .. } => {
                if nak.nak_sub_id2.get() == SUB_ID2_PE_CAPABILITIES_INQUIRY {
                    if matches!(self.phase, Phase::Asking { .. }) {
                        self.phase = Phase::Unavailable;
                        actions.push(PeAction::CapabilitiesFailed(
                            PeCapabilitiesFailure::Refused(nak.clone()),
                        ));
                    }
                } else if let Some((transaction, signal)) = PeSignal::from_nak(nak) {
                    self.on_signal(transaction, signal, now, &mut actions);
                }
            }
            _ => {}
        }
        actions
    }

    /// The superseded Notify message (M2-101 §8.13, M2-103 §12): it names a
    /// transaction by Request ID alone, so it is read as it would be for any
    /// transaction of that ID, this Initiator's request or a Subscription message
    /// the peer is sending, and no NAK answers it.
    fn on_notify(&mut self, chunk: &PeChunk, actions: &mut Vec<PeAction>) {
        let id = chunk.request_id();
        let Some(status) = chunk.notified() else {
            return;
        };
        let at = self.pending.iter().position(|p| p.id == id);
        match status {
            // As an ACK that asks to wait, with no period named: one more timeout.
            PeStatus::TimeoutWait => {
                let Some(at) = at else { return };
                NOTIFY_WAIT.extend(&mut self.pending[at].deadline);
                let kind = self.pending[at].work.reply_kind();
                self.reassembler.extend(self.peer, kind, id, NOTIFY_WAIT);
            }
            // As a NAK that terminates; the peer is the Responder.
            PeStatus::Terminate => {
                self.reassembler
                    .terminate(self.peer, PeMessageKind::SUBSCRIPTION_INQUIRY, id);
                if let Some(at) = at {
                    self.conclude(at, PeOutcome::Terminated(PeTerminator::Responder), actions);
                }
            }
            // As a NAK that says a timeout occurred.
            PeStatus::TimedOut => {
                self.reassembler
                    .terminate(self.peer, PeMessageKind::SUBSCRIPTION_INQUIRY, id);
                if let Some(at) = at {
                    let signal = PeSignal::TimedOut { request_id: id };
                    self.conclude(at, PeOutcome::Nak(signal), actions);
                }
            }
            // No other status is one a Notify carries (§12).
            PeStatus::Ok
            | PeStatus::Accepted
            | PeStatus::Unavailable
            | PeStatus::BadData
            | PeStatus::TooManyRequests
            | PeStatus::BadRequest
            | PeStatus::Forbidden
            | PeStatus::NotFound
            | PeStatus::NotAllowed
            | PeStatus::FlowControlUnsupported
            | PeStatus::FlowControlRequired
            | PeStatus::PayloadTooLarge
            | PeStatus::UnsupportedMedia
            | PeStatus::InvalidVersion
            | PeStatus::InternalError => {}
        }
    }

    fn on_chunk(
        &mut self,
        kind: PeMessageKind,
        chunk: PeChunk,
        now: Instant,
        actions: &mut Vec<PeAction>,
    ) {
        let id = chunk.request_id();
        let flow = if kind.reply {
            match self.find(id, kind) {
                Some(at) => self.pending[at].work.flow(),
                // A reply to nothing in flight: late, or never ours.
                None => return,
            }
        } else if kind == PeMessageKind::SUBSCRIPTION_INQUIRY {
            PeFlowControl::Off
        } else {
            // A Get or a Set inquiry is a Responder's to read.
            return;
        };
        match self.reassembler.push(self.peer, kind, chunk, flow, now) {
            Ok(PeStep::Pending { send }) => {
                actions.extend(send.map(|s| self.signal_action(kind.transaction, s)));
                if let Some(at) = self.find(id, kind) {
                    self.pending[at].deadline = now + PE_TIMEOUT;
                }
            }
            Ok(PeStep::Complete { message, send }) => {
                actions.extend(send.map(|s| self.signal_action(kind.transaction, s)));
                if kind.reply {
                    self.on_reply(*message, actions);
                } else {
                    self.on_subscription(*message, actions);
                }
            }
            Ok(PeStep::Abandoned) => {
                if let Some(at) = self.find(id, kind) {
                    self.conclude(at, PeOutcome::Unreadable(PeUnreadable::Abandoned), actions);
                }
            }
            Err(refused) => {
                actions.extend(
                    refused
                        .send
                        .map(|s| self.signal_action(kind.transaction, s)),
                );
                if let Some(at) = self.find(id, kind) {
                    self.conclude(
                        at,
                        PeOutcome::Unreadable(PeUnreadable::Chunks(refused.error)),
                        actions,
                    );
                }
            }
        }
    }

    /// A reply whole: the request it answers ends.
    fn on_reply(&mut self, received: PeReceived, actions: &mut Vec<PeAction>) {
        let Some(at) = self.find(received.request_id, received.kind) else {
            return;
        };
        let outcome = self.reply_outcome(received);
        if let (
            PeOutcome::Success(response),
            Work::Subscribe {
                resource, res_id, ..
            },
        ) = (&outcome, &self.pending[at].work)
        {
            match &response.header.subscribe_id {
                Some(subscribe_id) => self.subscriptions.push((
                    subscribe_id.clone(),
                    PeSubscribed {
                        resource: resource.clone(),
                        res_id: res_id.clone(),
                    },
                )),
                None => {
                    self.conclude(
                        at,
                        PeOutcome::Unreadable(PeUnreadable::NoSubscribeId),
                        actions,
                    );
                    return;
                }
            }
        }
        self.conclude(at, outcome, actions);
    }

    fn reply_outcome(&self, received: PeReceived) -> PeOutcome {
        let PeHeader::Reply(header) = received.header else {
            return PeOutcome::Unreadable(PeUnreadable::NotAReply);
        };
        // No Reply carries a Notify code (§7.4.1): such a reply is malformed.
        let outcome: fn(PeResponse) -> PeOutcome = match header.status.class() {
            PeStatusClass::Success => PeOutcome::Success,
            PeStatusClass::Retry => PeOutcome::Retry,
            PeStatusClass::ClientError | PeStatusClass::ServerError => PeOutcome::Failed,
            PeStatusClass::Notify => {
                return PeOutcome::Unreadable(PeUnreadable::NotAReplyStatus(header.status));
            }
        };
        match self.decode(header.mutual_encoding, &received.body) {
            Ok(body) => outcome(PeResponse { header, body }),
            Err(error) => PeOutcome::Unreadable(error.into()),
        }
    }

    fn decode(&self, encoding: Option<PeEncoding>, wire: &[u8]) -> Result<Vec<u8>, PeBodyError> {
        if wire.is_empty() {
            return Ok(Vec::new());
        }
        decode_body(
            encoding.unwrap_or(PeEncoding::Ascii),
            wire,
            self.settings.body_limit,
        )
    }

    /// A Subscription message of the Responder, whole: it is answered and given
    /// out (§11.2).
    fn on_subscription(&mut self, received: PeReceived, actions: &mut Vec<PeAction>) {
        let known = |this: &Self, id: &SubscribeId| {
            this.subscriptions.iter().position(|(held, _)| held == id)
        };
        let refusal = match &received.header {
            PeHeader::Subscription(PeSubscription::Update {
                command,
                subscribe_id,
                mutual_encoding,
                media_type,
                ..
            }) => match known(self, subscribe_id) {
                Some(at) => {
                    let change = match command {
                        PeUpdateCommand::Notify => PeChange::Notify,
                        PeUpdateCommand::Full | PeUpdateCommand::Partial => {
                            match self.decode(*mutual_encoding, &received.body) {
                                Ok(body) if *command == PeUpdateCommand::Full => {
                                    PeChange::Full(body)
                                }
                                Ok(body) => PeChange::Partial(body),
                                Err(error) => PeChange::Unreadable(error),
                            }
                        }
                    };
                    let resource = self.subscriptions[at].1.resource.clone();
                    actions.push(PeAction::Notification(PeNotification::Changed {
                        subscribe_id: subscribe_id.clone(),
                        resource,
                        change,
                        media_type: media_type.clone(),
                    }));
                    None
                }
                None => Some("unknown subscription id"),
            },
            PeHeader::Subscription(PeSubscription::End { subscribe_id, .. }) => {
                match known(self, subscribe_id) {
                    Some(at) => {
                        let (subscribe_id, sub) = self.subscriptions.remove(at);
                        actions.push(PeAction::Notification(PeNotification::Ended {
                            subscribe_id,
                            resource: sub.resource,
                        }));
                        None
                    }
                    None => Some("unknown subscription id"),
                }
            }
            _ => Some("not a subscription update"),
        };
        let mut reply = PeReply::new(if refusal.is_some() {
            PeStatus::BadRequest
        } else {
            PeStatus::Ok
        });
        reply.message = refusal.and_then(PeMessage::new);
        // The reply is a header and short: it fits the smallest receivable size, so
        // a split that fails would be a reply there is nothing to send.
        if let Ok(chunks) = split_message(
            received.request_id,
            &PeHeader::Reply(reply),
            &[],
            self.settings.peer_max_sysex,
        ) {
            for chunk in chunks {
                actions.push(self.send_chunk(PeMessageKind::SUBSCRIPTION_REPLY, chunk));
            }
        }
    }

    fn on_signal(
        &mut self,
        transaction: PeTransaction,
        signal: PeSignal,
        now: Instant,
        actions: &mut Vec<PeAction>,
    ) {
        let id = signal.request_id();
        let Some(at) = self
            .pending
            .iter()
            .position(|p| p.id == id && p.work.transaction() == transaction)
        else {
            return;
        };
        match signal {
            PeSignal::TimeoutWait { wait, .. } => {
                wait.extend(&mut self.pending[at].deadline);
                let kind = self.pending[at].work.reply_kind();
                self.reassembler.extend(self.peer, kind, id, wait);
            }
            PeSignal::SendNext { chunk, .. } => self.on_send_next(at, chunk.get(), now, actions),
            PeSignal::Resend { .. } => self.on_resend(at, now, actions),
            PeSignal::Terminate { by, .. } => {
                self.conclude(at, PeOutcome::Terminated(by), actions);
            }
            PeSignal::OutOfSequence { .. }
            | PeSignal::Malformed { .. }
            | PeSignal::TimedOut { .. } => self.conclude(at, PeOutcome::Nak(signal), actions),
        }
    }

    /// The ACK that asks for the next chunk of a Set under Flow Control.
    fn on_send_next(&mut self, at: usize, acked: u16, now: Instant, actions: &mut Vec<PeAction>) {
        let Work::Set {
            chunks,
            sent,
            flow: PeFlowControl::On,
            resends,
        } = &mut self.pending[at].work
        else {
            return;
        };
        // An ACK of an earlier chunk is a repeat; of the last, one nothing waits for.
        if usize::from(acked) != *sent || *sent >= chunks.len() {
            return;
        }
        let chunk = chunks[*sent].clone();
        *sent += 1;
        *resends = 0;
        self.pending[at].deadline = now + PE_TIMEOUT;
        let action = self.send_chunk(PeMessageKind::SET_INQUIRY, chunk);
        actions.push(action);
    }

    /// The NAK that asks for the most recent chunk again (§15.4).
    fn on_resend(&mut self, at: usize, now: Instant, actions: &mut Vec<PeAction>) {
        let id = self.pending[at].id;
        let Work::Set {
            chunks,
            sent,
            flow: PeFlowControl::On,
            resends,
        } = &mut self.pending[at].work
        else {
            return;
        };
        if *resends >= MAX_RESENDS {
            let signal = PeSignal::Terminate {
                request_id: id,
                by: PeTerminator::Initiator,
            };
            actions.push(self.signal_action(PeTransaction::Set, signal));
            self.conclude(at, PeOutcome::Terminated(PeTerminator::Initiator), actions);
            return;
        }
        *resends += 1;
        let chunk = chunks[*sent - 1].clone();
        self.pending[at].deadline = now + PE_TIMEOUT;
        let action = self.send_chunk(PeMessageKind::SET_INQUIRY, chunk);
        actions.push(action);
    }

    /// Ends the request at `at`: its ID is free and its reply, if half received,
    /// is dropped.
    fn conclude(&mut self, at: usize, outcome: PeOutcome, actions: &mut Vec<PeAction>) {
        let pending = self.pending.remove(at);
        self.ids.release(pending.id);
        self.reassembler
            .terminate(self.peer, pending.work.reply_kind(), pending.id);
        actions.push(PeAction::Finished {
            request_id: pending.id,
            kind: pending.work.kind(),
            outcome,
        });
    }

    fn find(&self, id: PeRequestId, reply_kind: PeMessageKind) -> Option<usize> {
        self.pending
            .iter()
            .position(|p| p.id == id && p.work.reply_kind() == reply_kind)
    }

    fn header(&self) -> CiHeader {
        CiHeader::new(self.own)
    }

    fn send_chunk(&self, kind: PeMessageKind, chunk: PeChunk) -> PeAction {
        PeAction::Send(CiMessage::PropertyExchange {
            header: self.header(),
            to: self.peer,
            message: PeCiMessage::Chunk { kind, chunk },
        })
    }

    fn signal_action(&self, transaction: PeTransaction, signal: PeSignal) -> PeAction {
        PeAction::Send(match signal.answer(transaction) {
            PeAnswer::Ack(ack) => CiMessage::Ack {
                header: self.header(),
                to: self.peer,
                ack,
            },
            PeAnswer::Nak(nak) => CiMessage::Nak {
                header: self.header(),
                to: self.peer,
                nak,
            },
        })
    }
}

#[cfg(test)]
mod tests;
