//! [`ResourceResponder`]: the Responder of Property Exchange, serving the
//! Resources of a [`ResourceStore`] (M2-103 §5 to §11, §14).

use std::time::Instant;

use super::super::body::{BodyLimit, decode_body, encode_body};
use super::super::capabilities::{PeCapabilities, PeCiMessage, SimultaneousRequests};
use super::super::chunk::{PeChunk, PeRequestId, split_message};
use super::super::flow::{PE_TIMEOUT, PeAnswer, PeMessageKind, PeSignal};
use super::super::header::{
    PeEncoding, PeEndedBy, PeHeader, PeReply, PeRequest, PeSetScope, PeStatus, PeStatusClass,
    PeSubscription, PeUpdateCommand,
};
use super::super::limits::{PeMessage, SubscribeId};
use super::super::partial::{PartialError, PartialUpdate};
use super::super::reassemble::{
    PeFlowControl, PeReassembler, PeReassemblyLimits, PeReceived, PeStep,
};
use super::super::tree::PeJson;
use super::data::PeData;
use super::fields::PeDataError;
use super::kind::{CanSet, PeResource};
use super::request::ResourceTarget;
use super::store::{ResourceStore, StoreError};
use crate::{CiHeader, CiMessage, Decoded, Destination, MaxSysexSize, Muid};

/// What a Responder holds, fixed for the device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResponderSettings {
    /// What this device reports in the Capabilities Reply: how many requests of
    /// the peer's it takes at once (M2-103 §5.3).
    pub capabilities: PeCapabilities,
    /// What the inquiries being received may buffer.
    pub reassembly: PeReassemblyLimits,
    /// The most a decoded body may be.
    pub body_limit: BodyLimit,
}

/// How the data of a Resource was changed by an Initiator.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChangeKind {
    /// A Full Set replaced it.
    Full,
    /// A Partial Set changed a part of it.
    Partial,
}

/// What the Responder tells its caller happened.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ResponderEvent {
    /// An Initiator's Set changed the data, which the store holds now.
    Changed {
        /// The Initiator.
        peer: Muid,
        /// The Resource.
        target: ResourceTarget,
        /// How.
        how: ChangeKind,
    },
    /// A Subscription started.
    Subscribed {
        /// The Initiator.
        peer: Muid,
        /// The Subscription.
        subscribe_id: SubscribeId,
        /// The Resource.
        target: ResourceTarget,
    },
    /// A Subscription ended.
    Unsubscribed {
        /// The Initiator.
        peer: Muid,
        /// The Subscription.
        subscribe_id: SubscribeId,
        /// Which device ended it.
        by: PeEndedBy,
    },
    /// A Subscription message could not be sent because the Initiator's count of
    /// simultaneous requests are all in use (M2-103 §5.3). The Initiator's data
    /// is out of date until the next update.
    UpdateNotSent {
        /// The Initiator.
        peer: Muid,
        /// The Subscription.
        subscribe_id: SubscribeId,
    },
}

/// What the driver does next.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum ResponderAction {
    /// Send this message to the peer.
    Send(CiMessage),
    /// Something happened.
    Event(ResponderEvent),
}

