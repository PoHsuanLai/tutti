//! What a Responder does beyond a plain reply, and what carries more than JSON:
//! the queue of Subscription messages, Flow Control, Timeout Wait, media types
//! and Link actions. Each test names the property of the spec it holds.

use std::time::Duration;

use super::super::capabilities::PeCiMessage;
use super::super::chunk::{PeChunk, PeRequestId, split_message};
use super::super::flow::{
    PE_TIMEOUT, PeAnswer, PeMessageKind, PeSignal, PeTerminator, PeTransaction,
};
use super::super::header::{
    PeEncoding, PeEndedBy, PeHeader, PeReply, PeSetScope, PeStatus, PeSubscription, PeUpdateCommand,
};
use super::super::initiator::{PeCall, PeSet};
use super::super::limits::{MediaType, ResourceName};
use super::super::partial::{JsonPointer, PartialUpdate};
use super::super::reassemble::PeFlowControl;
use super::super::tree::{PeJson, PeObject};
use super::super::value::{PeExtra, PeExtraError};
use super::exchange::*;
use super::*;
use crate::ci::testing::{OURS, PEER};
use crate::{CiHeader, CiMessage, Decoded, MaxSysexSize, Muid, TimeoutWait};

fn from_initiator(message: CiMessage) -> Decoded {
    Decoded::Message(message)
}

/// A message from the Initiator to the Responder.
fn pe_message(message: PeCiMessage) -> Decoded {
    from_initiator(CiMessage::PropertyExchange {
        header: CiHeader::new(Muid::of(OURS)),
        to: Muid::of(PEER),
        message,
    })
}

/// A signal of the Initiator about a transaction of the Responder's.
fn signal_message(transaction: PeTransaction, signal: PeSignal) -> Decoded {
    let header = CiHeader::new(Muid::of(OURS));
    let to = Muid::of(PEER);
    from_initiator(match signal.answer(transaction) {
        PeAnswer::Ack(ack) => CiMessage::Ack { header, to, ack },
        PeAnswer::Nak(nak) => CiMessage::Nak { header, to, nak },
    })
}

/// An inquiry from the Initiator, all its chunks, and what the Responder did.
fn inquire(
    link: &mut Link,
    kind: PeMessageKind,
    id: u8,
    header: PeHeader,
    body: &[u8],
) -> Vec<ResponderAction> {
    let chunks = split_message(PeRequestId::of(id), &header, body, MaxSysexSize::of(512)).unwrap();
    let mut out = Vec::new();
    for chunk in chunks {
        out.extend(
            link.resp
                .on_message(&pe_message(PeCiMessage::Chunk { kind, chunk }), link.now),
        );
    }
    out
}

/// A reply of the Initiator to a Subscription message of the Responder's.
fn reply_to_update(link: &mut Link, id: PeRequestId, status: PeStatus) -> Vec<ResponderAction> {
    let chunks = split_message(
        id,
        &PeHeader::Reply(PeReply::new(status)),
        &[],
        MaxSysexSize::of(256),
    )
    .unwrap();
    let mut out = Vec::new();
    for chunk in chunks {
        out.extend(link.resp.on_message(
            &pe_message(PeCiMessage::Chunk {
                kind: PeMessageKind::SUBSCRIPTION_REPLY,
                chunk,
            }),
            link.now,
        ));
    }
    out
}

fn chunks_of(actions: &[ResponderAction]) -> Vec<(PeMessageKind, PeChunk)> {
    actions
        .iter()
        .filter_map(|a| match a {
            ResponderAction::Send(CiMessage::PropertyExchange {
                message: PeCiMessage::Chunk { kind, chunk },
                ..
            }) => Some((*kind, chunk.clone())),
            _ => None,
        })
        .collect()
}

fn header_of(chunk: &PeChunk) -> PeHeader {
    PeHeader::parse(chunk.header()).unwrap()
}

fn acks_and_naks(actions: &[ResponderAction]) -> Vec<PeSignal> {
    actions
        .iter()
        .filter_map(|a| match a {
            ResponderAction::Send(CiMessage::Ack { ack, .. }) => {
                PeSignal::from_ack(ack).map(|(_, s)| s)
            }
            ResponderAction::Send(CiMessage::Nak { nak, .. }) => {
                PeSignal::from_nak(nak).map(|(_, s)| s)
            }
            _ => None,
        })
        .collect()
}

/// The Subscription messages among the actions: header and Property Data.
fn updates_of(actions: &[ResponderAction]) -> Vec<(PeSubscription, Vec<u8>)> {
    chunks_of(actions)
        .into_iter()
        .filter(|(kind, _)| *kind == PeMessageKind::SUBSCRIPTION_INQUIRY)
        .filter_map(|(_, chunk)| match header_of(&chunk) {
            PeHeader::Subscription(s) => Some((s, chunk.data().to_vec())),
            _ => None,
        })
        .collect()
}

fn channels(n: u16) -> PeData {
    PeData::ChannelList(PeChannelList::new(vec![channel(n, "Changing")]))
}

