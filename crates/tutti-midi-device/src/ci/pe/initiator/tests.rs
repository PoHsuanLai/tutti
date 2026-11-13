//! The Property Exchange Initiator against a scripted peer built from the pure
//! pieces: `split_message` for what the peer sends and a `PeReassembler` for what
//! it receives. Each test names the property of the spec it holds.

use std::time::{Duration, Instant};

use tutti_ump::U7;

use super::super::*;
use super::RequestIds;
use crate::MaxSysexSize;
use crate::ci::testing::{OTHER, OURS, PEER};
use crate::ci::{Ack, CiHeader, CiMessage, Decoded, Muid, Nak, TimeoutWait};

const SECOND: Duration = Duration::from_secs(1);
const MAX: u32 = 128;

fn id(n: u8) -> PeRequestId {
    PeRequestId::of(n)
}

fn name(text: &str) -> ResourceName {
    ResourceName::new(text).unwrap()
}

fn settings() -> PeSettings {
    PeSettings {
        capabilities: PeCapabilities::new(SimultaneousRequests::of(4)),
        peer_max_sysex: MaxSysexSize::of(MAX),
        reassembly: PeReassemblyLimits {
            streams: StreamLimit::new(8).unwrap(),
            bytes: BufferLimit::new(1 << 20).unwrap(),
        },
        body_limit: BodyLimit::new(1 << 16).unwrap(),
    }
}

fn from_peer(message: PeCiMessage) -> Decoded {
    Decoded::Message(CiMessage::PropertyExchange {
        header: CiHeader::new(Muid::of(PEER)),
        to: Muid::of(OURS),
        message,
    })
}

fn caps_reply(requests: u8) -> Decoded {
    from_peer(PeCiMessage::CapabilitiesReply(PeCapabilities::new(
        SimultaneousRequests::of(requests),
    )))
}

fn chunk_from_peer(kind: PeMessageKind, chunk: PeChunk) -> Decoded {
    from_peer(PeCiMessage::Chunk { kind, chunk })
}

/// A peer that takes `requests` at once, ready for requests at the returned time.
fn ready(requests: u8) -> (PeInitiator, Instant) {
    let now = Instant::now();
    let (mut initiator, _) = PeInitiator::new(Muid::of(OURS), Muid::of(PEER), settings(), now);
    initiator.on_message(&caps_reply(requests), now);
    (initiator, now)
}

fn sent(actions: &[PeAction]) -> Vec<&CiMessage> {
    actions
        .iter()
        .filter_map(|a| match a {
            PeAction::Send(message) => Some(message),
            _ => None,
        })
        .collect()
}

/// The chunks sent, in order, with the kind of message each is in.
fn chunks(actions: &[PeAction]) -> Vec<(PeMessageKind, PeChunk)> {
    sent(actions)
        .into_iter()
        .filter_map(|m| match m {
            CiMessage::PropertyExchange {
                message: PeCiMessage::Chunk { kind, chunk },
                ..
            } => Some((*kind, chunk.clone())),
            _ => None,
        })
        .collect()
}

fn finished(actions: &[PeAction]) -> Vec<(PeRequestId, PeKind, PeOutcome)> {
    actions
        .iter()
        .filter_map(|a| match a {
            PeAction::Finished {
                request_id,
                kind,
                outcome,
            } => Some((*request_id, *kind, outcome.clone())),
            _ => None,
        })
        .collect()
}

fn one_finished(actions: &[PeAction]) -> (PeRequestId, PeKind, PeOutcome) {
    let mut all = finished(actions);
    assert_eq!(all.len(), 1, "{actions:?}");
    all.remove(0)
}

fn naks(actions: &[PeAction]) -> Vec<&Nak> {
    sent(actions)
        .into_iter()
        .filter_map(|m| match m {
            CiMessage::Nak { nak, .. } => Some(nak),
            _ => None,
        })
        .collect()
}

fn acks(actions: &[PeAction]) -> Vec<&Ack> {
    sent(actions)
        .into_iter()
        .filter_map(|m| match m {
            CiMessage::Ack { ack, .. } => Some(ack),
            _ => None,
        })
        .collect()
}

fn raw(details: &[U7; 5]) -> [u8; 5] {
    details.map(U7::get)
}

fn signal_message(transaction: PeTransaction, signal: PeSignal) -> Decoded {
    let header = CiHeader::new(Muid::of(PEER));
    let to = Muid::of(OURS);
    Decoded::Message(match signal.answer(transaction) {
        PeAnswer::Ack(ack) => CiMessage::Ack { header, to, ack },
        PeAnswer::Nak(nak) => CiMessage::Nak { header, to, nak },
    })
}

/// The chunks of a reply of the peer's.
fn reply_chunks(
    request: PeRequestId,
    reply: PeReply,
    wire_body: &[u8],
) -> Vec<(PeMessageKind, PeChunk)> {
    split_message(
        request,
        &PeHeader::Reply(reply),
        wire_body,
        MaxSysexSize::of(MAX),
    )
    .unwrap()
    .into_iter()
    .map(|c| (PeMessageKind::GET_REPLY, c))
    .collect()
}

fn with_kind(
    chunks: Vec<(PeMessageKind, PeChunk)>,
    kind: PeMessageKind,
) -> Vec<(PeMessageKind, PeChunk)> {
    chunks.into_iter().map(|(_, c)| (kind, c)).collect()
}

fn ok(status: PeStatus) -> PeReply {
    PeReply::new(status)
}

/// Delivers a whole reply at `now` and returns the actions of its last chunk.
fn deliver(
    initiator: &mut PeInitiator,
    chunks: Vec<(PeMessageKind, PeChunk)>,
    now: Instant,
) -> Vec<PeAction> {
    let mut last = Vec::new();
    for (kind, chunk) in chunks {
        last = initiator.on_message(&chunk_from_peer(kind, chunk), now);
    }
    last
}

fn get(resource: &str) -> PeCall {
    PeCall::Get(PeGet::new(name(resource)))
}

fn success(outcome: PeOutcome) -> PeResponse {
    match outcome {
        PeOutcome::Success(response) => response,
        other => panic!("not a success: {other:?}"),
    }
}

/// The peer's side of a message the Initiator sent: its chunks reassembled.
fn received(sent_chunks: Vec<(PeMessageKind, PeChunk)>) -> PeReceived {
    let mut inbox = PeReassembler::new(settings().reassembly);
    let now = Instant::now();
    let mut done = None;
    for (kind, chunk) in sent_chunks {
        if let Ok(PeStep::Complete { message, .. }) =
            inbox.push(Muid::of(OURS), kind, chunk, PeFlowControl::Off, now)
        {
            done = Some(*message);
        }
    }
    done.expect("a whole message")
}