/// Data that cannot be published to the store and its Subscriptions.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum PublishError {
    /// The store refuses the data.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The data belongs to another Resource than the target.
    #[error("the data is of {data} and the target is {target}")]
    DataOfAnotherResource {
        /// The Resource of the data.
        data: String,
        /// The target.
        target: String,
    },
    /// The store holds no data to change.
    #[error("{0} has no data to change")]
    NoData(String),
    /// A change cannot be made to the data.
    #[error(transparent)]
    Change(#[from] PartialError),
    /// The changed data is not the Resource's.
    #[error(transparent)]
    Data(#[from] PeDataError),
}

#[derive(Debug)]
struct Peer {
    muid: Muid,
    max_sysex: MaxSysexSize,
    /// How many Subscription messages the peer takes at once.
    takes: SimultaneousRequests,
}

#[derive(Debug)]
struct Served {
    id: SubscribeId,
    peer: Muid,
    target: ResourceTarget,
    encoding: PeEncoding,
}

#[derive(Debug)]
struct Outgoing {
    peer: Muid,
    id: PeRequestId,
    sub: SubscribeId,
    deadline: Instant,
}

enum Announce {
    Full(PeData),
    Partial(PartialUpdate),
}

/// The Responder of Property Exchange: it serves the Resources of its store to
/// the Initiators it knows, and tells those that subscribed what changes.
///
/// Get, Set (Full and Partial) and Start and End Subscription are answered
/// with the statuses of M2-103 Table 15: 404 for a Resource or an entry the
/// store does not hold, 405 for a Get, Set or Subscribe the entry does not
/// allow, 400 for a request that breaks the rules of `resId` or pagination or
/// carries Property Data that is not the Resource's, 406 for Flow Control
/// (which this Responder does not do) and 415 for an encoding the entry does not
/// list. A Responder knows an Initiator after [`accept`](Self::accept), with
/// the Receivable Maximum SysEx Message Size of its Discovery.
#[derive(Debug)]
pub struct ResourceResponder {
    own: Muid,
    settings: ResponderSettings,
    store: ResourceStore,
    peers: Vec<Peer>,
    reassembler: PeReassembler,
    subs: Vec<Served>,
    outgoing: Vec<Outgoing>,
    next_sub: u32,
    next_request: u8,
}

/// The status a request is answered with, and the explanation.
type Refusal = (PeStatus, String);

fn refusal(status: PeStatus, why: impl Into<String>) -> Refusal {
    (status, why.into())
}

impl ResourceResponder {
    /// A Responder with MUID `own` serving `store`.
    #[must_use]
    pub fn new(own: Muid, settings: ResponderSettings, store: ResourceStore) -> Self {
        Self {
            own,
            settings,
            store,
            peers: Vec::new(),
            reassembler: PeReassembler::new(settings.reassembly),
            subs: Vec::new(),
            outgoing: Vec::new(),
            next_sub: 1,
            next_request: 0,
        }
    }

    /// The Resources served.
    #[must_use]
    pub fn store(&self) -> &ResourceStore {
        &self.store
    }

    /// The Subscriptions that are active: the Subscription, its Initiator and
    /// its Resource.
    pub fn subscriptions(&self) -> impl Iterator<Item = (&SubscribeId, Muid, &ResourceTarget)> {
        self.subs.iter().map(|s| (&s.id, s.peer, &s.target))
    }

    /// Serves `peer`, whose Discovery gave `max_sysex`.
    pub fn accept(&mut self, peer: Muid, max_sysex: MaxSysexSize) {
        self.forget(peer);
        self.peers.push(Peer {
            muid: peer,
            max_sysex,
            takes: SimultaneousRequests::ONE,
        });
    }

    /// Stops serving `peer` and drops its Subscriptions without a message: an
    /// Invalidate MUID ends them (M2-103 §11.5).
    pub fn forget(&mut self, peer: Muid) {
        self.peers.retain(|p| p.muid != peer);
        self.subs.retain(|s| s.peer != peer);
        self.outgoing.retain(|o| o.peer != peer);
    }

    fn header(&self) -> CiHeader {
        CiHeader::new(self.own)
    }

    fn peer(&self, muid: Muid) -> Option<&Peer> {
        self.peers.iter().find(|p| p.muid == muid)
    }

    /// When [`on_timeout`](Self::on_timeout) is next needed.
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        self.outgoing
            .iter()
            .map(|o| o.deadline)
            .chain(self.reassembler.next_deadline())
            .min()
    }

    /// Time has passed: an inquiry whose next chunk is late is dropped and NAKed,
    /// and a Subscription message nobody replied to is forgotten.
    pub fn on_timeout(&mut self, now: Instant) -> Vec<ResponderAction> {
        let mut out = Vec::new();
        self.outgoing.retain(|o| o.deadline > now);
        for expired in self.reassembler.expire(now) {
            out.push(self.signal(expired.peer, expired.kind, expired.send));
        }
        out
    }

    /// A CI message of the connection's Group arrived.
    pub fn on_message(&mut self, decoded: &Decoded, now: Instant) -> Vec<ResponderAction> {
        let mut out = Vec::new();
        let Decoded::Message(message) = decoded else {
            return out;
        };
        let source = message.header().source;
        if message.destination() != Destination::Muid(self.own) || self.peer(source).is_none() {
            return out;
        }
        let CiMessage::PropertyExchange { message, .. } = message else {
            return out;
        };
        match message {
            PeCiMessage::CapabilitiesInquiry(capabilities) => {
                if let Some(peer) = self.peers.iter_mut().find(|p| p.muid == source) {
                    peer.takes = capabilities.requests;
                }
                out.push(ResponderAction::Send(CiMessage::PropertyExchange {
                    header: self.header(),
                    to: source,
                    message: PeCiMessage::CapabilitiesReply(self.settings.capabilities),
                }));
            }
            PeCiMessage::Chunk { kind, chunk } => {
                self.on_chunk(source, *kind, chunk.clone(), now, &mut out);
            }
            PeCiMessage::CapabilitiesReply(_) | PeCiMessage::Notify(_) => {}
        }
        out
    }

    fn on_chunk(
        &mut self,
        peer: Muid,
        kind: PeMessageKind,
        chunk: PeChunk,
        now: Instant,
        out: &mut Vec<ResponderAction>,
    ) {
        let inquiry = [
            PeMessageKind::GET_INQUIRY,
            PeMessageKind::SET_INQUIRY,
            PeMessageKind::SUBSCRIPTION_INQUIRY,
            PeMessageKind::SUBSCRIPTION_REPLY,
        ];
        if !inquiry.contains(&kind) {
            return;
        }
        match self
            .reassembler
            .push(peer, kind, chunk, PeFlowControl::Off, now)
        {
            Ok(PeStep::Pending { send }) => {
                out.extend(send.map(|s| self.signal(peer, kind, s)));
            }
            Ok(PeStep::Complete { message, send }) => {
                out.extend(send.map(|s| self.signal(peer, kind, s)));
                self.complete(*message, now, out);
            }
            Ok(PeStep::Abandoned) => {}
            Err(refused) => out.extend(refused.send.map(|s| self.signal(peer, kind, s))),
        }
    }

    fn signal(&self, peer: Muid, kind: PeMessageKind, signal: PeSignal) -> ResponderAction {
        ResponderAction::Send(match signal.answer(kind.transaction) {
            PeAnswer::Ack(ack) => CiMessage::Ack {
                header: self.header(),
                to: peer,
                ack,
            },
            PeAnswer::Nak(nak) => CiMessage::Nak {
                header: self.header(),
                to: peer,
                nak,
            },
        })
    }

    fn complete(&mut self, received: PeReceived, now: Instant, out: &mut Vec<ResponderAction>) {
        let PeReceived {
            peer,
            kind,
            request_id,
            header,
            body,
        } = received;
        let result = match (kind, header) {
            (k, PeHeader::Request(request)) if k == PeMessageKind::GET_INQUIRY => {
                self.get(&request)
            }
            (k, PeHeader::Request(request)) if k == PeMessageKind::SET_INQUIRY => {
                self.set(peer, &request, &body, now, out)
            }
            (k, PeHeader::Subscription(sub)) if k == PeMessageKind::SUBSCRIPTION_INQUIRY => {
                self.subscription(peer, sub, out)
            }
            (k, PeHeader::Reply(reply)) if k == PeMessageKind::SUBSCRIPTION_REPLY => {
                self.update_replied(peer, request_id, &reply, now, out);
                return;
            }
            _ => Err(refusal(
                PeStatus::BadRequest,
                "not the header of this kind of message",
            )),
        };
        let reply_kind = PeMessageKind {
            transaction: kind.transaction,
            reply: true,
        };
        let (reply, body) = match result {
            Ok(answer) => answer,
            Err((status, why)) => {
                let mut reply = PeReply::new(status);
                reply.message = PeMessage::new(&why);
                (reply, Vec::new())
            }
        };
        self.reply(peer, reply_kind, request_id, reply, &body, out);
    }

    fn reply(
        &self,
        peer: Muid,
        kind: PeMessageKind,
        id: PeRequestId,
        reply: PeReply,
        body: &[u8],
        out: &mut Vec<ResponderAction>,
    ) {
        let Some(max) = self.peer(peer).map(|p| p.max_sysex) else {
            return;
        };
        let chunks = split_message(id, &PeHeader::Reply(reply), body, max).or_else(|_| {
            split_message(
                id,
                &PeHeader::Reply(PeReply::new(PeStatus::PayloadTooLarge)),
                &[],
                max,
            )
        });
        if let Ok(chunks) = chunks {
            self.send_chunks(peer, kind, chunks, out);
        }
    }

    fn send_chunks(
        &self,
        peer: Muid,
        kind: PeMessageKind,
        chunks: Vec<PeChunk>,
        out: &mut Vec<ResponderAction>,
    ) {
        for chunk in chunks {
            out.push(ResponderAction::Send(CiMessage::PropertyExchange {
                header: self.header(),
                to: peer,
                message: PeCiMessage::Chunk { kind, chunk },
            }));
        }
    }

    /// The Resource an inquiry names, checked against the store: its target and
    /// its encoding.
    fn resolve(&self, request: &PeRequest) -> Result<(ResourceTarget, PeEncoding), Refusal> {
        let resource = PeResource::from_name(&request.resource);
        let encoding = request.mutual_encoding.unwrap_or(PeEncoding::Ascii);
        let target = |resource: PeResource| {
            match request.res_id.clone() {
                Some(id) => ResourceTarget::entry(resource, id),
                None => ResourceTarget::new(resource),
            }
            .map_err(|e| refusal(PeStatus::BadRequest, e.to_string()))
        };
        if resource == PeResource::ResourceList {
            if request.res_id.is_some() || request.page.is_some() {
                return Err(refusal(
                    PeStatus::BadRequest,
                    "ResourceList takes no resId and no page",
                ));
            }
            return Ok((target(resource)?, encoding));
        }
        let entry = self
            .store
            .entry(&resource)
            .ok_or_else(|| refusal(PeStatus::NotFound, "no such Resource"))?;
        if entry.require_res_id() != request.res_id.is_some() {
            return Err(refusal(
                PeStatus::BadRequest,
                format!("requireResId is {}", entry.require_res_id()),
            ));
        }
        if !entry.encodings().contains(&encoding) {
            return Err(refusal(PeStatus::UnsupportedMedia, "encoding not listed"));
        }
        if request.page.is_some() && !entry.can_paginate() {
            return Err(refusal(
                PeStatus::BadRequest,
                "the Resource does not paginate",
            ));
        }
        Ok((target(resource)?, encoding))
    }

    /// The data a target names, as JSON.
    fn data_of(&self, target: &ResourceTarget) -> Result<PeData, Refusal> {
        if *target.resource() == PeResource::ResourceList {
            return Ok(PeData::ResourceList(self.store.resource_list()));
        }
        self.store
            .get(target)
            .cloned()
            .ok_or_else(|| refusal(PeStatus::NotFound, "no data for this resId"))
    }

    fn get(&self, request: &PeRequest) -> Result<(PeReply, Vec<u8>), Refusal> {
        if request.flow_control == PeFlowControl::On {
            return Err(refusal(PeStatus::FlowControlUnsupported, "no Flow Control"));
        }
        let (target, encoding) = self.resolve(request)?;
        if let Some(entry) = self.store.entry(target.resource())
            && !entry.can_get()
        {
            return Err(refusal(PeStatus::NotAllowed, "the Resource cannot be got"));
        }
        let mut json = self.data_of(&target)?.to_json();
        let mut reply = PeReply::new(PeStatus::Ok);
        if let Some(page) = request.page
            && let PeJson::Array(items) = &mut json
        {
            let total = items.len();
            let from = usize::try_from(page.offset)
                .unwrap_or(usize::MAX)
                .min(total);
            let to = from.saturating_add(page.limit.get() as usize).min(total);
            reply.total_count = u32::try_from(total).ok();
            *items = items[from..to].to_vec();
        }
        reply.mutual_encoding = (encoding != PeEncoding::Ascii).then_some(encoding);
        let body = encode_body(encoding, &json.encode())
            .map_err(|e| refusal(PeStatus::InternalError, e.to_string()))?;
        Ok((reply, body))
    }

    fn set(
        &mut self,
        peer: Muid,
        request: &PeRequest,
        wire: &[u8],
        now: Instant,
        out: &mut Vec<ResponderAction>,
    ) -> Result<(PeReply, Vec<u8>), Refusal> {
        let (target, encoding) = self.resolve(request)?;
        let entry = self
            .store
            .entry(target.resource())
            .ok_or_else(|| refusal(PeStatus::NotAllowed, "ResourceList cannot be set"))?;
        let allowed = match (entry.can_set(), request.set_scope) {
            (CanSet::None, _) => false,
            (CanSet::Full, PeSetScope::Partial) => false,
            (CanSet::Full | CanSet::Partial, _) => true,
        };
        if !allowed {
            return Err(refusal(
                PeStatus::NotAllowed,
                "the Resource cannot be set so",
            ));
        }
        let bytes = decode_body(encoding, wire, self.settings.body_limit)
            .map_err(|e| refusal(PeStatus::BadRequest, e.to_string()))?;
        let json =
            PeJson::parse(&bytes).map_err(|e| refusal(PeStatus::BadRequest, e.to_string()))?;
        let (data, announce, how) = match request.set_scope {
            PeSetScope::Whole => {
                let data = PeData::decode(target.resource(), &json)
                    .map_err(|e| refusal(PeStatus::BadRequest, e.to_string()))?;
                (data.clone(), Announce::Full(data), ChangeKind::Full)
            }
            PeSetScope::Partial => {
                let update = PartialUpdate::from_json(&json)
                    .map_err(|e| refusal(PeStatus::BadRequest, e.to_string()))?;
                let data = self.changed(&target, &update).map_err(|e| match e {
                    PublishError::NoData(_) => refusal(PeStatus::NotFound, e.to_string()),
                    other => refusal(PeStatus::BadRequest, other.to_string()),
                })?;
                (data, Announce::Partial(update), ChangeKind::Partial)
            }
        };
        self.store
            .put(target.res_id().cloned(), data)
            .map_err(|e| refusal(PeStatus::InternalError, e.to_string()))?;
        out.push(ResponderAction::Event(ResponderEvent::Changed {
            peer,
            target: target.clone(),
            how,
        }));
        out.extend(self.announce(&target, &announce, now));
        Ok((PeReply::new(PeStatus::Ok), Vec::new()))
    }

    /// The data of `target` with `update` made.
    fn changed(
        &self,
        target: &ResourceTarget,
        update: &PartialUpdate,
    ) -> Result<PeData, PublishError> {
        let held = self
            .store
            .get(target)
            .ok_or_else(|| PublishError::NoData(target.resource().as_str().to_owned()))?;
        let json = update.apply(&held.to_json())?;
        Ok(PeData::decode(target.resource(), &json)?)
    }

    fn subscription(
        &mut self,
        peer: Muid,
        subscription: PeSubscription,
        out: &mut Vec<ResponderAction>,
    ) -> Result<(PeReply, Vec<u8>), Refusal> {
        match subscription {
            PeSubscription::Start(request) => {
                if request.page.is_some() {
                    return Err(refusal(
                        PeStatus::BadRequest,
                        "a Subscription takes no page",
                    ));
                }
                let (target, encoding) = self.resolve(&request)?;
                let subscribable = self
                    .store
                    .entry(target.resource())
                    .is_some_and(|e| e.can_subscribe());
                if !subscribable {
                    return Err(refusal(
                        PeStatus::NotAllowed,
                        "the Resource is not subscribable",
                    ));
                }
                self.data_of(&target)?;
                let id = self.new_subscribe_id();
                self.subs.push(Served {
                    id: id.clone(),
                    peer,
                    target: target.clone(),
                    encoding,
                });
                out.push(ResponderAction::Event(ResponderEvent::Subscribed {
                    peer,
                    subscribe_id: id.clone(),
                    target,
                }));
                let mut reply = PeReply::new(PeStatus::Ok);
                reply.subscribe_id = Some(id);
                Ok((reply, Vec::new()))
            }
            PeSubscription::End { subscribe_id, .. } => {
                let at = self
                    .subs
                    .iter()
                    .position(|s| s.id == subscribe_id && s.peer == peer)
                    .ok_or_else(|| refusal(PeStatus::NotFound, "no such subscription"))?;
                self.subs.remove(at);
                out.push(ResponderAction::Event(ResponderEvent::Unsubscribed {
                    peer,
                    subscribe_id,
                    by: PeEndedBy::Initiator,
                }));
                Ok((PeReply::new(PeStatus::Ok), Vec::new()))
            }
            PeSubscription::Update { .. } => Err(refusal(
                PeStatus::BadRequest,
                "only a Responder sends an update",
            )),
        }
    }

    fn new_subscribe_id(&mut self) -> SubscribeId {
        loop {
            let text = format!("s{}", self.next_sub % 10_000_000);
            self.next_sub = self.next_sub.wrapping_add(1);
            if let Some(id) = SubscribeId::new(&text)
                && self.subs.iter().all(|s| s.id != id)
            {
                return id;
            }
        }
    }

    /// A reply to a Subscription message of ours.
    fn update_replied(
        &mut self,
        peer: Muid,
        id: PeRequestId,
        reply: &PeReply,
        now: Instant,
        out: &mut Vec<ResponderAction>,
    ) {
        let Some(at) = self
            .outgoing
            .iter()
            .position(|o| o.peer == peer && o.id == id)
        else {
            return;
        };
        let sent = self.outgoing.remove(at);
        // A failure ends the Subscription, which the spec lets the sender do in
        // place of a retry (M2-103 §11.2).
        if matches!(
            reply.status.class(),
            PeStatusClass::ClientError | PeStatusClass::ServerError
        ) {
            out.extend(self.end_subscription(&sent.sub, now));
        }
    }

    /// Ends a Subscription, sending the End to its Initiator.
    pub fn end_subscription(&mut self, id: &SubscribeId, now: Instant) -> Vec<ResponderAction> {
        let mut out = Vec::new();
        let Some(at) = self.subs.iter().position(|s| s.id == *id) else {
            return out;
        };
        let sub = self.subs.remove(at);
        let header = PeHeader::Subscription(PeSubscription::End {
            subscribe_id: sub.id.clone(),
            ended_by: Some(PeEndedBy::Responder),
            extra: Default::default(),
        });
        self.send_update(sub.peer, &sub.id, &header, &[], now, &mut out);
        out.push(ResponderAction::Event(ResponderEvent::Unsubscribed {
            peer: sub.peer,
            subscribe_id: sub.id,
            by: PeEndedBy::Responder,
        }));
        out
    }

    /// Replaces the data of a Resource and tells its subscribers, with `full`
    /// when it fits one chunk and `notify` when it does not (M2-103 §11.1.1).
    ///
    /// # Errors
    ///
    /// [`PublishError`] when the store refuses the data or it is not the
    /// target's Resource.
    pub fn publish(
        &mut self,
        target: &ResourceTarget,
        data: PeData,
        now: Instant,
    ) -> Result<Vec<ResponderAction>, PublishError> {
        if data.resource() != *target.resource() {
            return Err(PublishError::DataOfAnotherResource {
                data: data.resource().as_str().to_owned(),
                target: target.resource().as_str().to_owned(),
            });
        }
        self.store.put(target.res_id().cloned(), data.clone())?;
        Ok(self.announce(target, &Announce::Full(data), now))
    }

    /// Changes a part of the data of a Resource and tells its subscribers, with
    /// `partial` when it fits one chunk and `notify` when it does not.
    ///
    /// # Errors
    ///
    /// [`PublishError`] when there is no data, the change cannot be made or the
    /// changed data is not the Resource's.
    pub fn change(
        &mut self,
        target: &ResourceTarget,
        update: PartialUpdate,
        now: Instant,
    ) -> Result<Vec<ResponderAction>, PublishError> {
        let data = self.changed(target, &update)?;
        self.store.put(target.res_id().cloned(), data)?;
        Ok(self.announce(target, &Announce::Partial(update), now))
    }

    fn announce(
        &mut self,
        target: &ResourceTarget,
        what: &Announce,
        now: Instant,
    ) -> Vec<ResponderAction> {
        let mut out = Vec::new();
        let subs: Vec<(SubscribeId, Muid, PeEncoding)> = self
            .subs
            .iter()
            .filter(|s| s.target == *target)
            .map(|s| (s.id.clone(), s.peer, s.encoding))
            .collect();
        for (id, peer, encoding) in subs {
            let (command, json) = match what {
                Announce::Full(data) => (PeUpdateCommand::Full, data.to_json()),
                Announce::Partial(update) => (PeUpdateCommand::Partial, update.to_json()),
            };
            let header = |command, encoding: Option<PeEncoding>| {
                PeHeader::Subscription(PeSubscription::Update {
                    command,
                    subscribe_id: id.clone(),
                    mutual_encoding: encoding,
                    media_type: None,
                    extra: Default::default(),
                })
            };
            let wire_encoding = (encoding != PeEncoding::Ascii).then_some(encoding);
            let body = encode_body(encoding, &json.encode()).unwrap_or_default();
            let max = self.peer(peer).map(|p| p.max_sysex);
            let fits = max.is_some_and(|max| {
                split_message(
                    PeRequestId::of(0),
                    &header(command, wire_encoding),
                    &body,
                    max,
                )
                .is_ok_and(|chunks| chunks.len() == 1)
            });
            if fits {
                self.send_update(
                    peer,
                    &id,
                    &header(command, wire_encoding),
                    &body,
                    now,
                    &mut out,
                );
            } else {
                // The data spans chunks: the Initiator is asked to Get it.
                self.send_update(
                    peer,
                    &id,
                    &header(PeUpdateCommand::Notify, None),
                    &[],
                    now,
                    &mut out,
                );
            }
        }
        out
    }

    /// Sends a Subscription message to `peer` under a Request ID not in use.
    fn send_update(
        &mut self,
        peer: Muid,
        sub: &SubscribeId,
        header: &PeHeader,
        body: &[u8],
        now: Instant,
        out: &mut Vec<ResponderAction>,
    ) {
        let Some((max, takes)) = self.peer(peer).map(|p| (p.max_sysex, p.takes)) else {
            return;
        };
        let in_use = self.outgoing.iter().filter(|o| o.peer == peer).count();
        let free = (0..128u8)
            .map(|step| (self.next_request + step) % 128)
            .find(|n| {
                !self
                    .outgoing
                    .iter()
                    .any(|o| o.peer == peer && o.id.get() == *n)
            });
        let id = match free.and_then(PeRequestId::new) {
            Some(id) if in_use < usize::from(takes.get()) => id,
            _ => {
                out.push(ResponderAction::Event(ResponderEvent::UpdateNotSent {
                    peer,
                    subscribe_id: sub.clone(),
                }));
                return;
            }
        };
        let Ok(chunks) = split_message(id, header, body, max) else {
            return;
        };
        self.next_request = (id.get() + 1) % 128;
        self.outgoing.push(Outgoing {
            peer,
            id,
            sub: sub.clone(),
            deadline: now + PE_TIMEOUT,
        });
        self.send_chunks(peer, PeMessageKind::SUBSCRIPTION_INQUIRY, chunks, out);
    }
}