fn subscribed_link(
    requests: u8,
    resource: PeResource,
    res_id: Option<&str>,
) -> (Link, ResourceTarget) {
    let mut link = Link::taking(store(), 256, requests);
    let encoding = if resource == custom("X-Blob") {
        PeEncoding::Mcoded7
    } else {
        PeEncoding::Ascii
    };
    let target = match res_id {
        Some(i) => entry_target(resource, i),
        None => super::exchange::target(resource),
    };
    let made = link.init.subscribe(
        &ResourceGet::new(target.clone()).encoded(encoding),
        link.now,
    );
    link.call(made);
    assert_eq!(link.resp.subscriptions().count(), 1);
    (link, target)
}

fn request_id(actions: &[ResponderAction]) -> PeRequestId {
    chunks_of(actions)[0].1.request_id()
}

// M2-103 §5.3, §11.1: a Subscription message waits for a Request ID when the
// Initiator takes one at a time, goes when the reply to the one before arrives,
// and carries the data as it is then.
#[test]
fn an_update_waits_for_a_free_request_id_and_carries_the_data_as_it_is_then() {
    let (mut link, target) = subscribed_link(1, PeResource::ChannelList, None);
    let first = link.resp.publish(&target, channels(1), link.now).unwrap();
    assert_eq!(chunks_of(&first).len(), 1);
    let second = link.resp.publish(&target, channels(2), link.now).unwrap();
    let third = link.resp.publish(&target, channels(3), link.now).unwrap();
    assert!(second.is_empty() && third.is_empty());
    assert_eq!(link.resp.queued(Muid::of(OURS)), 1);
    let sent = reply_to_update(&mut link, request_id(&first), PeStatus::Ok);
    let updates = updates_of(&sent);
    assert_eq!(updates.len(), 1);
    let (PeSubscription::Update { command, .. }, body) = &updates[0] else {
        panic!("not an update");
    };
    assert_eq!(*command, PeUpdateCommand::Full);
    assert_eq!(
        PeJson::parse(body).unwrap(),
        channels(3).to_json().unwrap(),
        "the newest data goes, and the two before it with it"
    );
    assert_eq!(link.resp.queued(Muid::of(OURS)), 0);
}

// M2-103 §5.3: the queue never grows past one item for a Subscription, and nothing is
// dropped.
#[test]
fn the_queue_holds_one_item_for_each_subscription() {
    let (mut link, target) = subscribed_link(1, PeResource::ChannelList, None);
    for n in 1..=20 {
        link.resp.publish(&target, channels(n), link.now).unwrap();
    }
    assert_eq!(link.resp.queued(Muid::of(OURS)), 1);
}

fn pointer(text: &str) -> JsonPointer {
    JsonPointer::new(text).unwrap()
}

