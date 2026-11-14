//! [`ResourceResponder`]: the Responder of Property Exchange, serving the
//! Resources of a [`ResourceStore`] (M2-103 §5 to §12, §14, §15).

use std::time::Instant;

use super::super::body::{BodyLimit, decode_body, encode_body};
use super::super::capabilities::{PeCapabilities, PeCiMessage, SimultaneousRequests};
use super::super::chunk::{PeChunk, PeRequestId, split_message};
use super::super::flow::{
    PE_TIMEOUT, PeAnswer, PeMessageKind, PeSignal, PeTerminator, PeTransaction,
};
use super::super::header::{
    PeEncoding, PeEndedBy, PeHeader, PeReply, PeRequest, PeSetScope, PeStatus, PeStatusClass,
    PeSubscription, PeUpdateCommand,
};
use super::super::limits::{MediaType, PeMessage, SubscribeId};
use super::super::partial::{JsonPointer, PartialError, PartialUpdate};
use super::super::reassemble::{
    PeFlowControl, PeReassembler, PeReassemblyLimits, PeReceived, PeStep,
};
use super::super::tree::PeJson;
use super::data::PeData;
use super::fields::PeDataError;
use super::kind::{CanSet, PeResource};
use super::request::ResourceTarget;
use super::store::{ResourceStore, StoreError};
use crate::{CiHeader, CiMessage, Decoded, Destination, MaxSysexSize, Muid, TimeoutWait};

/// The most resends of a chunk, whether for a NAK or for the lack of an ACK,
/// before the transaction ends (M2-103 §15.5).
const MAX_RESENDS: u8 = 3;