// M2-103 §3.2 step 2, M2-101 §8.5: Property Exchange Capabilities is asked
// before anything else, and Table 30 is its message.
#[test]
fn the_capabilities_inquiry_is_the_first_message_and_nothing_is_asked_before_the_reply() {
    let now = Instant::now();
    let (mut i, actions) = PeInitiator::new(Muid::of(OURS), Muid::of(PEER), settings(), now);
    let [message] = sent(&actions)[..] else {
        panic!("one message, got {actions:?}");
    };
    assert_eq!(
        message.encode(),
        [
            0x7E, 0x7F, 0x0D, 0x30, 0x02, // preamble, sub-ID#2, version 2
            0x01, 0, 0, 0x08, // our MUID, 0x0100_0001, 7 bits a byte, least significant first
            0x02, 0, 0, 0x10, // the peer's MUID, 0x0200_0002
            4, 0, 0, // four requests, Common Rules 1.1
        ]
    );
    assert_eq!(
        i.start(get("State"), now),
        Err(PeError::CapabilitiesPending)
    );
    assert_eq!(i.capabilities(), None);
    let actions = i.on_message(&caps_reply(3), now);
    assert_eq!(
        actions,
        [PeAction::Capabilities(PeCapabilities::new(
            SimultaneousRequests::of(3)
        ))]
    );
    assert!(i.start(get("State"), now).is_ok());
}

// M2-101 §8.5 (Table 32 and the Version 1 form): the reply of a Message Format
// Version 1 peer has no version fields, and the exchange still succeeds.
#[test]
fn a_version_1_capabilities_reply_is_enough() {
    let now = Instant::now();
    let (mut i, _) = PeInitiator::new(Muid::of(OURS), Muid::of(PEER), settings(), now);
    let reply = PeCapabilities {
        requests: SimultaneousRequests::ONE,
        version: None,
    };
    i.on_message(&from_peer(PeCiMessage::CapabilitiesReply(reply)), now);
    assert_eq!(i.capabilities(), Some(&reply));
}

// M2-103 §12.2: a Responder replies within 3 seconds; a Capabilities inquiry
// that is not answered is over at that instant, and a NAK of it ends it too.
#[test]
fn the_capabilities_inquiry_times_out_at_three_seconds_and_a_nak_refuses_it() {
    let now = Instant::now();
    let (mut i, _) = PeInitiator::new(Muid::of(OURS), Muid::of(PEER), settings(), now);
    assert_eq!(i.deadline(), Some(now + 3 * SECOND));
    assert!(
        i.on_timeout(now + 3 * SECOND - Duration::from_nanos(1))
            .is_empty()
    );
    assert_eq!(
        i.on_timeout(now + 3 * SECOND),
        [PeAction::CapabilitiesFailed(
            PeCapabilitiesFailure::TimedOut
        )]
    );
    assert_eq!(
        i.start(get("State"), now),
        Err(PeError::CapabilitiesUnavailable)
    );
    assert_eq!(i.deadline(), None);

    let (mut i, _) = PeInitiator::new(Muid::of(OURS), Muid::of(PEER), settings(), now);
    let nak = Nak::new(U7::of(0x30), Nak::STATUS_MESSAGE_NOT_SUPPORTED);
    let refusal = Decoded::Message(CiMessage::Nak {
        header: CiHeader::new(Muid::of(PEER)),
        to: Muid::of(OURS),
        nak: nak.clone(),
    });
    assert_eq!(
        i.on_message(&refusal, now),
        [PeAction::CapabilitiesFailed(
            PeCapabilitiesFailure::Refused(nak)
        )]
    );
    // Asking again starts the exchange over.
    let again = i.ask_capabilities(now);
    assert_eq!(sent(&again).len(), 1);
    i.on_message(&caps_reply(1), now);
    assert!(i.capabilities().is_some());
}

// M2-101 Table 33 and M2-103 §15.2 Table 95: a Get is one chunk of Header Data and
// no Property Data, numbered 1 of 1 (M2-101 §8.3.2), to the Function Block.
#[test]
fn a_get_is_one_header_only_chunk() {
    let (mut i, now) = ready(2);
    let mut get = PeGet::new(name("State"));
    get.res_id = Some(ResourceId::new("buffer").unwrap());
    get.encoding = PeEncoding::Mcoded7;
    get.flow_control = PeFlowControl::On;
    let (request, actions) = i.start(PeCall::Get(get), now).unwrap();
    let [message] = sent(&actions)[..] else {
        panic!("one message");
    };
    let text =
        br#"{"resource":"State","resId":"buffer","mutualEncoding":"Mcoded7","flowControl":true}"#;
    let mut expected = vec![
        0x7E,
        0x7F,
        0x0D,
        0x34,
        0x02, // preamble, Get Property Data, version 2
        0x01,
        0,
        0,
        0x08, // our MUID
        0x02,
        0,
        0,
        0x10, // the peer's MUID
        request.get(),
        u8::try_from(text.len()).unwrap(),
        0,
    ];
    expected.extend(text);
    expected.extend([1, 0, 1, 0, 0, 0]);
    assert_eq!(message.encode(), expected);
}

// M2-103 §9, §6.2: the reply is reassembled whatever its chunks, its Property
// Data decoded by the encoding the reply names, and the status, header and body
// delivered together.
#[test]
fn a_get_reply_of_many_chunks_is_decoded_by_its_mutual_encoding() {
    let (mut i, now) = ready(2);
    let mut call = PeGet::new(name("State"));
    call.encoding = PeEncoding::Mcoded7;
    let (request, _) = i.start(PeCall::Get(call), now).unwrap();
    let body: Vec<u8> = (0..=255).cycle().take(300).collect();
    let mut reply = ok(PeStatus::Ok);
    reply.mutual_encoding = Some(PeEncoding::Mcoded7);
    reply.cache_time_secs = Some(30);
    let wire = encode_body(PeEncoding::Mcoded7, &body).unwrap();
    let chunks = reply_chunks(request, reply.clone(), &wire);
    assert!(chunks.len() > 2);
    let early = &chunks[..chunks.len() - 1];
    for (kind, chunk) in early {
        let actions = i.on_message(&chunk_from_peer(*kind, chunk.clone()), now);
        assert!(actions.is_empty(), "no Flow Control was asked: {actions:?}");
    }
    let actions = deliver(&mut i, chunks[chunks.len() - 1..].to_vec(), now);
    let (finished_id, kind, outcome) = one_finished(&actions);
    assert_eq!((finished_id, kind), (request, PeKind::Get));
    let response = success(outcome);
    assert_eq!(response.status(), PeStatus::Ok);
    assert_eq!(response.header, reply);
    assert_eq!(response.body, body);
}