// M2-103 §11.1: `partial` updates use the format of a Partial Set; two that wait
// together are one, the later value of a pointer winning.
#[test]
fn queued_partial_updates_fold_into_one() {
    let (mut link, target) = subscribed_link(1, custom("X-ProgramEdit"), Some("abcd"));
    let change = |p: &str, n: i64| {
        PartialUpdate::new()
            .with(pointer(p), PeJson::Integer(n))
            .unwrap()
    };
    let first = link
        .resp
        .change(&target, change("/lfoSpeed", 10), link.now)
        .unwrap();
    link.resp
        .change(&target, change("/pitchEnvelope/rates/0", 80), link.now)
        .unwrap();
    link.resp
        .change(&target, change("/pitchEnvelope/levels/0", 60), link.now)
        .unwrap();
    link.resp
        .change(&target, change("/pitchEnvelope/rates/0", 81), link.now)
        .unwrap();
    assert_eq!(link.resp.queued(Muid::of(OURS)), 1);
    let sent = reply_to_update(&mut link, request_id(&first), PeStatus::Ok);
    let updates = updates_of(&sent);
    let (PeSubscription::Update { command, .. }, body) = &updates[0] else {
        panic!("not an update");
    };
    assert_eq!(*command, PeUpdateCommand::Partial);
    assert_eq!(
        PeJson::parse(body).unwrap(),
        PeJson::parse(br#"{"/pitchEnvelope/rates/0":81,"/pitchEnvelope/levels/0":60}"#).unwrap()
    );
}

// M2-103 §11.1: a `partial` that cannot be one change, a pointer inside another or
// too many, is told as a `full` of the data as it is.
#[test]
fn partial_updates_that_cannot_fold_are_told_as_a_full() {
    let one = |p: &str| {
        PartialUpdate::new()
            .with(pointer(p), PeJson::Integer(1))
            .unwrap()
    };
    assert!(super::server::merged(&one("/a"), &one("/b")).is_some());
    assert!(super::server::merged(&one("/a"), &one("/a")).is_some());
    assert!(super::server::merged(&one(""), &one("/a")).is_none());
    assert!(super::server::merged(&one("/a/b"), &one("/a")).is_none());
    let (mut link, target) = subscribed_link(1, custom("X-ProgramEdit"), Some("abcd"));
    let first = link
        .resp
        .change(&target, one("/lfoSpeed"), link.now)
        .unwrap();
    for n in 0..70 {
        link.resp
            .change(&target, one(&format!("/p{n}")), link.now)
            .unwrap();
    }
    let sent = reply_to_update(&mut link, request_id(&first), PeStatus::Ok);
    let updates = updates_of(&sent);
    let (PeSubscription::Update { command, .. }, body) = &updates[0] else {
        panic!("not an update");
    };
    // The data as it is: a `full` when it fits one chunk, a `notify` when not.
    match command {
        PeUpdateCommand::Full => {
            let held = link.resp.store().get(&target).unwrap().to_json().unwrap();
            assert_eq!(PeJson::parse(body).unwrap(), held);
        }
        other => assert_eq!(*other, PeUpdateCommand::Notify),
    }
    assert_ne!(*command, PeUpdateCommand::Partial);
}

// M2-103 §11.5: the End of a Subscription is told even when updates wait; it takes
// their place.
#[test]
fn an_end_goes_after_the_replies_and_replaces_queued_updates() {
    let (mut link, target) = subscribed_link(1, PeResource::ChannelList, None);
    let first = link.resp.publish(&target, channels(1), link.now).unwrap();
    link.resp.publish(&target, channels(2), link.now).unwrap();
    let id = link.resp.subscriptions().next().unwrap().0.clone();
    let ended = link.resp.end_subscription(&id, link.now);
    assert!(chunks_of(&ended).is_empty(), "no Request ID is free");
    assert_eq!(link.resp.queued(Muid::of(OURS)), 1);
    let sent = reply_to_update(&mut link, request_id(&first), PeStatus::Ok);
    let updates = updates_of(&sent);
    assert!(matches!(
        updates.as_slice(),
        [(
            PeSubscription::End {
                ended_by: Some(PeEndedBy::Responder),
                ..
            },
            _
        )]
    ));
}

// M2-103 §12.2: a Subscription message nobody replied to is given up after the
// timeout, which frees its Request ID for what waits.
#[test]
fn a_timeout_frees_the_request_id_for_the_queue() {
    let (mut link, target) = subscribed_link(1, PeResource::ChannelList, None);
    link.resp.publish(&target, channels(1), link.now).unwrap();
    link.resp.publish(&target, channels(2), link.now).unwrap();
    assert!(link.resp.deadline().is_some());
    let later = link.now + PE_TIMEOUT + Duration::from_millis(1);
    let sent = link.resp.on_timeout(later);
    assert_eq!(updates_of(&sent).len(), 1);
    assert_eq!(link.resp.queued(Muid::of(OURS)), 0);
}

// M2-103 §12.2.1: a Timeout Wait ACK for a Subscription message adds its period to
// the time the Responder waits for the reply.
#[test]
fn a_timeout_wait_extends_the_wait_for_a_reply_to_an_update() {
    let (mut link, target) = subscribed_link(1, PeResource::ChannelList, None);
    let sent = link.resp.publish(&target, channels(1), link.now).unwrap();
    let before = link.resp.deadline().unwrap();
    let signal = PeSignal::TimeoutWait {
        request_id: request_id(&sent),
        wait: TimeoutWait::of(30),
        chunk: None,
    };
    link.resp.on_message(
        &signal_message(PeTransaction::Subscription, signal),
        link.now,
    );
    assert_eq!(
        link.resp.deadline().unwrap(),
        before + Duration::from_secs(3)
    );
}

// M2-103 §11.2: a Terminate NAK of a Subscription message frees its Request ID.
#[test]
fn a_terminate_of_an_update_frees_its_request_id() {
    let (mut link, target) = subscribed_link(1, PeResource::ChannelList, None);
    let sent = link.resp.publish(&target, channels(1), link.now).unwrap();
    link.resp.publish(&target, channels(2), link.now).unwrap();
    let signal = PeSignal::Terminate {
        request_id: request_id(&sent),
        by: PeTerminator::Initiator,
    };
    let after = link.resp.on_message(
        &signal_message(PeTransaction::Subscription, signal),
        link.now,
    );
    assert_eq!(updates_of(&after).len(), 1);
}

fn flow_get(resource: &str) -> PeHeader {
    let mut request = super::super::header::PeRequest::new(ResourceName::new(resource).unwrap());
    request.flow_control = PeFlowControl::On;
    PeHeader::Request(request)
}

/// A link whose ChannelList is long enough for several chunks.
fn long_channel_link() -> Link {
    let mut link = Link::new(store(), 128);
    let many: Vec<PeChannelEntry> = (1..=12)
        .map(|n| channel(n, &format!("Channel number {n}")))
        .collect();
    link.resp
        .publish(
            &super::exchange::target(PeResource::ChannelList),
            PeData::ChannelList(PeChannelList::new(many)),
            link.now,
        )
        .unwrap();
    link
}

// M2-103 §15.2: under Flow Control the Responder sends a chunk, waits for the ACK that
// asks for the next, and the last chunk needs its ACK too.
#[test]
fn a_flow_controlled_reply_goes_a_chunk_for_each_ack() {
    let mut link = long_channel_link();
    let mark = link.wire.len();
    let made = link
        .init
        .get(&get(PeResource::ChannelList).with_flow_control(), link.now);
    let call = link.call(made);
    let Ok(PeData::ChannelList(list)) = link.got(call) else {
        panic!("{:?}", link.events);
    };
    assert_eq!(list.entries().len(), 12);
    // After the inquiry the wire alternates: a chunk, its ACK, a chunk, its ACK.
    let flow: Vec<bool> = link.wire[mark + 1..]
        .iter()
        .map(|(to_resp, _)| *to_resp)
        .collect();
    assert!(flow.len() >= 6, "several chunks: {flow:?}");
    for (at, to_resp) in flow.iter().enumerate() {
        assert_eq!(*to_resp, at % 2 == 1, "{flow:?}");
    }
    assert_eq!(flow.last(), Some(&true), "the last chunk is ACKed");
}

/// The first chunk of a flow-controlled Get of the long ChannelList, and the link.
fn started_stream() -> (Link, PeChunk) {
    let mut link = long_channel_link();
    let sent = inquire(
        &mut link,
        PeMessageKind::GET_INQUIRY,
        9,
        flow_get("ChannelList"),
        &[],
    );
    let chunks = chunks_of(&sent);
    assert_eq!(chunks.len(), 1, "one chunk until it is ACKed");
    (link, chunks[0].1.clone())
}

// M2-103 §15.4: a NAK that asks for the last chunk again gets it again, to three times;
// a fourth ends the transaction (§15.5).
#[test]
fn a_resend_nak_gets_the_last_chunk_again() {
    let (mut link, first) = started_stream();
    let resend = || {
        signal_message(
            PeTransaction::Get,
            PeSignal::Resend {
                request_id: PeRequestId::of(9),
                last_good: None,
            },
        )
    };
    for _ in 0..3 {
        let again = link.resp.on_message(&resend(), link.now);
        let chunks = chunks_of(&again);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].1, first);
    }
    let end = link.resp.on_message(&resend(), link.now);
    assert!(chunks_of(&end).is_empty());
    assert!(matches!(
        acks_and_naks(&end).as_slice(),
        [PeSignal::Terminate {
            by: PeTerminator::Responder,
            ..
        }]
    ));
    assert!(link.resp.deadline().is_none());
}