/// The most changes a queued `partial` update holds before it falls back to a `full`
/// of the data as it is.
const MAX_QUEUED_CHANGES: usize = 64;

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
    /// An inquiry for a Resource marked busy is held, and the Initiator was asked to
    /// wait (M2-103 §12.2.1). It is answered when the Resource is ready.
    Deferred {
        /// The Initiator.
        peer: Muid,
        /// The Resource.
        target: ResourceTarget,
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
    /// The data to change is not JSON, or the changed data is not the Resource's.
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

/// A Subscription message sent and not yet replied to.
#[derive(Debug)]
struct Outgoing {
    peer: Muid,
    id: PeRequestId,
    sub: SubscribeId,
    deadline: Instant,
}

/// What waits for a free Request ID of a peer: one item per Subscription, which
/// later changes fold into.
#[derive(Clone, Debug, PartialEq)]
enum Pending {
    /// Changes the Initiator has not been told: `partial` when they are sent.
    Partial(PartialUpdate),
    /// The data as it is when the slot frees: `full`, or `notify` when it does not
    /// fit one chunk.
    Resync,
    /// The Subscription ended.
    End,
}

#[derive(Debug)]
struct Queued {
    peer: Muid,
    sub: SubscribeId,
    pending: Pending,
}

/// A reply under Flow Control, sent a chunk at a time (M2-103 §15.2).
#[derive(Debug)]
struct Stream {
    peer: Muid,
    id: PeRequestId,
    chunks: Vec<PeChunk>,
    /// How many chunks have been sent: the last is the one waiting for its ACK.
    sent: usize,
    resends: u8,
    deadline: Instant,
}

/// An inquiry held while its Resource is busy.
#[derive(Debug)]
struct Deferred {
    received: PeReceived,
    target: ResourceTarget,
    wait: TimeoutWait,
    /// When to ask the Initiator for more time if the Resource is still busy.
    renew: Instant,
}

enum Answer {
    Reply(PeReply, Vec<u8>),
    /// A reply to send under Flow Control.
    Streamed(PeReply, Vec<u8>),
    Defer(ResourceTarget, TimeoutWait),
}

enum Announce {
    Full,
    Partial(PartialUpdate),
}

/// The Responder of Property Exchange: it serves the Resources of its store to
/// the Initiators it knows, and tells those that subscribed what changes.
///
/// Get, Set (Full and Partial) and Start and End Subscription are answered
/// with the statuses of M2-103 Table 15: 404 for a Resource or an entry the
/// store does not hold, 405 for a Get, Set or Subscribe the entry does not
/// allow, 400 for a request that breaks the rules of `resId` or pagination or
/// carries Property Data that is not the Resource's, 406 for Flow Control on
/// a Resource that does not declare it, 407 for a Set that needs it and does
/// not use it, and 415 for an encoding or a media type the entry does not list.
///
/// - **Flow Control** (§15): a Get of a Resource with `fcOnGet` is answered a chunk
///   at a time, each waiting for its ACK, with a NAK asking for a resend honoured
///   and a chunk with no ACK in 3 seconds sent again up to three times before a
///   Terminate NAK. A Set with `flowControl` is ACKed a chunk at a time.
/// - **Timeout Wait** (§12.2.1): [`mark_busy`](Self::mark_busy) says a Resource
///   cannot be answered yet; an inquiry for it is held, the Initiator is sent an
///   ACK that asks it to wait, and [`ready`](Self::ready) answers what was held. A
///   Timeout Wait ACK from an Initiator extends the time a Subscription message or
///   a Flow Control chunk waits.
/// - **Subscription messages** wait for the Initiator's count of simultaneous
///   requests (§5.3): one that cannot go is queued, per Subscription. A queued
///   `partial` takes later changes into it; when they cannot be merged, or are more
///   than 64 it becomes a `full` of the data as it is, or a
///   `notify` when that does not fit a chunk. Nothing is dropped, and the queue
///   holds one item for each Subscription.
///
/// A Responder knows an Initiator after [`accept`](Self::accept), with the
/// Receivable Maximum SysEx Message Size of its Discovery.
#[derive(Debug)]
pub struct ResourceResponder {
    own: Muid,
    settings: ResponderSettings,
    store: ResourceStore,
    peers: Vec<Peer>,
    reassembler: PeReassembler,
    subs: Vec<Served>,
    outgoing: Vec<Outgoing>,
    queue: Vec<Queued>,
    streams: Vec<Stream>,
    busy: Vec<(ResourceTarget, TimeoutWait)>,
    deferred: Vec<Deferred>,
    next_sub: u32,
    next_request: u8,
}

/// The status a request is answered with, and the explanation.
type Refusal = (PeStatus, String);

fn refusal(status: PeStatus, why: impl Into<String>) -> Refusal {
    (status, why.into())
}

/// Whether one pointer is a proper prefix of the other, by tokens, so the order of
/// two changes decides the result.
pub(super) fn nested(a: &JsonPointer, b: &JsonPointer) -> bool {
    let (a, b): (Vec<String>, Vec<String>) = (a.tokens().collect(), b.tokens().collect());
    let short = a.len().min(b.len());
    a != b && a[..short] == b[..short]
}

/// The changes `old` then `new` make, as one change, or `None` when they cannot
/// be one: a pointer inside another, or too many.
pub(super) fn merged(old: &PartialUpdate, new: &PartialUpdate) -> Option<PartialUpdate> {
    let clash = old
        .iter()
        .any(|(a, _)| new.iter().any(|(b, _)| nested(a, b)));
    if clash {
        return None;
    }
    let mut all = old.clone();
    for (pointer, value) in new.iter() {
        all = all.with(pointer.clone(), value.clone()).ok()?;
    }
    (all.iter().count() <= MAX_QUEUED_CHANGES).then_some(all)
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
            queue: Vec::new(),
            streams: Vec::new(),
            busy: Vec::new(),
            deferred: Vec::new(),
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

    /// How many Subscription messages wait for a Request ID of `peer`.
    #[must_use]
    pub fn queued(&self, peer: Muid) -> usize {
        self.queue.iter().filter(|q| q.peer == peer).count()
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
        self.queue.retain(|q| q.peer != peer);
        self.streams.retain(|s| s.peer != peer);
        self.deferred.retain(|d| d.received.peer != peer);
    }

    fn header(&self) -> CiHeader {
        CiHeader::new(self.own)
    }

    fn peer(&self, muid: Muid) -> Option<&Peer> {
        self.peers.iter().find(|p| p.muid == muid)
    }

    /// Says that `target` cannot be answered yet. An inquiry for it is held and the
    /// Initiator is asked to wait `wait` more (M2-103 §12.2.1), as often as it takes,
    /// until [`ready`](Self::ready).
    pub fn mark_busy(&mut self, target: ResourceTarget, wait: TimeoutWait) {
        match self.busy.iter_mut().find(|(t, _)| *t == target) {
            Some((_, old)) => *old = wait,
            None => self.busy.push((target, wait)),
        }
    }

    /// Says that `target` can be answered, and answers the inquiries held for it.
    pub fn ready(&mut self, target: &ResourceTarget, now: Instant) -> Vec<ResponderAction> {
        self.busy.retain(|(t, _)| t != target);
        let mut out = Vec::new();
        let (held, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.deferred)
            .into_iter()
            .partition(|d| d.target == *target);
        self.deferred = rest;
        for deferred in held {
            self.complete(deferred.received, now, &mut out);
        }
        out
    }

    /// When [`on_timeout`](Self::on_timeout) is next needed.
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        self.outgoing
            .iter()
            .map(|o| o.deadline)
            .chain(self.streams.iter().map(|s| s.deadline))
            .chain(self.deferred.iter().map(|d| d.renew))
            .chain(self.reassembler.next_deadline())
            .min()
    }

    /// Time has passed: an inquiry whose next chunk is late is dropped and NAKed, a
    /// Subscription message nobody replied to is forgotten and the queue goes on, a
    /// Flow Control chunk with no ACK is sent again or the transaction ends, and
    /// an inquiry held for a busy Resource is given more time.
    pub fn on_timeout(&mut self, now: Instant) -> Vec<ResponderAction> {
        let mut out = Vec::new();
        for expired in self.reassembler.expire(now) {
            out.push(self.signal(expired.peer, expired.kind, expired.send));
        }
        let mut freed = Vec::new();
        self.outgoing.retain(|o| {
            let live = o.deadline > now;
            if !live {
                freed.push(o.peer);
            }
            live
        });
        for peer in freed {
            self.drain(peer, now, &mut out);
        }
        self.streams_timed_out(now, &mut out);
        for at in 0..self.deferred.len() {
            if self.deferred[at].renew <= now {
                let d = &mut self.deferred[at];
                d.renew = now + d.wait.duration();
                let (peer, id, wait) = (d.received.peer, d.received.request_id, d.wait);
                let kind = d.received.kind;
                out.push(self.signal(
                    peer,
                    kind,
                    PeSignal::TimeoutWait {
                        request_id: id,
                        wait,
                        chunk: None,
                    },
                ));
            }
        }
        out
    }

    fn streams_timed_out(&mut self, now: Instant, out: &mut Vec<ResponderAction>) {
        let mut at = 0;
        while at < self.streams.len() {
            if self.streams[at].deadline > now {
                at += 1;
                continue;
            }
            if self.streams[at].resends < MAX_RESENDS {
                self.resend(at, now, out);
                at += 1;
            } else {
                let s = self.streams.remove(at);
                let signal = PeSignal::Terminate {
                    request_id: s.id,
                    by: PeTerminator::Responder,
                };
                out.push(self.signal(s.peer, PeMessageKind::GET_INQUIRY, signal));
            }
        }
    }

    fn resend(&mut self, at: usize, now: Instant, out: &mut Vec<ResponderAction>) {
        let s = &mut self.streams[at];
        s.resends += 1;
        s.deadline = now + PE_TIMEOUT;
        let (peer, chunk) = (s.peer, s.chunks[s.sent - 1].clone());
        self.send_chunks(peer, PeMessageKind::GET_REPLY, vec![chunk], out);
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
        match message {
            CiMessage::PropertyExchange { message, .. } => match message {
                PeCiMessage::CapabilitiesInquiry(capabilities) => {
                    if let Some(peer) = self.peers.iter_mut().find(|p| p.muid == source) {
                        peer.takes = capabilities.requests;
                    }
                    out.push(ResponderAction::Send(CiMessage::PropertyExchange {
                        header: self.header(),
                        to: source,
                        message: PeCiMessage::CapabilitiesReply(self.settings.capabilities),
                    }));
                    self.drain(source, now, &mut out);
                }
                PeCiMessage::Chunk { kind, chunk } => {
                    self.on_chunk(source, *kind, chunk.clone(), now, &mut out);
                }
                PeCiMessage::CapabilitiesReply(_) | PeCiMessage::Notify(_) => {}
            },
            CiMessage::Ack { ack, .. } => {
                if let Some((transaction, signal)) = PeSignal::from_ack(ack) {
                    self.on_signal(source, transaction, signal, now, &mut out);
                }
            }
            CiMessage::Nak { nak, .. } => {
                if let Some((transaction, signal)) = PeSignal::from_nak(nak) {
                    self.on_signal(source, transaction, signal, now, &mut out);
                }
            }
            _ => {}
        }
        out
    }

    /// What an Initiator says about a transaction of ours: a reply under Flow Control
    /// or a Subscription message.
    fn on_signal(
        &mut self,
        peer: Muid,
        transaction: PeTransaction,
        signal: PeSignal,
        now: Instant,
        out: &mut Vec<ResponderAction>,
    ) {
        let id = signal.request_id();
        match transaction {
            PeTransaction::Get => {
                let Some(at) = self
                    .streams
                    .iter()
                    .position(|s| s.peer == peer && s.id == id)
                else {
                    return;
                };
                match signal {
                    PeSignal::TimeoutWait { wait, .. } => {
                        wait.extend(&mut self.streams[at].deadline);
                    }
                    PeSignal::SendNext { chunk, .. } => {
                        let s = &mut self.streams[at];
                        // An ACK of an earlier chunk is a repeat.
                        if usize::from(chunk.get()) != s.sent {
                            return;
                        }
                        if s.sent >= s.chunks.len() {
                            self.streams.remove(at);
                            return;
                        }
                        let next = s.chunks[s.sent].clone();
                        s.sent += 1;
                        s.resends = 0;
                        s.deadline = now + PE_TIMEOUT;
                        self.send_chunks(peer, PeMessageKind::GET_REPLY, vec![next], out);
                    }
                    PeSignal::Resend { .. } => {
                        if self.streams[at].resends < MAX_RESENDS {
                            self.resend(at, now, out);
                        } else {
                            self.streams.remove(at);
                            let terminate = PeSignal::Terminate {
                                request_id: id,
                                by: PeTerminator::Responder,
                            };
                            out.push(self.signal(peer, PeMessageKind::GET_INQUIRY, terminate));
                        }
                    }
                    PeSignal::Terminate { .. }
                    | PeSignal::OutOfSequence { .. }
                    | PeSignal::Malformed { .. }
                    | PeSignal::TimedOut { .. } => {
                        self.streams.remove(at);
                    }
                }
            }
            PeTransaction::Subscription => {
                let Some(at) = self
                    .outgoing
                    .iter()
                    .position(|o| o.peer == peer && o.id == id)
                else {
                    return;
                };
                match signal {
                    PeSignal::TimeoutWait { wait, .. } => {
                        wait.extend(&mut self.outgoing[at].deadline);
                    }
                    PeSignal::SendNext { .. } | PeSignal::Resend { .. } => {}
                    PeSignal::Terminate { .. }
                    | PeSignal::OutOfSequence { .. }
                    | PeSignal::Malformed { .. }
                    | PeSignal::TimedOut { .. } => {
                        self.outgoing.remove(at);
                        self.drain(peer, now, out);
                    }
                }
            }
            PeTransaction::Set => {}
        }
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
        let (peer, kind, request_id) = (received.peer, received.kind, received.request_id);
        let answer = match (&received.header, kind) {
            (PeHeader::Request(request), k) if k == PeMessageKind::GET_INQUIRY => self.get(request),
            (PeHeader::Request(request), k) if k == PeMessageKind::SET_INQUIRY => {
                self.set(peer, request, &received.body, now, out)
            }
            (PeHeader::Subscription(sub), k) if k == PeMessageKind::SUBSCRIPTION_INQUIRY => {
                self.subscription(peer, sub, out)
            }
            (PeHeader::Reply(reply), k) if k == PeMessageKind::SUBSCRIPTION_REPLY => {
                self.update_replied(peer, request_id, reply, now, out);
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
        match answer {
            Ok(Answer::Reply(reply, body)) => {
                self.reply(peer, reply_kind, request_id, reply, &body, false, now, out);
            }
            Ok(Answer::Streamed(reply, body)) => {
                self.reply(peer, reply_kind, request_id, reply, &body, true, now, out);
            }
            Ok(Answer::Defer(target, wait)) => {
                out.push(ResponderAction::Event(ResponderEvent::Deferred {
                    peer,
                    target: target.clone(),
                }));
                let signal = PeSignal::TimeoutWait {
                    request_id,
                    wait,
                    chunk: None,
                };
                out.push(self.signal(peer, kind, signal));
                self.deferred.push(Deferred {
                    received,
                    target,
                    wait,
                    renew: now + wait.duration(),
                });
            }
            Err((status, why)) => {
                let mut reply = PeReply::new(status);
                reply.message = PeMessage::new(&why);
                self.reply(peer, reply_kind, request_id, reply, &[], false, now, out);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn reply(
        &mut self,
        peer: Muid,
        kind: PeMessageKind,
        id: PeRequestId,
        reply: PeReply,
        body: &[u8],
        flow: bool,
        now: Instant,
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
        let Ok(chunks) = chunks else { return };
        if flow && let Some(first) = chunks.first().cloned() {
            self.streams.retain(|s| !(s.peer == peer && s.id == id));
            self.streams.push(Stream {
                peer,
                id,
                chunks,
                sent: 1,
                resends: 0,
                deadline: now + PE_TIMEOUT,
            });
            self.send_chunks(peer, kind, vec![first], out);
        } else {
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

    fn busy_wait(&self, target: &ResourceTarget) -> Option<TimeoutWait> {
        self.busy.iter().find(|(t, _)| t == target).map(|(_, w)| *w)
    }

    /// The data a target names.
    fn data_of(&self, target: &ResourceTarget) -> Result<PeData, Refusal> {
        if *target.resource() == PeResource::ResourceList {
            return Ok(PeData::ResourceList(self.store.resource_list()));
        }
        self.store
            .get(target)
            .cloned()
            .ok_or_else(|| refusal(PeStatus::NotFound, "no data for this resId"))
    }

    fn get(&self, request: &PeRequest) -> Result<Answer, Refusal> {
        let (target, encoding) = self.resolve(request)?;
        let entry = self.store.entry(target.resource());
        if let Some(entry) = entry
            && !entry.can_get()
        {
            return Err(refusal(PeStatus::NotAllowed, "the Resource cannot be got"));
        }
        let streamed = request.flow_control == PeFlowControl::On;
        if streamed && !entry.is_some_and(|e| e.fc_on_get()) {
            return Err(refusal(
                PeStatus::FlowControlUnsupported,
                "the Resource has no Flow Control on Get",
            ));
        }
        if let Some(wait) = self.busy_wait(&target) {
            return Ok(Answer::Defer(target, wait));
        }
        let data = self.data_of(&target)?;
        let mut reply = PeReply::new(PeStatus::Ok);
        let (media_type, mut bytes) = data.to_wire();
        if let Some(page) = request.page {
            let Some(PeJson::Array(mut items)) = data.to_json() else {
                return Err(refusal(PeStatus::BadRequest, "only a list has pages"));
            };
            let total = items.len();
            let from = usize::try_from(page.offset)
                .unwrap_or(usize::MAX)
                .min(total);
            let to = from.saturating_add(page.limit.get() as usize).min(total);
            reply.total_count = u32::try_from(total).ok();
            items = items[from..to].to_vec();
            bytes = PeJson::Array(items).encode();
        }
        reply.media_type = media_type;
        data.write_header(&mut reply.extra);
        reply.mutual_encoding = (encoding != PeEncoding::Ascii).then_some(encoding);
        let body = encode_body(encoding, &bytes)
            .map_err(|e| refusal(PeStatus::UnsupportedMedia, e.to_string()))?;
        Ok(if streamed {
            Answer::Streamed(reply, body)
        } else {
            Answer::Reply(reply, body)
        })
    }

    fn set(
        &mut self,
        peer: Muid,
        request: &PeRequest,
        wire: &[u8],
        now: Instant,
        out: &mut Vec<ResponderAction>,
    ) -> Result<Answer, Refusal> {
        let (target, encoding) = self.resolve(request)?;
        let entry = self
            .store
            .entry(target.resource())
            .ok_or_else(|| refusal(PeStatus::NotAllowed, "ResourceList cannot be set"))?;
        let allowed = match (entry.can_set(), request.set_scope) {
            (CanSet::None, _) | (CanSet::Full, PeSetScope::Partial) => false,
            (CanSet::Full | CanSet::Partial, _) => true,
        };
        if !allowed {
            return Err(refusal(
                PeStatus::NotAllowed,
                "the Resource cannot be set so",
            ));
        }
        let flow = request.flow_control == PeFlowControl::On;
        if flow && !entry.fc_on_set() {
            return Err(refusal(
                PeStatus::FlowControlUnsupported,
                "the Resource has no Flow Control on Set",
            ));
        }
        if !flow && entry.fc_on_set() {
            return Err(refusal(
                PeStatus::FlowControlRequired,
                "the Resource needs Flow Control on Set",
            ));
        }
        let json_type = MediaType::new("application/json");
        let media_type = request.media_type.as_ref().or(json_type.as_ref());
        if let Some(media_type) = media_type
            && !entry.media_types().contains(media_type)
        {
            return Err(refusal(PeStatus::UnsupportedMedia, "media type not listed"));
        }
        if let Some(wait) = self.busy_wait(&target) {
            return Ok(Answer::Defer(target, wait));
        }
        let bytes = decode_body(encoding, wire, self.settings.body_limit)
            .map_err(|e| refusal(PeStatus::BadRequest, e.to_string()))?;
        let (data, announce, how) = match request.set_scope {
            PeSetScope::Whole => {
                let data =
                    PeData::decode_bytes(target.resource(), request.media_type.as_ref(), &bytes)
                        .map_err(|e| refusal(PeStatus::BadRequest, e.to_string()))?;
                (data, Announce::Full, ChangeKind::Full)
            }
            PeSetScope::Partial => {
                let json = PeJson::parse(&bytes)
                    .map_err(|e| refusal(PeStatus::BadRequest, e.to_string()))?;
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
        self.announce(&target, announce, now, out);
        Ok(Answer::Reply(PeReply::new(PeStatus::Ok), Vec::new()))
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
        let json = held.to_json().ok_or_else(|| {
            PeDataError::new(
                target.resource().as_str(),
                super::fields::PeDataFault::NotValid("JSON data to change"),
            )
        })?;
        let json = update.apply(&json)?;
        Ok(PeData::decode(target.resource(), &json)?)
    }

    fn subscription(
        &mut self,
        peer: Muid,
        subscription: &PeSubscription,
        out: &mut Vec<ResponderAction>,
    ) -> Result<Answer, Refusal> {
        match subscription {
            PeSubscription::Start(request) => {
                if request.page.is_some() {
                    return Err(refusal(
                        PeStatus::BadRequest,
                        "a Subscription takes no page",
                    ));
                }
                let (target, encoding) = self.resolve(request)?;
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
                if let Some(wait) = self.busy_wait(&target) {
                    return Ok(Answer::Defer(target, wait));
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
                Ok(Answer::Reply(reply, Vec::new()))
            }
            PeSubscription::End { subscribe_id, .. } => {
                let at = self
                    .subs
                    .iter()
                    .position(|s| s.id == *subscribe_id && s.peer == peer)
                    .ok_or_else(|| refusal(PeStatus::NotFound, "no such subscription"))?;
                self.subs.remove(at);
                self.queue
                    .retain(|q| !(q.peer == peer && q.sub == *subscribe_id));
                out.push(ResponderAction::Event(ResponderEvent::Unsubscribed {
                    peer,
                    subscribe_id: subscribe_id.clone(),
                    by: PeEndedBy::Initiator,
                }));
                Ok(Answer::Reply(PeReply::new(PeStatus::Ok), Vec::new()))
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
        self.drain(peer, now, out);
    }

    /// Ends a Subscription, sending the End to its Initiator.
    pub fn end_subscription(&mut self, id: &SubscribeId, now: Instant) -> Vec<ResponderAction> {
        let mut out = Vec::new();
        let Some(at) = self.subs.iter().position(|s| s.id == *id) else {
            return out;
        };
        let sub = self.subs.remove(at);
        self.enqueue(sub.peer, &sub.id, Pending::End);
        self.drain(sub.peer, now, &mut out);
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
        let mut out = Vec::new();
        self.announce(target, Announce::Full, now, &mut out);
        Ok(out)
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
        let mut out = Vec::new();
        self.announce(target, Announce::Partial(update), now, &mut out);
        Ok(out)
    }

    /// Queues what the subscribers of `target` are to be told, and sends what the
    /// peers' counts of requests allow.
    fn announce(
        &mut self,
        target: &ResourceTarget,
        what: Announce,
        now: Instant,
        out: &mut Vec<ResponderAction>,
    ) {
        let subs: Vec<(SubscribeId, Muid)> = self
            .subs
            .iter()
            .filter(|s| s.target == *target)
            .map(|s| (s.id.clone(), s.peer))
            .collect();
        for (id, peer) in subs {
            let pending = match &what {
                Announce::Full => Pending::Resync,
                Announce::Partial(update) => Pending::Partial(update.clone()),
            };
            self.enqueue(peer, &id, pending);
            self.drain(peer, now, out);
        }
    }

    /// Adds `new` to what waits for the Subscription `sub`: it takes the place of
    /// older items, or folds into a queued `partial` (M2-103 §11.1.1).
    fn enqueue(&mut self, peer: Muid, sub: &SubscribeId, new: Pending) {
        let Some(at) = self
            .queue
            .iter()
            .position(|q| q.peer == peer && q.sub == *sub)
        else {
            self.queue.push(Queued {
                peer,
                sub: sub.clone(),
                pending: new,
            });
            return;
        };
        let held = &mut self.queue[at].pending;
        *held = match (&*held, new) {
            (Pending::End, _) => Pending::End,
            (_, Pending::End) => Pending::End,
            (_, Pending::Resync) | (Pending::Resync, Pending::Partial(_)) => Pending::Resync,
            (Pending::Partial(old), Pending::Partial(new)) => {
                merged(old, &new).map_or(Pending::Resync, Pending::Partial)
            }
        };
    }

    /// Sends the queued Subscription messages of `peer` that its count of
    /// simultaneous requests has a place for, oldest first.
    fn drain(&mut self, peer: Muid, now: Instant, out: &mut Vec<ResponderAction>) {
        loop {
            let Some(takes) = self.peer(peer).map(|p| usize::from(p.takes.get())) else {
                return;
            };
            if self.outgoing.iter().filter(|o| o.peer == peer).count() >= takes {
                return;
            }
            let Some(at) = self.queue.iter().position(|q| q.peer == peer) else {
                return;
            };
            let Queued { sub, pending, .. } = self.queue.remove(at);
            self.send_pending(peer, &sub, pending, now, out);
        }
    }

    fn send_pending(
        &mut self,
        peer: Muid,
        sub: &SubscribeId,
        pending: Pending,
        now: Instant,
        out: &mut Vec<ResponderAction>,
    ) {
        let update = |command, encoding: PeEncoding, media_type| {
            PeHeader::Subscription(PeSubscription::Update {
                command,
                subscribe_id: sub.clone(),
                mutual_encoding: (encoding != PeEncoding::Ascii).then_some(encoding),
                media_type,
                extra: Default::default(),
            })
        };
        let notify = || update(PeUpdateCommand::Notify, PeEncoding::Ascii, None);
        if pending == Pending::End {
            let header = PeHeader::Subscription(PeSubscription::End {
                subscribe_id: sub.clone(),
                ended_by: Some(PeEndedBy::Responder),
                extra: Default::default(),
            });
            self.send_update(peer, sub, &header, &[], now, out);
            return;
        }
        let Some((target, encoding)) = self
            .subs
            .iter()
            .find(|s| s.id == *sub && s.peer == peer)
            .map(|s| (s.target.clone(), s.encoding))
        else {
            return;
        };
        let max = self.peer(peer).map(|p| p.max_sysex);
        let fits = |header: &PeHeader, body: &[u8]| {
            max.is_some_and(|max| {
                split_message(PeRequestId::of(0), header, body, max)
                    .is_ok_and(|chunks| chunks.len() == 1)
            })
        };
        // The message, and the body it carries: a `partial` as it was queued, a
        // `full` of the data as it is, and a `notify` when neither fits a chunk.
        let mut message: Option<(PeHeader, Vec<u8>)> = None;
        if let Pending::Partial(changes) = &pending {
            let header = update(PeUpdateCommand::Partial, encoding, None);
            if let Ok(body) =
                encode_body(encoding, &PeJson::Object(changes_object(changes)).encode())
                && fits(&header, &body)
            {
                message = Some((header, body));
            }
        }
        if message.is_none()
            && let Some(data) = self.store.get(&target)
        {
            let (media_type, bytes) = data.to_wire();
            let header = update(PeUpdateCommand::Full, encoding, media_type);
            if let Ok(body) = encode_body(encoding, &bytes)
                && fits(&header, &body)
            {
                message = Some((header, body));
            }
        }
        let (header, body) = message.unwrap_or_else(|| (notify(), Vec::new()));
        self.send_update(peer, sub, &header, &body, now, out);
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
        let Some(max) = self.peer(peer).map(|p| p.max_sysex) else {
            return;
        };
        let free = (0..128u8)
            .map(|step| (self.next_request + step) % 128)
            .find(|n| {
                !self
                    .outgoing
                    .iter()
                    .any(|o| o.peer == peer && o.id.get() == *n)
            })
            .and_then(PeRequestId::new);
        let Some(id) = free else { return };
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

/// The Property Data of a `partial` update: the pointers and their values.
fn changes_object(update: &PartialUpdate) -> super::super::tree::PeObject {
    match update.to_json() {
        PeJson::Object(object) => object,
        _ => super::super::tree::PeObject::new(),
    }
}