// M2-103 §6.1.6: Property Data is JSON in ASCII by default, so a reply that names
// no encoding is ASCII; zlib+Mcoded7 inflates.
#[test]
fn a_reply_without_an_encoding_is_ascii_and_a_zlib_reply_inflates() {
    let (mut i, now) = ready(2);
    let (request, _) = i.start(get("State"), now).unwrap();
    let chunks = reply_chunks(request, ok(PeStatus::Ok), br#"{"a":1}"#);
    let outcome = one_finished(&deliver(&mut i, chunks, now)).2;
    assert_eq!(success(outcome).body, br#"{"a":1}"#);

    let (request, _) = i.start(get("State"), now).unwrap();
    let body = vec![b'x'; 2000];
    let mut reply = ok(PeStatus::Ok);
    reply.mutual_encoding = Some(PeEncoding::ZlibMcoded7);
    let wire = encode_body(PeEncoding::ZlibMcoded7, &body).unwrap();
    let outcome = one_finished(&deliver(&mut i, reply_chunks(request, reply, &wire), now)).2;
    assert_eq!(success(outcome).body, body);
}

// A body that does not decode in the encoding named is a reply that cannot be used.
#[test]
fn a_body_that_does_not_decode_is_unreadable() {
    let (mut i, now) = ready(2);
    let (request, _) = i.start(get("State"), now).unwrap();
    let mut reply = ok(PeStatus::Ok);
    reply.mutual_encoding = Some(PeEncoding::ZlibMcoded7);
    let outcome = one_finished(&deliver(
        &mut i,
        reply_chunks(request, reply, b"not zlib"),
        now,
    ))
    .2;
    assert!(matches!(
        outcome,
        PeOutcome::Unreadable(PeUnreadable::Body(_))
    ));
}

// M2-103 §7.4.1 Table 15: 200-299 do not retry, 300-399 should, 400-499 fail, 500
// is a major error. Each ends the request with its status; none is resent.
#[test]
fn every_status_ends_the_request_with_its_status_and_nothing_is_retried() {
    for (status, class) in [
        (PeStatus::Ok, 'S'),
        (PeStatus::Accepted, 'S'),
        (PeStatus::Unavailable, 'R'),
        (PeStatus::BadData, 'R'),
        (PeStatus::TooManyRequests, 'R'),
        (PeStatus::BadRequest, 'F'),
        (PeStatus::NotFound, 'F'),
        (PeStatus::PayloadTooLarge, 'F'),
        (PeStatus::InternalError, 'F'),
    ] {
        let (mut i, now) = ready(2);
        let (request, _) = i.start(get("State"), now).unwrap();
        let mut reply = ok(status);
        reply.message = PeMessage::new("why");
        let actions = deliver(&mut i, reply_chunks(request, reply.clone(), b""), now);
        assert_eq!(sent(&actions).len(), 0, "{status:?} is not retried");
        let outcome = one_finished(&actions).2;
        let response = match (class, outcome) {
            ('S', PeOutcome::Success(r))
            | ('R', PeOutcome::Retry(r))
            | ('F', PeOutcome::Failed(r)) => r,
            (class, other) => panic!("{status:?} is {class}, got {other:?}"),
        };
        assert_eq!(response.status(), status);
        assert_eq!(response.header, reply);
        assert_eq!(i.in_flight(), 0);
    }
}

// M2-103 §7.4.1: a Reply carries only the codes of Table 15; a Notify status in
// one is not a reply.
#[test]
fn a_notify_status_is_not_a_reply_status() {
    let (mut i, now) = ready(2);
    let (request, _) = i.start(get("State"), now).unwrap();
    let outcome = one_finished(&deliver(
        &mut i,
        reply_chunks(request, ok(PeStatus::TimedOut), b""),
        now,
    ))
    .2;
    assert_eq!(
        outcome,
        PeOutcome::Unreadable(PeUnreadable::NotAReplyStatus(PeStatus::TimedOut))
    );
}

// M2-103 §15, §15.2: under Flow Control each chunk of the reply is acknowledged
// with ACK 0x11 naming the chunk (Table 94), and the last too for a Get.
#[test]
fn a_get_under_flow_control_acknowledges_every_chunk_including_the_last() {
    let (mut i, now) = ready(2);
    let mut call = PeGet::new(name("State"));
    call.encoding = PeEncoding::Mcoded7;
    call.flow_control = PeFlowControl::On;
    let (request, _) = i.start(PeCall::Get(call), now).unwrap();
    let mut reply = ok(PeStatus::Ok);
    reply.mutual_encoding = Some(PeEncoding::Mcoded7);
    let wire = encode_body(PeEncoding::Mcoded7, &[7u8; 200]).unwrap();
    let chunks = reply_chunks(request, reply, &wire);
    let count = chunks.len();
    assert!(count >= 3);
    for (n, (kind, chunk)) in chunks.into_iter().enumerate() {
        let actions = i.on_message(&chunk_from_peer(kind, chunk), now);
        let [ack] = acks(&actions)[..] else {
            panic!("chunk {}: one ACK, got {actions:?}", n + 1);
        };
        assert_eq!(ack.ack_sub_id2.get(), 0x34);
        assert_eq!(ack.status_code.get(), 0x11);
        assert_eq!(raw(&ack.details), [request.get(), (n + 1) as u8, 0, 0, 0]);
        assert_eq!(finished(&actions).len(), usize::from(n + 1 == count));
    }
}

// M2-103 §12.2: the time to a reply is under 3 seconds: the request is over at its
// deadline, a message arriving at that instant is late, and the NAK 0x42 says so
// (§12.2.3); the Request ID is free again.
#[test]
fn a_request_times_out_at_three_seconds_and_its_id_is_free() {
    let (mut i, now) = ready(1);
    let (request, _) = i.start(get("State"), now).unwrap();
    assert_eq!(i.deadline(), Some(now + 3 * SECOND));
    assert!(
        i.on_timeout(now + 3 * SECOND - Duration::from_nanos(1))
            .is_empty()
    );
    let actions = i.on_timeout(now + 3 * SECOND);
    let [nak] = naks(&actions)[..] else {
        panic!("one NAK");
    };
    assert_eq!(nak.nak_sub_id2.get(), 0x34);
    assert_eq!(nak.status_code, Nak::STATUS_TIMEOUT);
    assert_eq!(raw(&nak.details)[0], request.get());
    assert_eq!(
        one_finished(&actions),
        (request, PeKind::Get, PeOutcome::TimedOut)
    );
    assert_eq!(i.in_flight(), 0);
    assert_eq!(i.deadline(), None);
    // The one request the peer takes can be made again.
    assert!(i.start(get("State"), now + 3 * SECOND).is_ok());
}

// M2-103 §12.2: the time between chunks is under 3 seconds, from each chunk.
#[test]
fn the_time_between_chunks_counts_from_each_chunk() {
    let (mut i, now) = ready(1);
    let (request, _) = i.start(get("State"), now).unwrap();
    let mut reply = ok(PeStatus::Ok);
    reply.mutual_encoding = Some(PeEncoding::Mcoded7);
    let wire = encode_body(PeEncoding::Mcoded7, &[1u8; 200]).unwrap();
    let chunks = reply_chunks(request, reply, &wire);
    assert!(chunks.len() >= 3);
    let (kind, first) = chunks[0].clone();
    i.on_message(&chunk_from_peer(kind, first), now + 2 * SECOND);
    assert_eq!(i.deadline(), Some(now + 5 * SECOND));
    assert!(
        i.on_timeout(now + 5 * SECOND - Duration::from_nanos(1))
            .is_empty()
    );
    let actions = i.on_timeout(now + 5 * SECOND);
    assert_eq!(one_finished(&actions).2, PeOutcome::TimedOut);
    assert_eq!(naks(&actions).len(), 1);
    // The reply's remaining chunks are for nothing now.
    let (kind, second) = chunks[1].clone();
    assert!(
        i.on_message(&chunk_from_peer(kind, second), now + 6 * SECOND)
            .is_empty()
    );
}

fn timeout_wait(request: PeRequestId, tenths: u8) -> Decoded {
    signal_message(
        PeTransaction::Get,
        PeSignal::TimeoutWait {
            request_id: request,
            wait: TimeoutWait::of(tenths),
            chunk: None,
        },
    )
}

// M2-103 §12.2.1: ACK 0x10 adds its wait, in tenths of a second, to the time that
// is left; a wait of 0 adds nothing, and each wait counts on from the deadline.
#[test]
fn a_timeout_wait_adds_its_period_to_the_deadline() {
    let (mut i, now) = ready(1);
    let (request, _) = i.start(get("State"), now).unwrap();
    // Example 1 of §12.2.1: 0x3C is 6 seconds, 9 seconds in all.
    assert!(
        i.on_message(&timeout_wait(request, 0x3C), now + SECOND)
            .is_empty()
    );
    assert_eq!(i.deadline(), Some(now + 9 * SECOND));
    assert!(
        i.on_timeout(now + 9 * SECOND - Duration::from_nanos(1))
            .is_empty()
    );
    // A second wait adds to it again; 0 adds nothing.
    i.on_message(&timeout_wait(request, 0x1E), now + 8 * SECOND);
    assert_eq!(i.deadline(), Some(now + 12 * SECOND));
    i.on_message(&timeout_wait(request, 0), now + 9 * SECOND);
    assert_eq!(i.deadline(), Some(now + 12 * SECOND));
    assert_eq!(
        one_finished(&i.on_timeout(now + 12 * SECOND)).2,
        PeOutcome::TimedOut
    );
}

// An ACK for a request that is not in flight, or of another transaction, or from
// another device, extends nothing.
#[test]
fn a_timeout_wait_for_nothing_in_flight_extends_nothing() {
    let (mut i, now) = ready(2);
    let (request, _) = i.start(get("State"), now).unwrap();
    i.on_message(&timeout_wait(id(request.get() ^ 1), 0x7F), now);
    let set_wait = signal_message(
        PeTransaction::Set,
        PeSignal::TimeoutWait {
            request_id: request,
            wait: TimeoutWait::of(0x7F),
            chunk: None,
        },
    );
    i.on_message(&set_wait, now);
    let Decoded::Message(CiMessage::Ack { to, ack, .. }) = timeout_wait(request, 0x7F) else {
        unreachable!()
    };
    let other = Decoded::Message(CiMessage::Ack {
        header: CiHeader::new(Muid::of(OTHER)),
        to,
        ack,
    });
    i.on_message(&other, now);
    assert_eq!(i.deadline(), Some(now + 3 * SECOND));
}

// The wait also holds a reply that has begun: the chunk after a late one is on time.
#[test]
fn a_timeout_wait_holds_a_reply_that_has_begun() {
    let (mut i, now) = ready(1);
    let (request, _) = i.start(get("State"), now).unwrap();
    let mut reply = ok(PeStatus::Ok);
    reply.mutual_encoding = Some(PeEncoding::Mcoded7);
    let wire = encode_body(PeEncoding::Mcoded7, &[1u8; 200]).unwrap();
    let chunks = reply_chunks(request, reply, &wire);
    let (kind, first) = chunks[0].clone();
    i.on_message(&chunk_from_peer(kind, first), now);
    i.on_message(&timeout_wait(request, 0x3C), now + SECOND);
    assert_eq!(i.deadline(), Some(now + 9 * SECOND));
    assert!(i.on_timeout(now + 8 * SECOND).is_empty());
    let last = deliver(&mut i, chunks[1..].to_vec(), now + 8 * SECOND);
    assert!(success(one_finished(&last).2).body.len() == 200);
}

// M2-103 §12.1.1: a NAK with Terminate and status data 0 from the Responder ends
// the request, and both sides consider it concluded.
#[test]
fn a_terminate_nak_ends_the_request() {
    let (mut i, now) = ready(1);
    let (request, _) = i.start(get("State"), now).unwrap();
    let terminate = signal_message(
        PeTransaction::Get,
        PeSignal::Terminate {
            request_id: request,
            by: PeTerminator::Responder,
        },
    );
    let actions = i.on_message(&terminate, now);
    assert_eq!(
        one_finished(&actions),
        (
            request,
            PeKind::Get,
            PeOutcome::Terminated(PeTerminator::Responder)
        )
    );
    assert!(sent(&actions).is_empty());
    assert_eq!(i.in_flight(), 0);
    assert_eq!(i.deadline(), None);
}

// M2-101 Table 16: a NAK of out of sequence, malformed or timeout ends the request
// and says which.
#[test]
fn the_other_chunk_naks_end_the_request_with_their_signal() {
    for signal in [
        PeSignal::OutOfSequence { request_id: id(0) },
        PeSignal::Malformed { request_id: id(0) },
        PeSignal::TimedOut { request_id: id(0) },
    ] {
        let (mut i, now) = ready(1);
        let (request, _) = i.start(get("State"), now).unwrap();
        assert_eq!(request, id(0));
        let actions = i.on_message(&signal_message(PeTransaction::Get, signal), now);
        assert_eq!(one_finished(&actions).2, PeOutcome::Nak(signal));
    }
}

fn set_body() -> Vec<u8> {
    (0..=255u8).cycle().take(300).collect()
}

fn set(flow: PeFlowControl) -> PeSet {
    let mut set = PeSet::new(name("State"), set_body());
    set.encoding = PeEncoding::Mcoded7;
    set.flow_control = flow;
    set
}

// M2-103 §10, M2-101 §8.3: a Set encodes its Property Data, splits it so that each
// chunk fits the peer's receivable size, carries the header in chunk 1 only, and
// sends every chunk at once when no Flow Control was asked.
#[test]
fn a_set_sends_every_chunk_at_once_without_flow_control() {
    let (mut i, now) = ready(1);
    let (request, actions) = i.start(PeCall::Set(set(PeFlowControl::Off)), now).unwrap();
    let sent_chunks = chunks(&actions);
    assert!(sent_chunks.len() > 2);
    for (n, (kind, chunk)) in sent_chunks.iter().enumerate() {
        assert_eq!(*kind, PeMessageKind::SET_INQUIRY);
        assert_eq!(chunk.request_id(), request);
        assert!(chunk.sysex_len() <= MAX as usize);
        assert_eq!(chunk.header().is_empty(), n > 0);
    }
    let message = received(sent_chunks);
    let PeHeader::Request(header) = &message.header else {
        panic!("not a request");
    };
    assert_eq!(header.resource, name("State"));
    assert_eq!(header.mutual_encoding, Some(PeEncoding::Mcoded7));
    assert_eq!(header.flow_control, PeFlowControl::Off);
    assert_eq!(header.set_scope, PeSetScope::Whole);
    assert_eq!(
        decode_body(
            PeEncoding::Mcoded7,
            &message.body,
            BodyLimit::new(1 << 12).unwrap()
        )
        .unwrap(),
        set_body()
    );
    // The reply is the whole answer: the status of a Set (§7.4).
    let reply = reply_chunks(request, ok(PeStatus::Accepted), b"");
    let actions = deliver(&mut i, with_kind(reply, PeMessageKind::SET_REPLY), now);
    let (_, kind, outcome) = one_finished(&actions);
    assert_eq!(kind, PeKind::Set);
    assert_eq!(success(outcome).status(), PeStatus::Accepted);
}

// M2-103 §10.2: a Partial Set says so in its header.
#[test]
fn a_partial_set_says_so() {
    let (mut i, now) = ready(1);
    let mut call = PeSet::new(name("State"), br#"{"a":1}"#.to_vec());
    call.scope = PeSetScope::Partial;
    let (_, actions) = i.start(PeCall::Set(call), now).unwrap();
    let message = received(chunks(&actions));
    let PeHeader::Request(header) = message.header else {
        panic!("not a request");
    };
    assert_eq!(header.set_scope, PeSetScope::Partial);
    assert_eq!(message.body, br#"{"a":1}"#);
}

fn ack_next(request: PeRequestId, chunk: u16) -> Decoded {
    signal_message(
        PeTransaction::Set,
        PeSignal::SendNext {
            request_id: request,
            chunk: ChunkNo::of(chunk),
        },
    )
}

fn resend_nak(request: PeRequestId, last_good: Option<u16>) -> Decoded {
    signal_message(
        PeTransaction::Set,
        PeSignal::Resend {
            request_id: request,
            last_good: last_good.map(ChunkNo::of),
        },
    )
}

// M2-103 §15, §15.3: under Flow Control the Sender sends chunk 1, and each next
// chunk after the ACK for the one before; the last needs no ACK, since the reply
// acknowledges the data.
#[test]
fn a_set_under_flow_control_sends_a_chunk_per_ack_and_needs_none_for_the_last() {
    let (mut i, now) = ready(1);
    let (request, actions) = i.start(PeCall::Set(set(PeFlowControl::On)), now).unwrap();
    let first = chunks(&actions);
    assert_eq!(first.len(), 1, "only chunk 1 until it is acknowledged");
    let total = match first[0].1.total() {
        ChunkTotal::Known(n) => n.get(),
        ChunkTotal::Unknown => panic!("unknown count"),
    };
    assert!(total >= 3);
    let mut all = first;
    for n in 1..total {
        // An ACK of another chunk, or twice, sends nothing.
        if n > 1 {
            assert!(i.on_message(&ack_next(request, n - 1), now).is_empty());
        }
        let actions = i.on_message(&ack_next(request, n), now);
        let next = chunks(&actions);
        assert_eq!(next.len(), 1, "after the ACK of chunk {n}");
        assert_eq!(
            next[0].1.position(),
            ChunkPosition::Number(ChunkNo::of(n + 1))
        );
        all.extend(next);
        assert!(i.on_message(&ack_next(request, n), now).is_empty());
    }
    // The ACK of the last chunk, if one comes, asks for nothing more.
    assert!(i.on_message(&ack_next(request, total), now).is_empty());
    let message = received(all);
    assert_eq!(
        decode_body(
            PeEncoding::Mcoded7,
            &message.body,
            BodyLimit::new(1 << 12).unwrap()
        )
        .unwrap(),
        set_body()
    );
    let reply = reply_chunks(request, ok(PeStatus::Ok), b"");
    let actions = deliver(&mut i, with_kind(reply, PeMessageKind::SET_REPLY), now);
    assert_eq!(success(one_finished(&actions).2).status(), PeStatus::Ok);
}

// M2-103 §15.4: a NAK 0x12 asks for the most recent chunk again, and it is the
// same chunk; a chunk is sent again at most three times (§15.5), and then the
// transaction ends with a Terminate NAK from the Initiator (status data 1).
#[test]
fn a_resend_nak_sends_the_chunk_again_and_the_fourth_ends_the_transaction() {
    let (mut i, now) = ready(1);
    let (request, _) = i.start(PeCall::Set(set(PeFlowControl::On)), now).unwrap();
    let actions = i.on_message(&ack_next(request, 1), now);
    let second = chunks(&actions);
    assert_eq!(
        second[0].1.position(),
        ChunkPosition::Number(ChunkNo::of(2))
    );
    for _ in 0..3 {
        let actions = i.on_message(&resend_nak(request, Some(1)), now);
        assert_eq!(chunks(&actions), second, "the most recent chunk, as it was");
    }
    let actions = i.on_message(&resend_nak(request, Some(1)), now);
    let [nak] = naks(&actions)[..] else {
        panic!("one NAK, got {actions:?}");
    };
    assert_eq!(nak.status_code, Nak::STATUS_TERMINATE);
    assert_eq!(nak.status_data.get(), 1);
    assert_eq!(nak.nak_sub_id2.get(), 0x36);
    assert_eq!(
        one_finished(&actions).2,
        PeOutcome::Terminated(PeTerminator::Initiator)
    );
    assert_eq!(i.in_flight(), 0);
}

// An ACK in between gives the chunk after it a fresh count.
#[test]
fn an_ack_resets_the_count_of_resends() {
    let (mut i, now) = ready(1);
    let (request, _) = i.start(PeCall::Set(set(PeFlowControl::On)), now).unwrap();
    for n in 1..=2 {
        i.on_message(&ack_next(request, n), now);
        for _ in 0..3 {
            let actions = i.on_message(&resend_nak(request, Some(n)), now);
            assert_eq!(chunks(&actions).len(), 1);
        }
    }
    assert_eq!(i.in_flight(), 1);
}

// M2-103 §15.5: no ACK within 3 seconds of a chunk sends it again, three times;
// then the Sender fails and sends a Terminate NAK.
#[test]
fn a_chunk_with_no_ack_is_sent_again_after_three_seconds_and_given_up_after_three_resends() {
    let (mut i, now) = ready(1);
    let (request, actions) = i.start(PeCall::Set(set(PeFlowControl::On)), now).unwrap();
    let first = chunks(&actions);
    assert_eq!(i.deadline(), Some(now + 3 * SECOND));
    assert!(
        i.on_timeout(now + 3 * SECOND - Duration::from_nanos(1))
            .is_empty()
    );
    let mut at = now;
    for _ in 0..3 {
        at += 3 * SECOND;
        let actions = i.on_timeout(at);
        assert_eq!(chunks(&actions), first);
        assert!(finished(&actions).is_empty());
        assert_eq!(i.deadline(), Some(at + 3 * SECOND));
    }
    at += 3 * SECOND;
    let actions = i.on_timeout(at);
    let [nak] = naks(&actions)[..] else {
        panic!("one NAK, got {actions:?}");
    };
    assert_eq!(nak.status_code, Nak::STATUS_TERMINATE);
    assert_eq!(nak.status_data.get(), 1);
    assert_eq!(
        one_finished(&actions),
        (request, PeKind::Set, PeOutcome::TimedOut)
    );
}

// M2-103 §12.2.1 holds for the wait on an ACK too.
#[test]
fn a_timeout_wait_holds_a_chunk_that_waits_for_its_ack() {
    let (mut i, now) = ready(1);
    let (request, _) = i.start(PeCall::Set(set(PeFlowControl::On)), now).unwrap();
    i.on_message(
        &signal_message(
            PeTransaction::Set,
            PeSignal::TimeoutWait {
                request_id: request,
                wait: TimeoutWait::of(0x1E),
                chunk: Some(ChunkNo::FIRST),
            },
        ),
        now,
    );
    assert!(i.on_timeout(now + 3 * SECOND).is_empty());
    assert_eq!(chunks(&i.on_timeout(now + 6 * SECOND)).len(), 1);
}

// M2-103 §5.3: a Request ID is 0 to 127, no more requests are made at once than the
// peer's count, and an ID is free when its transaction is complete.
#[test]
fn requests_use_distinct_ids_up_to_the_peers_count_and_ids_come_free() {
    let (mut i, now) = ready(2);
    let (a, _) = i.start(get("A"), now).unwrap();
    let (b, _) = i.start(get("B"), now).unwrap();
    assert_ne!(a, b);
    let refused = i.start(get("C"), now);
    assert_eq!(
        refused,
        Err(PeError::TooManyRequests {
            limit: SimultaneousRequests::of(2)
        })
    );
    assert_eq!(i.in_flight(), 2);
    // A reply to `a` ends it: another can be made, under an ID not in use.
    let actions = deliver(&mut i, reply_chunks(a, ok(PeStatus::Ok), b""), now);
    assert_eq!(finished(&actions).len(), 1);
    let (c, _) = i.start(get("C"), now).unwrap();
    assert_ne!(c, b);
    assert!(i.start(get("D"), now).is_err());
}

// §5.3: the reply that answers a request ends it for that ID only; a timeout frees
// the ID too ("Requests that don't receive a response will time out and the Request ID
// can be reused").
#[test]
fn a_timeout_frees_the_id_of_a_full_pool() {
    let (mut i, now) = ready(2);
    i.start(get("A"), now).unwrap();
    i.start(get("B"), now + SECOND).unwrap();
    assert!(i.start(get("C"), now + SECOND).is_err());
    let actions = i.on_timeout(now + 3 * SECOND);
    assert_eq!(finished(&actions).len(), 1, "only the first is due");
    assert!(i.start(get("C"), now + 3 * SECOND).is_ok());
}

// M2-103 §5.3: there are 128 Request IDs, 0 to 127, and each is given to one request
// at a time; the pool refuses when every ID is in use, and one that is released is
// the one given next.
#[test]
fn the_pool_gives_each_of_the_128_ids_once_and_refuses_the_next() {
    let mut ids = RequestIds::default();
    let mut given = std::collections::BTreeSet::new();
    for _ in 0..128 {
        let id = ids.allocate(128).expect("an ID is free");
        assert!(given.insert(id.get()), "{id:?} given twice");
    }
    assert_eq!(given.len(), 128);
    assert_eq!(given.iter().copied().max(), Some(127));
    assert_eq!(ids.allocate(128), None);
    ids.release(id(77));
    assert_eq!(ids.allocate(128), Some(id(77)));
    assert_eq!(ids.allocate(128), None);
}

// M2-103 §5.3: an Initiator "shall not use more than the reported number": the
// limit binds before the IDs run out, and an ID is not reused at once.
#[test]
fn the_pool_stops_at_the_peers_count_and_reuses_an_id_after_the_others() {
    let mut ids = RequestIds::default();
    let first = ids.allocate(2).unwrap();
    let second = ids.allocate(2).unwrap();
    assert_eq!(ids.allocate(2), None);
    ids.release(first);
    let third = ids.allocate(2).unwrap();
    assert!(
        third != first && third != second,
        "the freed ID waits its turn"
    );
    ids.release(second);
    ids.release(third);
    let ids_seen: Vec<_> = (0..130)
        .map(|_| {
            let id = ids.allocate(1).unwrap();
            ids.release(id);
            id.get()
        })
        .collect();
    // All 128 come round, so a late reply to a recycled ID is as unlikely as the pool allows.
    assert_eq!(
        ids_seen
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        128
    );
}

// A request that cannot be built takes no ID, whether the Property Data does not
// encode or the Subscription is not held.
#[test]
fn a_request_that_cannot_be_made_takes_no_id() {
    let (mut i, now) = ready(1);
    let mut call = PeSet::new(name("State"), vec![0xFF]);
    call.encoding = PeEncoding::Ascii;
    assert_eq!(
        i.start(PeCall::Set(call), now),
        Err(PeError::Body(PeBodyError::NotAscii { at: 0 }))
    );
    let unknown = SubscribeId::new("nope").unwrap();
    assert_eq!(
        i.start(PeCall::Unsubscribe(unknown.clone()), now),
        Err(PeError::UnknownSubscription(unknown))
    );
    assert_eq!(i.in_flight(), 0);
    assert!(i.start(get("State"), now).is_ok());
}

// M2-101 §8.3: a message that the peer cannot receive is refused when it is made.
#[test]
fn a_header_the_peer_cannot_receive_is_refused() {
    let (mut i, now) = ready(1);
    let mut call = PeGet::new(name("State"));
    call.res_id = Some(ResourceId::new(&"a".repeat(36)).unwrap());
    call.encoding = PeEncoding::ZlibMcoded7;
    call.flow_control = PeFlowControl::On;
    call.page = Some(PePage {
        offset: u32::MAX,
        limit: std::num::NonZeroU32::new(u32::MAX).unwrap(),
    });
    assert!(matches!(
        i.start(PeCall::Get(call), now),
        Err(PeError::Chunk(PeChunkError::Header(_)))
    ));
    assert_eq!(i.in_flight(), 0);
}

fn subscribe_reply(request: PeRequestId, id_text: &str) -> Vec<(PeMessageKind, PeChunk)> {
    let mut reply = ok(PeStatus::Ok);
    reply.subscribe_id = SubscribeId::new(id_text);
    with_kind(
        reply_chunks(request, reply, b""),
        PeMessageKind::SUBSCRIPTION_REPLY,
    )
}

fn subscribed(i: &mut PeInitiator, now: Instant, resource: &str, sub: &str) {
    let (request, _) = i
        .start(PeCall::Subscribe(PeGet::new(name(resource))), now)
        .unwrap();
    deliver(i, subscribe_reply(request, sub), now);
}

/// A Subscription message of the Responder, its header as given.
fn update_chunks(
    request: u8,
    header: PeSubscription,
    wire: &[u8],
) -> Vec<(PeMessageKind, PeChunk)> {
    split_message(
        id(request),
        &PeHeader::Subscription(header),
        wire,
        MaxSysexSize::of(MAX),
    )
    .unwrap()
    .into_iter()
    .map(|c| (PeMessageKind::SUBSCRIPTION_INQUIRY, c))
    .collect()
}

fn update(command: PeUpdateCommand, sub: &str, encoding: Option<PeEncoding>) -> PeSubscription {
    PeSubscription::Update {
        command,
        subscribe_id: SubscribeId::new(sub).unwrap(),
        mutual_encoding: encoding,
        media_type: None,
        extra: PeExtra::default(),
    }
}

fn reply_status(actions: &[PeAction]) -> (PeRequestId, PeReply) {
    let chunks: Vec<_> = chunks(actions)
        .into_iter()
        .filter(|(kind, _)| *kind == PeMessageKind::SUBSCRIPTION_REPLY)
        .collect();
    let [(_, chunk)] = &chunks[..] else {
        panic!("one reply to the subscription message, got {actions:?}");
    };
    match PeHeader::parse(chunk.header()).unwrap() {
        PeHeader::Reply(reply) => (chunk.request_id(), reply),
        other => panic!("not a reply: {other:?}"),
    }
}

// M2-103 §11.1: Start is the header of a Get with the command `start` first; the
// Responder assigns the Subscription Id in its reply, and the Initiator keeps a
// table of it to the Resource.
#[test]
fn start_subscription_is_a_get_shaped_header_and_the_reply_names_the_id() {
    let (mut i, now) = ready(2);
    let (request, actions) = i
        .start(PeCall::Subscribe(PeGet::new(name("ChannelList"))), now)
        .unwrap();
    let [(kind, chunk)] = &chunks(&actions)[..] else {
        panic!("one chunk");
    };
    assert_eq!(*kind, PeMessageKind::SUBSCRIPTION_INQUIRY);
    assert_eq!(
        chunk.header(),
        br#"{"command":"start","resource":"ChannelList"}"#
    );
    assert_eq!(i.subscriptions().count(), 0);
    let actions = deliver(&mut i, subscribe_reply(request, "sub1"), now);
    let (finished_id, kind, outcome) = one_finished(&actions);
    assert_eq!((finished_id, kind), (request, PeKind::Subscribe));
    assert_eq!(
        success(outcome).header.subscribe_id,
        SubscribeId::new("sub1")
    );
    let table: Vec<_> = i.subscriptions().collect();
    assert_eq!(table.len(), 1);
    assert_eq!(table[0].0, &SubscribeId::new("sub1").unwrap());
    assert_eq!(table[0].1.resource, name("ChannelList"));
}

// §11.1: a reply that starts a Subscription without an id leaves none in the table;
// a failure status leaves none either.
#[test]
fn a_start_that_gives_no_id_or_fails_holds_no_subscription() {
    let (mut i, now) = ready(2);
    let (request, _) = i
        .start(PeCall::Subscribe(PeGet::new(name("X"))), now)
        .unwrap();
    let no_id = with_kind(
        reply_chunks(request, ok(PeStatus::Ok), b""),
        PeMessageKind::SUBSCRIPTION_REPLY,
    );
    let outcome = one_finished(&deliver(&mut i, no_id, now)).2;
    assert_eq!(outcome, PeOutcome::Unreadable(PeUnreadable::NoSubscribeId));
    let (request, _) = i
        .start(PeCall::Subscribe(PeGet::new(name("X"))), now)
        .unwrap();
    let refused = with_kind(
        reply_chunks(request, ok(PeStatus::NotFound), b""),
        PeMessageKind::SUBSCRIPTION_REPLY,
    );
    let outcome = one_finished(&deliver(&mut i, refused, now)).2;
    assert!(matches!(outcome, PeOutcome::Failed(_)));
    assert_eq!(i.subscriptions().count(), 0);
}

// M2-103 §11.1, §11.2: the Responder's `full` and `partial` updates carry Property
// Data in the encoding they name; each is answered with a Reply to Subscription of
// the same Request ID and status 200, and handed out against its Resource.
#[test]
fn a_full_and_a_partial_update_are_answered_and_handed_out() {
    let (mut i, now) = ready(2);
    subscribed(&mut i, now, "ChannelList", "sub1");
    for (command, request) in [
        (PeUpdateCommand::Full, 40u8),
        (PeUpdateCommand::Partial, 41),
    ] {
        let wire = encode_body(PeEncoding::Mcoded7, br#"[{"title":"x"}]"#).unwrap();
        let chunks = update_chunks(
            request,
            update(command, "sub1", Some(PeEncoding::Mcoded7)),
            &wire,
        );
        let actions = deliver(&mut i, chunks, now);
        let (answered, reply) = reply_status(&actions);
        assert_eq!(answered, id(request));
        assert_eq!(reply.status, PeStatus::Ok);
        let notes: Vec<_> = actions
            .iter()
            .filter_map(|a| match a {
                PeAction::Notification(n) => Some(n.clone()),
                _ => None,
            })
            .collect();
        let body = br#"[{"title":"x"}]"#.to_vec();
        let change = if command == PeUpdateCommand::Full {
            PeChange::Full(body)
        } else {
            PeChange::Partial(body)
        };
        assert_eq!(
            notes,
            [PeNotification::Changed {
                subscribe_id: SubscribeId::new("sub1").unwrap(),
                resource: name("ChannelList"),
                change,
            }]
        );
    }
}

// §11.1: `notify` has no Property Data; it asks the Initiator to Get.
#[test]
fn a_notify_update_has_no_body() {
    let (mut i, now) = ready(2);
    subscribed(&mut i, now, "ChannelList", "sub1");
    let chunks = update_chunks(5, update(PeUpdateCommand::Notify, "sub1", None), b"");
    let actions = deliver(&mut i, chunks, now);
    assert_eq!(reply_status(&actions).1.status, PeStatus::Ok);
    assert!(
        actions.contains(&PeAction::Notification(PeNotification::Changed {
            subscribe_id: SubscribeId::new("sub1").unwrap(),
            resource: name("ChannelList"),
            change: PeChange::Notify,
        }))
    );
}

// §11.2: an update the Initiator cannot read is still answered with success, and the
// Initiator Gets the Resource to stay in step: the change says so.
#[test]
fn an_unreadable_update_is_answered_with_success_and_flagged() {
    let (mut i, now) = ready(2);
    subscribed(&mut i, now, "ChannelList", "sub1");
    let chunks = update_chunks(
        6,
        update(PeUpdateCommand::Full, "sub1", Some(PeEncoding::ZlibMcoded7)),
        b"junk",
    );
    let actions = deliver(&mut i, chunks, now);
    assert_eq!(reply_status(&actions).1.status, PeStatus::Ok);
    assert!(actions.iter().any(|a| matches!(
        a,
        PeAction::Notification(PeNotification::Changed {
            change: PeChange::Unreadable(_),
            ..
        })
    )));
}

// §11.2: every Subscription message is answered; one for a Subscription the table
// lacks is answered with a failure status and handed out to nobody.
#[test]
fn an_update_for_an_unknown_subscription_is_refused() {
    let (mut i, now) = ready(2);
    let chunks = update_chunks(9, update(PeUpdateCommand::Full, "ghost", None), b"1");
    let actions = deliver(&mut i, chunks, now);
    let (answered, reply) = reply_status(&actions);
    assert_eq!(answered, id(9));
    assert_eq!(reply.status, PeStatus::BadRequest);
    assert!(
        !actions
            .iter()
            .any(|a| matches!(a, PeAction::Notification(_)))
    );
}

// §11.5, §11.1: the Responder ends a Subscription with `end`; it is answered and
// leaves the table, and an update after it is refused.
#[test]
fn the_responder_ends_a_subscription() {
    let (mut i, now) = ready(2);
    subscribed(&mut i, now, "ChannelList", "sub1");
    subscribed(&mut i, now, "State", "sub2");
    let end = PeSubscription::End {
        subscribe_id: SubscribeId::new("sub1").unwrap(),
        ended_by: Some(PeEndedBy::Responder),
        extra: PeExtra::default(),
    };
    let actions = deliver(&mut i, update_chunks(3, end, b""), now);
    assert_eq!(reply_status(&actions).1.status, PeStatus::Ok);
    assert!(
        actions.contains(&PeAction::Notification(PeNotification::Ended {
            subscribe_id: SubscribeId::new("sub1").unwrap(),
            resource: name("ChannelList"),
        }))
    );
    let table: Vec<_> = i.subscriptions().map(|(id, _)| id.as_str()).collect();
    assert_eq!(table, ["sub2"]);
    let late = update_chunks(4, update(PeUpdateCommand::Full, "sub1", None), b"1");
    assert_eq!(
        reply_status(&deliver(&mut i, late, now)).1.status,
        PeStatus::BadRequest
    );
}

// §11.5, §11.1: the Initiator ends it with `end` and `endedBy` `initiator`, which
// leaves the table at once, and the reply is the end of the transaction.
#[test]
fn the_initiator_ends_a_subscription() {
    let (mut i, now) = ready(2);
    subscribed(&mut i, now, "ChannelList", "sub1");
    let (request, actions) = i
        .start(PeCall::Unsubscribe(SubscribeId::new("sub1").unwrap()), now)
        .unwrap();
    let [(kind, chunk)] = &chunks(&actions)[..] else {
        panic!("one chunk");
    };
    assert_eq!(*kind, PeMessageKind::SUBSCRIPTION_INQUIRY);
    assert_eq!(
        chunk.header(),
        br#"{"command":"end","subscribeId":"sub1","endedBy":"initiator"}"#
    );
    assert_eq!(i.subscriptions().count(), 0);
    let reply = with_kind(
        reply_chunks(request, ok(PeStatus::Ok), b""),
        PeMessageKind::SUBSCRIPTION_REPLY,
    );
    let (finished_id, kind, outcome) = one_finished(&deliver(&mut i, reply, now));
    assert_eq!((finished_id, kind), (request, PeKind::Unsubscribe));
    assert_eq!(success(outcome).status(), PeStatus::Ok);
}

// An ACK or reply from another device, and a message addressed to another, change
// nothing.
#[test]
fn messages_from_another_device_or_to_another_are_not_this_peers() {
    let (mut i, now) = ready(1);
    let (request, _) = i.start(get("State"), now).unwrap();
    let (kind, chunk) = reply_chunks(request, ok(PeStatus::Ok), b"").remove(0);
    for (source, to) in [(OTHER, OURS), (PEER, OTHER)] {
        let message = Decoded::Message(CiMessage::PropertyExchange {
            header: CiHeader::new(Muid::of(source)),
            to: Muid::of(to),
            message: PeCiMessage::Chunk {
                kind,
                chunk: chunk.clone(),
            },
        });
        assert!(i.on_message(&message, now).is_empty());
    }
    assert_eq!(i.in_flight(), 1);
    // A reply to an ID nothing waits on is ignored too.
    let (kind, stray) = reply_chunks(id(100), ok(PeStatus::Ok), b"").remove(0);
    assert!(i.on_message(&chunk_from_peer(kind, stray), now).is_empty());
    assert_eq!(i.in_flight(), 1);
}

// M2-101 §8.2: the Device ID of every Property Exchange message is the Function
// Block, and a message is sent from our MUID to the peer's.
#[test]
fn every_message_is_addressed_to_the_function_block_from_us_to_the_peer() {
    let (mut i, now) = ready(2);
    let (_, actions) = i.start(get("State"), now).unwrap();
    let (_, more) = i.start(PeCall::Set(set(PeFlowControl::Off)), now).unwrap();
    for message in sent(&actions).into_iter().chain(sent(&more)) {
        assert_eq!(message.header().device_id, crate::CiDeviceId::FunctionBlock);
        assert_eq!(message.header().source, Muid::of(OURS));
        assert_eq!(
            message.destination(),
            crate::Destination::Muid(Muid::of(PEER))
        );
    }
}