// M2-103 §15.5: a chunk with no ACK in 3 seconds is sent again; after three repeats the
// Responder fails with a Terminate NAK.
#[test]
fn a_chunk_with_no_ack_is_sent_again_three_times_and_then_terminated() {
    let (mut link, first) = started_stream();
    let mut now = link.now;
    for _ in 0..3 {
        now += PE_TIMEOUT + Duration::from_millis(1);
        let again = link.resp.on_timeout(now);
        let chunks = chunks_of(&again);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].1, first);
    }
    now += PE_TIMEOUT + Duration::from_millis(1);
    let end = link.resp.on_timeout(now);
    assert!(matches!(
        acks_and_naks(&end).as_slice(),
        [PeSignal::Terminate {
            by: PeTerminator::Responder,
            ..
        }]
    ));
    assert!(link.resp.deadline().is_none());
}

// M2-103 §15.1: an ACK asks for the chunk after the one it names; an ACK of an
// earlier chunk is a repeat and sends nothing.
#[test]
fn an_ack_sends_the_next_chunk_and_a_repeat_sends_nothing() {
    let (mut link, _) = started_stream();
    let ack = |n: u16| {
        signal_message(
            PeTransaction::Get,
            PeSignal::SendNext {
                request_id: PeRequestId::of(9),
                chunk: super::super::chunk::ChunkNo::of(n),
            },
        )
    };
    let second = link.resp.on_message(&ack(1), link.now);
    let chunks = chunks_of(&second);
    assert_eq!(chunks.len(), 1);
    assert_eq!(
        chunks[0].1.position(),
        super::super::chunk::ChunkPosition::Number(super::super::chunk::ChunkNo::of(2))
    );
    assert!(chunks_of(&link.resp.on_message(&ack(1), link.now)).is_empty());
}

// M2-103 §15.3: a Set under Flow Control is ACKed a chunk at a time except the
// last, which the reply to the Set acknowledges.
#[test]
fn a_set_under_flow_control_is_acked_a_chunk_at_a_time() {
    let mut link = Link::new(store(), 192);
    let bytes = blob_bytes(400);
    let target = super::exchange::target(custom("X-BigBlob"));
    let set = ResourceSet::full(target.clone(), blob_data("X-BigBlob", &bytes))
        .unwrap()
        .encoded(PeEncoding::Mcoded7);
    let mark = link.wire.len();
    let made = link.init.set(&set, link.now);
    let call = link.call(made);
    assert_eq!(link.set_result(call), Ok(PeStatus::Ok));
    assert_eq!(
        link.resp.store().get(&target),
        Some(&blob_data("X-BigBlob", &bytes))
    );
    let from_resp = link.wire[mark..].iter().filter(|(to, _)| !*to).count();
    let to_resp = link.wire[mark..].iter().filter(|(to, _)| *to).count();
    // One ACK for each chunk but the last, and the reply.
    assert_eq!(
        to_resp, from_resp,
        "chunks sent: {to_resp}, answers {from_resp}"
    );
    assert!(to_resp >= 4);
}

// M2-103 §15.3, Table 15: a Set that does not use Flow Control on a Resource with
// `fcOnSet` is 407, and one that does on a Resource without it is 406.
#[test]
fn a_set_answers_407_without_flow_control_and_406_with_it_unasked() {
    let mut raw = Raw::new();
    let body = blob_bytes(20);
    let set = |name: &str, flow: PeFlowControl| {
        let mut set = PeSet::new(ResourceName::new(name).unwrap(), body.clone());
        set.media_type = Some(octets());
        set.encoding = PeEncoding::Mcoded7;
        set.flow_control = flow;
        PeCall::Set(set)
    };
    assert_eq!(
        raw.status(set("X-BigBlob", PeFlowControl::Off)),
        PeStatus::FlowControlRequired
    );
    assert_eq!(
        raw.status(set("X-Blob", PeFlowControl::On)),
        PeStatus::FlowControlUnsupported
    );
    assert_eq!(raw.status(set("X-Blob", PeFlowControl::Off)), PeStatus::Ok);
    assert_eq!(
        raw.status(set("X-BigBlob", PeFlowControl::On)),
        PeStatus::Ok
    );
}

// M2-103 §12.2.1: a Responder that cannot answer in time sends an ACK that asks the
// Initiator to wait, and answers when it can.
#[test]
fn a_busy_resource_is_answered_with_a_timeout_wait_and_then_the_reply() {
    let mut link = Link::new(store(), 256);
    let device = super::exchange::target(PeResource::DeviceInfo);
    link.resp.mark_busy(device.clone(), TimeoutWait::of(20));
    let start = link.now;
    let made = link.init.get(&get(PeResource::DeviceInfo), link.now);
    let call = link.call(made);
    assert!(
        !link
            .events
            .iter()
            .any(|e| matches!(e, ResourceEvent::Got { call: c, .. } if *c == call)),
        "no reply yet"
    );
    assert!(
        link.served
            .iter()
            .any(|e| matches!(e, ResponderEvent::Deferred { .. }))
    );
    // The Initiator's wait is the 3 seconds and the 2 the ACK asked for.
    assert_eq!(
        link.init.deadline(),
        Some(start + PE_TIMEOUT + Duration::from_secs(2))
    );
    let sent = link.resp.ready(&device, link.now);
    link.publish(sent);
    assert_eq!(link.got(call), Ok(PeData::DeviceInfo(device_info())));
}

// M2-103 §12.2.1: while the Resource stays busy the Responder asks for more time
// before the Initiator's wait is over.
#[test]
fn a_responder_asks_again_while_the_resource_stays_busy() {
    let mut link = Link::new(store(), 256);
    let device = super::exchange::target(PeResource::DeviceInfo);
    link.resp.mark_busy(device, TimeoutWait::of(20));
    let made = link.init.get(&get(PeResource::DeviceInfo), link.now);
    link.call(made);
    let again = link.resp.on_timeout(link.now + Duration::from_secs(2));
    assert!(matches!(
        acks_and_naks(&again).as_slice(),
        [PeSignal::TimeoutWait { wait, .. }] if wait.tenths() == 20
    ));
    // A Resource that is not busy is answered at once.
    let other = link.init.get(&get(PeResource::LocalOn), link.now);
    let call = link.call(other);
    assert!(link.got(call).is_ok());
}

// M2-103 §12.2.1: a Set for a busy Resource waits too, and is made when it is ready.
#[test]
fn a_set_for_a_busy_resource_is_held_until_it_is_ready() {
    let mut link = Link::new(store(), 256);
    let local_on = super::exchange::target(PeResource::LocalOn);
    link.resp.mark_busy(local_on.clone(), TimeoutWait::of(10));
    let set = ResourceSet::full(local_on.clone(), PeData::LocalOn(PeLocalOn(true))).unwrap();
    let made = link.init.set(&set, link.now);
    let call = link.call(made);
    assert_eq!(
        link.resp.store().get(&local_on),
        Some(&PeData::LocalOn(PeLocalOn(false)))
    );
    let sent = link.resp.ready(&local_on, link.now);
    link.publish(sent);
    assert_eq!(link.set_result(call), Ok(PeStatus::Ok));
    assert_eq!(
        link.resp.store().get(&local_on),
        Some(&PeData::LocalOn(PeLocalOn(true)))
    );
}

fn reply_header(link: &Link, to_initiator: bool) -> Vec<super::super::header::PeReply> {
    link.wire
        .iter()
        .filter(|(to, _)| *to != to_initiator)
        .filter_map(|(_, m)| match m {
            CiMessage::PropertyExchange {
                message: PeCiMessage::Chunk { chunk, .. },
                ..
            } => match PeHeader::parse(chunk.header()) {
                Ok(PeHeader::Reply(r)) => Some(r),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

// M2-103 §6.1.5, §7.5: Property Data that is not JSON comes with its `mediaType` and
// reaches the caller as bytes with that type.
#[test]
fn binary_property_data_reaches_the_caller_with_its_media_type() {
    let mut link = Link::new(store(), 256);
    let blob = super::exchange::target(custom("X-Blob"));
    let made = link.init.get(
        &ResourceGet::new(blob).encoded(PeEncoding::Mcoded7),
        link.now,
    );
    let call = link.call(made);
    let Ok(PeData::Opaque(opaque)) = link.got(call) else {
        panic!("{:?}", link.events);
    };
    let OpaqueBody::Binary { media_type, bytes } = opaque.body() else {
        panic!("not binary");
    };
    assert_eq!(*media_type, octets());
    assert_eq!(*bytes, blob_bytes(300));
    let replies = reply_header(&link, true);
    assert!(replies.iter().any(|r| r.media_type == Some(octets())));
}

// M2-103 §7.5: a Set of binary data names its media type in the Header Data.
#[test]
fn a_set_of_binary_data_carries_its_media_type() {
    let mut link = Link::new(store(), 256);
    let blob = super::exchange::target(custom("X-Blob"));
    let new = blob_bytes(90);
    let set = ResourceSet::full(blob.clone(), blob_data("X-Blob", &new))
        .unwrap()
        .encoded(PeEncoding::Mcoded7);
    let made = link.init.set(&set, link.now);
    let call = link.call(made);
    assert_eq!(link.set_result(call), Ok(PeStatus::Ok));
    assert_eq!(
        link.resp.store().get(&blob),
        Some(&blob_data("X-Blob", &new))
    );
    let sets: Vec<_> = link
        .wire
        .iter()
        .filter(|(to, _)| *to)
        .filter_map(|(_, m)| match m {
            CiMessage::PropertyExchange {
                message: PeCiMessage::Chunk { kind, chunk },
                ..
            } if *kind == PeMessageKind::SET_INQUIRY && !chunk.header().is_empty() => {
                Some(header_of(chunk))
            }
            _ => None,
        })
        .collect();
    let [PeHeader::Request(request)] = sets.as_slice() else {
        panic!("{sets:?}");
    };
    assert_eq!(request.media_type, Some(octets()));
    assert_eq!(request.set_scope, PeSetScope::Whole);
}

// M2-103 §14.2: a media type the entry does not list is refused, by the Initiator
// before it is sent and by the Responder with 415.
#[test]
fn a_media_type_the_entry_does_not_list_is_refused() {
    let mut link = Link::new(store(), 256);
    // JSON data for a Resource whose entry lists only octet-stream.
    let blob = super::exchange::target(custom("X-Blob"));
    let json_data = PeData::decode(&custom("X-Blob"), &json(r#"{"a":1}"#)).unwrap();
    let set = ResourceSet::full(blob, json_data)
        .unwrap()
        .encoded(PeEncoding::Mcoded7);
    assert!(matches!(
        link.init.set(&set, link.now),
        Err(CallError::Refused(
            RequestRefused::MediaTypeNotDeclared { .. }
        ))
    ));
    // The Responder: text/plain, and JSON by omission.
    let mut raw = Raw::new();
    let mut plain = PeSet::new(ResourceName::new("X-Blob").unwrap(), b"hello".to_vec());
    plain.media_type = MediaType::new("text/plain");
    plain.encoding = PeEncoding::Mcoded7;
    assert_eq!(raw.status(PeCall::Set(plain)), PeStatus::UnsupportedMedia);
    let mut jsonish = PeSet::new(ResourceName::new("X-Blob").unwrap(), b"{}".to_vec());
    jsonish.encoding = PeEncoding::Mcoded7;
    assert_eq!(raw.status(PeCall::Set(jsonish)), PeStatus::UnsupportedMedia);
}

// M2-103 §7.5: a `full` update of binary data carries the media type, and the
// Initiator holds the bytes.
#[test]
fn a_full_update_of_binary_data_carries_its_media_type() {
    let (mut link, target) = subscribed_link(4, custom("X-Blob"), None);
    let new = blob_bytes(40);
    let actions = link
        .resp
        .publish(&target, blob_data("X-Blob", &new), link.now)
        .unwrap();
    let updates = updates_of(&actions);
    let [(PeSubscription::Update { media_type, .. }, _)] = updates.as_slice() else {
        panic!("{updates:?}");
    };
    assert_eq!(*media_type, Some(octets()));
    link.publish(actions);
    let (_, held, how) = link.updates().pop().unwrap();
    assert_eq!(how, UpdateKind::Full);
    assert_eq!(held, blob_data("X-Blob", &new));
}

// M2-103 §6.1.5: only a Resource without a type holds data that is not JSON; and
// binary data is not of the JSON media type.
#[test]
fn only_an_untyped_resource_holds_binary_data() {
    assert!(PeOpaque::binary(PeResource::LocalOn, octets(), vec![1]).is_none());
    assert!(
        PeOpaque::binary(
            custom("X-A"),
            MediaType::new("application/json").unwrap(),
            vec![]
        )
        .is_none()
    );
    assert!(PeOpaque::binary(custom("X-A"), octets(), vec![1, 255]).is_some());
    assert!(PeData::decode_bytes(&PeResource::LocalOn, Some(&octets()), b"x").is_err());
    let json_bytes = PeData::decode_bytes(&PeResource::LocalOn, None, b"true").unwrap();
    assert_eq!(json_bytes, PeData::LocalOn(PeLocalOn(true)));
}

// M2-103 §10: a Partial Set is for JSON; binary data has no pointers.
#[test]
fn a_partial_change_does_not_apply_to_binary_data() {
    let mut link = Link::new(store(), 256);
    let blob = super::exchange::target(custom("X-Blob"));
    let update = PartialUpdate::new()
        .with(pointer("/a"), PeJson::Integer(1))
        .unwrap();
    assert!(matches!(
        link.resp.change(&blob, update, link.now),
        Err(PublishError::Data(_))
    ));
}

// M2-103 §13.1.1 Table 69: a ProgramList entry with `simpleAction` Links, each with
// its `propData`.
#[test]
fn a_simple_action_link_reads_table_69() {
    let text = r#"[{"bankPC":[0,1,1],"title":"Piano","links":[
        {"resource":"X-Copy","propData":"0_1_1","role":"simpleAction","title":"copy"},
        {"resource":"X-Paste","propData":"0_1_1","role":"simpleAction","title":"paste",
         "confirmMsg":"Paste over Piano?","confirmType":"yesno","confirmIcon":"warning"}]}]"#;
    let data = PeData::decode(&PeResource::ProgramList, &json(text)).unwrap();
    let PeData::ProgramList(list) = &data else {
        panic!("not ProgramList");
    };
    let links = &list.entries()[0].links;
    let Some(LinkRole::Simple(copy)) = &links[0].role else {
        panic!("not simple");
    };
    assert_eq!(copy.prop_data, PeJson::String("0_1_1".to_owned()));
    assert!(copy.confirmation.is_none());
    let Some(LinkRole::Simple(paste)) = &links[1].role else {
        panic!("not simple");
    };
    let confirmation = paste.confirmation.as_ref().unwrap();
    assert_eq!(confirmation.kind, Some(ConfirmType::YesNo));
    assert_eq!(confirmation.icon, Some(ConfirmIcon::Warning));
    assert_eq!(data.to_json(), Some(json(text)));
}

// M2-103 §13.1.1 Table 70: the button Sets the Resource of the Link with the `propData`.
#[test]
fn a_simple_action_is_a_set_of_its_prop_data() {
    let link = PeLink {
        role: Some(LinkRole::Simple(SimpleAction {
            prop_data: PeJson::String("0_1_1".to_owned()),
            confirmation: None,
        })),
        ..PeLink::new(custom("X-Copy"))
    };
    let set = ResourceSet::from_simple_action(&link).unwrap();
    assert_eq!(set.target().resource(), &custom("X-Copy"));
    let SetBody::Full(data) = set.body() else {
        panic!("not a full set");
    };
    assert_eq!(data.to_wire(), (None, br#""0_1_1""#.to_vec()));
    // A Link that is no simple action is not one.
    assert!(matches!(
        ResourceSet::from_simple_action(&PeLink::new(custom("X-Copy"))),
        Err(ActionError::NotThatAction(_))
    ));
}

// M2-103 §13.1.1 Table 68: `propData` is required of a `simpleAction`; the question
// of a confirmation needs its message.
#[test]
fn a_simple_action_requires_its_properties() {
    let entry = |link: &str| format!(r#"[{{"title":"T","channel":1,"links":[{link}]}}]"#);
    let good = r#"{"resource":"X-A","role":"simpleAction","propData":1}"#;
    assert!(PeData::decode(&PeResource::ChannelList, &json(&entry(good))).is_ok());
    for bad in [
        r#"{"resource":"X-A","role":"simpleAction"}"#,
        r#"{"resource":"X-A","role":"simpleAction","propData":1,"confirmType":"yesno"}"#,
        r#"{"resource":"X-A","role":"simpleAction","propData":1,"confirmMsg":"?","confirmType":"maybe"}"#,
        r#"{"resource":"X-A","role":"simpleAction","propData":1,"confirmMsg":"?","confirmIcon":"star"}"#,
    ] {
        assert!(
            PeData::decode(&PeResource::ChannelList, &json(&entry(bad))).is_err(),
            "{bad}"
        );
    }
}

fn selector_channels() -> &'static str {
    r#"[{"channel":1,"bankPC":[0,1,120],"title":"Piano","links":[
        {"resource":"X-AudioOutList","resId":"ch1","role":"selectorAction","title":"Stereo Main",
         "selectedProperty":"output","actionResource":"X-Output","actionResId":"ch1"}]}]"#
}

// M2-103 §13.1.2 Table 73: a ChannelList with a `selectorAction` Link.
#[test]
fn a_selector_action_link_reads_table_73() {
    let data = PeData::decode(&PeResource::ChannelList, &json(selector_channels())).unwrap();
    let PeData::ChannelList(list) = &data else {
        panic!("not ChannelList");
    };
    let link = &list.entries()[0].links[0];
    let Some(LinkRole::Selector(action)) = &link.role else {
        panic!("not a selector");
    };
    assert_eq!(link.resource, custom("X-AudioOutList"));
    assert_eq!(action.action_resource, custom("X-Output"));
    assert_eq!(action.action_res_id, Some(id("ch1")));
    assert_eq!(action.selected_property.as_str(), "output");
    assert_eq!(data.to_json(), Some(json(selector_channels())));
}

// M2-103 §13.1.2 Table 76: the chosen entry's `selectedProperty` is the Property
// Data of a Set of the action Resource.
#[test]
fn a_selection_is_a_set_of_the_selected_property() {
    let data = PeData::decode(&PeResource::ChannelList, &json(selector_channels())).unwrap();
    let PeData::ChannelList(list) = data else {
        panic!("not ChannelList");
    };
    let link = &list.entries()[0].links[0];
    let chosen: PeObject = match json(r#"{"output":"LA","name":"Left Alt","selected":false}"#) {
        PeJson::Object(o) => o,
        _ => panic!("not an object"),
    };
    let set = ResourceSet::from_selection(link, &chosen).unwrap();
    assert_eq!(set.target().resource(), &custom("X-Output"));
    assert_eq!(set.target().res_id(), Some(&id("ch1")));
    let SetBody::Full(data) = set.body() else {
        panic!("not a full set");
    };
    assert_eq!(data.to_wire(), (None, br#""LA""#.to_vec()));
    let lacking: PeObject = match json(r#"{"name":"Left Alt"}"#) {
        PeJson::Object(o) => o,
        _ => panic!("not an object"),
    };
    assert!(matches!(
        ResourceSet::from_selection(link, &lacking),
        Err(ActionError::NoSelectedProperty(_))
    ));
}

// M2-103 Table 72: `actionResource` and `selectedProperty` are required of a
// `selectorAction`.
#[test]
fn a_selector_action_requires_its_properties() {
    let entry = |link: &str| format!(r#"[{{"title":"T","channel":1,"links":[{link}]}}]"#);
    for bad in [
        r#"{"resource":"X-L","role":"selectorAction","selectedProperty":"output"}"#,
        r#"{"resource":"X-L","role":"selectorAction","actionResource":"X-O"}"#,
        r#"{"resource":"X-L","role":"selectorAction","actionResource":"X-O","selectedProperty":"not a name"}"#,
    ] {
        assert!(
            PeData::decode(&PeResource::ChannelList, &json(&entry(bad))).is_err(),
            "{bad}"
        );
    }
}

// M2-103 §13 Table 64: any other role is kept by its name, of at most 32
// characters, the manufacturer's `x-` roles among them.
#[test]
fn another_role_is_kept_by_name() {
    let entry = |role: &str| {
        format!(r#"[{{"title":"T","channel":1,"links":[{{"resource":"X-A","role":"{role}"}}]}}]"#)
    };
    let text = entry("x-vendorRole");
    let PeData::ChannelList(list) = PeData::decode(&PeResource::ChannelList, &json(&text)).unwrap()
    else {
        panic!("not ChannelList");
    };
    assert!(
        matches!(&list.entries()[0].links[0].role, Some(LinkRole::Other(n)) if n.as_str() == "x-vendorRole")
    );
    assert!(PeData::decode(&PeResource::ChannelList, &json(&entry(&"r".repeat(33)))).is_err());
}

// M2-103 §7.1.1, §10: one JSON value type reads and writes both: a header property
// and a change of a Partial Set are scalars, Property Data is anything.
#[test]
fn a_header_property_and_a_change_are_scalars_of_the_one_json_type() {
    let mut extra = PeExtra::default();
    let name = super::super::limits::PropertyName::new("x-a").unwrap();
    assert_eq!(
        extra.insert(name.clone(), PeJson::Array(vec![])),
        Err(PeExtraError::NotScalar)
    );
    assert_eq!(
        extra.insert(name.clone(), PeJson::Null),
        Err(PeExtraError::NotScalar)
    );
    assert!(extra.insert(name, PeJson::Integer(1)).is_ok());
    assert!(
        PartialUpdate::new()
            .with(pointer("/a"), PeJson::Object(PeObject::new()))
            .is_err()
    );
    // The grammar of Header Data has no whitespace and no nesting; Property Data has both.
    assert!(PeHeader::parse(br#"{"resource": "A"}"#).is_err());
    assert!(PeHeader::parse(br#"{"resource":"A","x-n":[1]}"#).is_err());
    assert!(PeJson::parse(br#"{"resource": "A", "x-n": [1]}"#).is_ok());
}
