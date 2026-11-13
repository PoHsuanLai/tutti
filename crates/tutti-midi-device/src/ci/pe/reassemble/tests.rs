//! Reassembly (M2-101 §8.3, §8.4; M2-103 §5.3, §12, §15). Each test names the
//! property of the spec it holds.

use std::time::Duration;

use super::*;
use crate::MaxSysexSize;
use crate::ci::pe::{PeRequest, ResourceName, split_message};

const KIND: PeMessageKind = PeMessageKind::SET_INQUIRY;
const SECOND: Duration = Duration::from_secs(1);

fn peer(n: u32) -> Muid {
    Muid::new(n).unwrap()
}

fn id(n: u8) -> PeRequestId {
    PeRequestId::of(n)
}

fn limits(streams: u8, bytes: usize) -> PeReassemblyLimits {
    PeReassemblyLimits {
        streams: StreamLimit::new(streams).unwrap(),
        bytes: BufferLimit::new(bytes).unwrap(),
    }
}

fn reassembler() -> PeReassembler {
    PeReassembler::new(limits(8, 1 << 20))
}

fn request() -> PeHeader {
    PeHeader::Request(PeRequest::new(ResourceName::new("State").unwrap()))
}

fn request_bytes() -> Vec<u8> {
    request().encode(MaxSysexSize::of(512)).unwrap()
}

fn chunk(request_id: u8, total: u16, position: u16, data: &[u8]) -> PeChunk {
    let header = if position == 1 {
        request_bytes()
    } else {
        vec![]
    };
    PeChunk::new(
        id(request_id),
        ChunkTotal::from_wire(total).unwrap(),
        ChunkPosition::from_wire(position).unwrap(),
        header,
        data.to_vec(),
    )
    .unwrap()
}

fn body(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 128) as u8).collect()
}

/// Pushes without Flow Control at `now`.
fn push(
    r: &mut PeReassembler,
    kind: PeMessageKind,
    c: PeChunk,
    now: Instant,
) -> Result<PeStep, PeRefused> {
    r.push(peer(1), kind, c, PeFlowControl::Off, now)
}

fn complete(step: Result<PeStep, PeRefused>) -> PeReceived {
    match step {
        Ok(PeStep::Complete { message, .. }) => *message,
        other => panic!("not complete: {other:?}"),
    }
}

fn pending(step: Result<PeStep, PeRefused>) -> Option<PeSignal> {
    match step {
        Ok(PeStep::Pending { send }) => send,
        other => panic!("not pending: {other:?}"),
    }
}

fn refused(step: Result<PeStep, PeRefused>) -> PeRefused {
    step.expect_err("the chunk is refused")
}

// M2-101 §8.3: the chunks of a message, in order, are one message with the header
// of the first and the data of all.
#[test]
fn the_chunks_of_a_message_reassemble_in_order() {
    let now = Instant::now();
    let data = body(1000);
    let chunks = split_message(id(3), &request(), &data, MaxSysexSize::of(128)).unwrap();
    let mut r = reassembler();
    let last = chunks.len() - 1;
    for (i, c) in chunks.into_iter().enumerate() {
        let step = push(&mut r, KIND, c, now);
        if i == last {
            let got = complete(step);
            assert_eq!(got.header, request());
            assert_eq!(got.body, data);
            assert_eq!((got.peer, got.kind, got.request_id), (peer(1), KIND, id(3)));
        } else {
            assert_eq!(pending(step), None);
        }
    }
    assert_eq!(r.buffered(), PeBuffered::default());
}

// M2-101 §8.3.2: a message with no Property Data is one chunk, 1 of 1.
#[test]
fn a_header_only_message_is_complete_at_once() {
    let mut r = reassembler();
    let got = complete(push(&mut r, KIND, chunk(1, 1, 1, &[]), Instant::now()));
    assert!(got.body.is_empty());
    assert_eq!(r.buffered(), PeBuffered::default());
}

// M2-101 §8.3, Table 29, "Unknown Number": the count is 0 until the final chunk
// declares it equal to its own number.
#[test]
fn an_unknown_count_ends_at_the_chunk_that_names_it() {
    let now = Instant::now();
    let mut r = reassembler();
    assert_eq!(
        pending(push(&mut r, KIND, chunk(1, 0, 1, b"ab"), now)),
        None
    );
    assert_eq!(
        pending(push(&mut r, KIND, chunk(1, 0, 2, b"cd"), now)),
        None
    );
    let got = complete(push(&mut r, KIND, chunk(1, 3, 3, b"ef"), now));
    assert_eq!(got.body, b"abcdef");
}

// M2-101 §8.3, Table 29, "Unexpected End Before Chunk 6, Property Data Remains
// Good": a message that declared 6 ends at chunk 3 by naming 3.
#[test]
fn a_known_count_may_end_early_by_naming_a_smaller_one() {
    let now = Instant::now();
    let mut r = reassembler();
    pending(push(&mut r, KIND, chunk(1, 6, 1, b"ab"), now));
    pending(push(&mut r, KIND, chunk(1, 6, 2, b"cd"), now));
    let got = complete(push(&mut r, KIND, chunk(1, 3, 3, b"ef"), now));
    assert_eq!(got.body, b"abcdef");
}

// M2-101 §8.3, Table 29, "Property Data is Bad": a final chunk numbered 0, for a
// declared count and for an unknown one, abandons the message and its data.
#[test]
fn a_final_chunk_numbered_zero_abandons_the_message() {
    for total in [6, 0] {
        let now = Instant::now();
        let mut r = reassembler();
        pending(push(&mut r, KIND, chunk(1, total, 1, b"ab"), now));
        let step = push(&mut r, KIND, chunk(1, total, 0, b"cd"), now);
        assert_eq!(step, Ok(PeStep::Abandoned));
        assert_eq!(r.buffered(), PeBuffered::default());
        // The Request ID is free: a new message with it starts.
        pending(push(&mut r, KIND, chunk(1, 2, 1, b"xy"), now));
    }
}

// M2-101 §8.3: a chunk numbered 0 ends a message that was started.
#[test]
fn an_abandoning_chunk_with_no_message_is_refused() {
    let mut r = reassembler();
    let e = refused(push(&mut r, KIND, chunk(1, 2, 0, b""), Instant::now()));
    assert_eq!(e.error, PeReassemblyError::NoMessage(id(1)));
}

// M2-101 §8.3: the count of Number of This Chunk starts from 1, so a message
// begins at its first chunk; a later one with no message before it is out of
// sequence (NAK 0x21).
#[test]
fn a_later_chunk_with_no_first_is_out_of_sequence() {
    let mut r = reassembler();
    let e = refused(push(&mut r, KIND, chunk(1, 3, 2, b"ab"), Instant::now()));
    assert_eq!(e.error, PeReassemblyError::NoMessage(id(1)));
    assert_eq!(e.send, Some(PeSignal::OutOfSequence { request_id: id(1) }));
}

// M2-101 §8.3: the chunks are numbered in sequence; a skipped number ends the
// message, which cannot be recovered (Table 16, NAK 0x21).
#[test]
fn a_skipped_chunk_ends_the_message() {
    let now = Instant::now();
    let mut r = reassembler();
    pending(push(&mut r, KIND, chunk(1, 4, 1, b"ab"), now));
    let e = refused(push(&mut r, KIND, chunk(1, 4, 3, b"ef"), now));
    assert_eq!(
        e.error,
        PeReassemblyError::OutOfSequence {
            expected: ChunkNo::of(2),
            got: ChunkNo::of(3)
        }
    );
    assert_eq!(e.send, Some(PeSignal::OutOfSequence { request_id: id(1) }));
    assert_eq!(r.buffered(), PeBuffered::default());
}

// M2-101 §8.3: a number lower than the last is out of sequence as well.
#[test]
fn an_earlier_chunk_than_the_last_ends_the_message() {
    let now = Instant::now();
    let mut r = reassembler();
    for n in 1..=3 {
        pending(push(&mut r, KIND, chunk(1, 5, n, b"ab"), now));
    }
    let e = refused(push(&mut r, KIND, chunk(1, 5, 2, b"ab"), now));
    assert!(matches!(e.error, PeReassemblyError::OutOfSequence { .. }));
    assert_eq!(r.buffered(), PeBuffered::default());
}

// M2-103 §15.5: a Sender repeats a chunk whose ACK it did not get, so a repeat of
// the last chunk is taken once and, under Flow Control, acknowledged again.
#[test]
fn a_repeat_of_the_last_chunk_is_taken_once() {
    let now = Instant::now();
    let mut r = reassembler();
    pending(push(&mut r, KIND, chunk(1, 3, 1, b"ab"), now));
    pending(push(&mut r, KIND, chunk(1, 3, 2, b"cd"), now));
    assert_eq!(
        pending(push(&mut r, KIND, chunk(1, 3, 2, b"cd"), now)),
        None
    );
    let got = complete(push(&mut r, KIND, chunk(1, 3, 3, b"ef"), now));
    assert_eq!(got.body, b"abcdef");
}

// M2-103 §15.5: the same, with the ACK sent again.
#[test]
fn a_repeated_chunk_is_acknowledged_again_under_flow_control() {
    let now = Instant::now();
    let mut r = reassembler();
    let on = |r: &mut PeReassembler, c| r.push(peer(1), KIND, c, PeFlowControl::On, now);
    pending(on(&mut r, chunk(1, 3, 1, b"ab")));
    let again = pending(on(&mut r, chunk(1, 3, 1, b"ab")));
    assert_eq!(
        again,
        Some(PeSignal::SendNext {
            request_id: id(1),
            chunk: ChunkNo::FIRST
        })
    );
    let got = complete({
        pending(on(&mut r, chunk(1, 3, 2, b"cd")));
        on(&mut r, chunk(1, 3, 3, b"ef"))
    });
    assert_eq!(got.body, b"abcdef");
}

// M2-103 §5.3: an ID is reused only after its transaction is complete, so a
// first chunk for an ID in use is refused, and the message in progress goes on.
#[test]
fn a_first_chunk_for_an_id_in_use_is_refused() {
    let now = Instant::now();
    let mut r = reassembler();
    pending(push(&mut r, KIND, chunk(1, 3, 1, b"ab"), now));
    pending(push(&mut r, KIND, chunk(1, 3, 2, b"cd"), now));
    let e = refused(push(&mut r, KIND, chunk(1, 2, 1, b"xy"), now));
    assert_eq!(e.error, PeReassemblyError::RequestIdInUse(id(1)));
    let got = complete(push(&mut r, KIND, chunk(1, 3, 3, b"ef"), now));
    assert_eq!(got.body, b"abcdef");
}

// M2-103 §5.3: Request IDs may be reused after a Transaction is complete.
#[test]
fn an_id_is_free_again_when_its_message_is_complete() {
    let now = Instant::now();
    let mut r = reassembler();
    complete(push(&mut r, KIND, chunk(1, 1, 1, b"ab"), now));
    let got = complete(push(&mut r, KIND, chunk(1, 1, 1, b"cd"), now));
    assert_eq!(got.body, b"cd");
}

// M2-103 §5.3: an ID is unique only to the pair of MUIDs, so the same ID from two
// peers is two messages; M2-101 §8.4: and an inquiry and a reply of one ID are two.
#[test]
fn an_id_is_unique_to_a_peer_and_a_direction() {
    let now = Instant::now();
    let mut r = reassembler();
    let on = |r: &mut PeReassembler, p, kind, c| r.push(peer(p), kind, c, PeFlowControl::Off, now);
    pending(on(&mut r, 1, KIND, chunk(1, 2, 1, b"aa")));
    pending(on(&mut r, 2, KIND, chunk(1, 2, 1, b"bb")));
    pending(on(
        &mut r,
        1,
        PeMessageKind::SET_REPLY,
        chunk(1, 2, 1, b"cc"),
    ));
    assert_eq!(r.buffered().streams, 3);
    assert_eq!(
        complete(on(&mut r, 2, KIND, chunk(1, 2, 2, b"yy"))).body,
        b"bbyy"
    );
    assert_eq!(
        complete(on(&mut r, 1, KIND, chunk(1, 2, 2, b"xx"))).body,
        b"aaxx"
    );
    assert_eq!(
        complete(on(
            &mut r,
            1,
            PeMessageKind::SET_REPLY,
            chunk(1, 2, 2, b"zz")
        ))
        .body,
        b"cczz"
    );
}

// M2-101 §8.3: a count that changes before the end, one smaller than the chunk's
// number, or a known count on a message that began unknown, is not a message.
#[test]
fn a_count_that_disagrees_ends_the_message() {
    let now = Instant::now();
    let cases: [(u16, u16, u16); 3] = [(6, 5, 2), (6, 1, 2), (0, 4, 2)];
    for (declared, then, number) in cases {
        let mut r = reassembler();
        pending(push(&mut r, KIND, chunk(1, declared, 1, b"ab"), now));
        let e = refused(push(&mut r, KIND, chunk(1, then, number, b"cd"), now));
        assert_eq!(
            e.error,
            PeReassemblyError::CountMismatch,
            "{declared} {then}"
        );
        assert_eq!(r.buffered(), PeBuffered::default());
    }
    // An unknown count mid-way through a message that declared one.
    let mut r = reassembler();
    pending(push(&mut r, KIND, chunk(1, 6, 1, b"ab"), now));
    let e = refused(push(&mut r, KIND, chunk(1, 0, 2, b"cd"), now));
    assert_eq!(e.error, PeReassemblyError::CountMismatch);
}

// M2-103 §12.2: the time between consecutive chunks is under 3 seconds, so the
// message times out at 3 seconds and not a nanosecond before, and the NAK is 0x42.
#[test]
fn a_message_times_out_after_three_seconds_from_its_last_chunk() {
    let t0 = Instant::now();
    let mut r = reassembler();
    pending(push(&mut r, KIND, chunk(1, 3, 1, b"ab"), t0));
    assert_eq!(r.next_deadline(), Some(t0 + 3 * SECOND));
    // A chunk 2.9 s later moves the deadline to 3 s after it.
    pending(push(
        &mut r,
        KIND,
        chunk(1, 3, 2, b"cd"),
        t0 + 2900 * Duration::from_millis(1),
    ));
    assert!(r.expire(t0 + 3 * SECOND).is_empty());
    let deadline = t0 + Duration::from_millis(5900);
    assert_eq!(r.next_deadline(), Some(deadline));
    assert!(r.expire(deadline - Duration::from_nanos(1)).is_empty());
    assert_eq!(
        r.expire(deadline),
        vec![PeExpired {
            peer: peer(1),
            kind: KIND,
            send: PeSignal::TimedOut { request_id: id(1) },
        }]
    );
    assert_eq!(r.buffered(), PeBuffered::default());
    assert_eq!(r.next_deadline(), None);
}

// M2-103 §12.2: a chunk that arrives at the deadline is late; the message has
// ended and its ID starts a new one.
#[test]
fn a_chunk_at_the_deadline_is_late() {
    let t0 = Instant::now();
    let mut r = reassembler();
    pending(push(&mut r, KIND, chunk(1, 3, 1, b"ab"), t0));
    let at = t0 + 3 * SECOND - Duration::from_nanos(1);
    pending(push(&mut r, KIND, chunk(1, 3, 2, b"cd"), at));
    let mut r = reassembler();
    pending(push(&mut r, KIND, chunk(1, 3, 1, b"ab"), t0));
    let e = refused(push(&mut r, KIND, chunk(1, 3, 2, b"cd"), t0 + 3 * SECOND));
    assert_eq!(e.error, PeReassemblyError::NoMessage(id(1)));
}

// M2-103 §5.3: a request that times out frees its ID.
#[test]
fn an_id_is_free_again_after_a_timeout() {
    let t0 = Instant::now();
    let mut r = reassembler();
    pending(push(&mut r, KIND, chunk(1, 3, 1, b"ab"), t0));
    assert_eq!(r.expire(t0 + 3 * SECOND).len(), 1);
    pending(push(&mut r, KIND, chunk(1, 2, 1, b"xy"), t0 + 3 * SECOND));
}

// M2-103 §12.2.1, Example 1: a wait of 0x3C (6 s) is added to the 3 s in hand, so
// the message times out at 9 s.
#[test]
fn a_timeout_wait_extends_the_deadline_by_the_wait() {
    let t0 = Instant::now();
    let mut r = reassembler();
    pending(push(&mut r, KIND, chunk(1, 3, 1, b"ab"), t0));
    assert!(r.extend(peer(1), KIND, id(1), TimeoutWait::of(0x3C)));
    assert!(
        r.expire(t0 + 9 * SECOND - Duration::from_nanos(1))
            .is_empty()
    );
    assert_eq!(r.expire(t0 + 9 * SECOND).len(), 1);
    assert!(!r.extend(peer(1), KIND, id(1), TimeoutWait::of(1)));
}

// M2-103 §12.1: a Terminate NAK concludes the transaction at once.
#[test]
fn a_terminate_drops_the_message() {
    let now = Instant::now();
    let mut r = reassembler();
    pending(push(&mut r, KIND, chunk(1, 3, 1, b"ab"), now));
    assert!(r.terminate(peer(1), KIND, id(1)));
    assert!(!r.terminate(peer(1), KIND, id(1)));
    assert_eq!(r.buffered(), PeBuffered::default());
}

// M2-101 §8.3 (and §5.5.3's `Number of Simultaneous` of M2-103 §5.3): the messages
// held at once are capped, and the one past the cap is refused with the others kept.
#[test]
fn a_message_past_the_stream_limit_is_refused() {
    let now = Instant::now();
    let mut r = PeReassembler::new(limits(2, 1 << 20));
    pending(push(&mut r, KIND, chunk(1, 2, 1, b"aa"), now));
    pending(push(&mut r, KIND, chunk(2, 2, 1, b"bb"), now));
    let e = refused(push(&mut r, KIND, chunk(3, 2, 1, b"cc"), now));
    assert_eq!(
        e.error,
        PeReassemblyError::TooManyStreams(StreamLimit::new(2).unwrap())
    );
    assert_eq!(r.buffered().streams, 2);
    complete(push(&mut r, KIND, chunk(1, 2, 2, b"AA"), now));
    pending(push(&mut r, KIND, chunk(3, 2, 1, b"cc"), now));
}

#[test]
fn the_limits_are_ranges() {
    assert_eq!(StreamLimit::new(0), None);
    assert_eq!(StreamLimit::new(128).map(StreamLimit::get), Some(128));
    assert_eq!(StreamLimit::new(129), None);
    assert_eq!(BufferLimit::new(0), None);
}

// The buffer is capped over Header Data and Property Data across all messages;
// one byte over drops the message and tells the sender to stop (NAK 0x20).
#[test]
fn the_buffer_cap_counts_headers_and_data_and_is_exact() {
    let now = Instant::now();
    let header = request_bytes().len();
    let cap = header + 4;
    let mut r = PeReassembler::new(limits(8, cap));
    pending(push(&mut r, KIND, chunk(1, 3, 1, b"abcd"), now));
    assert_eq!(
        r.buffered(),
        PeBuffered {
            streams: 1,
            bytes: cap
        }
    );
    let e = refused(push(&mut r, KIND, chunk(1, 3, 2, b"e"), now));
    assert_eq!(
        e.error,
        PeReassemblyError::Overflow(BufferLimit::new(cap).unwrap())
    );
    assert_eq!(
        e.send,
        Some(PeSignal::Terminate {
            request_id: id(1),
            by: PeTerminator::Responder
        })
    );
    assert_eq!(r.buffered(), PeBuffered::default());
    // A reply's receiver is the Initiator.
    let reply = PeMessageKind::GET_REPLY;
    pending(push(&mut r, reply, chunk(1, 3, 1, b"abcd"), now));
    let e = refused(push(&mut r, reply, chunk(1, 3, 2, b"e"), now));
    assert_eq!(
        e.send,
        Some(PeSignal::Terminate {
            request_id: id(1),
            by: PeTerminator::Initiator
        })
    );
    // Exactly at the cap, a message completes.
    let mut r = PeReassembler::new(limits(8, header + 4));
    complete(push(&mut r, KIND, chunk(1, 1, 1, b"abcd"), now));
}

// The cap is across messages: the bytes of one count against the room of another.
#[test]
fn the_buffer_cap_is_shared_by_all_messages() {
    let now = Instant::now();
    let header = request_bytes().len();
    let mut r = PeReassembler::new(limits(8, 2 * header + 5));
    pending(push(&mut r, KIND, chunk(1, 2, 1, b"abc"), now));
    let e = refused(push(&mut r, KIND, chunk(2, 2, 1, b"abc"), now));
    assert!(matches!(e.error, PeReassemblyError::Overflow(_)));
    assert_eq!(r.buffered().streams, 1);
}

fn flow_request(flow: bool) -> Vec<u8> {
    let mut request = PeRequest::new(ResourceName::new("State").unwrap());
    request.flow_control = flow;
    PeHeader::Request(request)
        .encode(MaxSysexSize::of(512))
        .unwrap()
}

fn with_header(header: Vec<u8>, total: u16, position: u16, data: &[u8]) -> PeChunk {
    PeChunk::new(
        id(1),
        ChunkTotal::from_wire(total).unwrap(),
        ChunkPosition::from_wire(position).unwrap(),
        if position == 1 { header } else { vec![] },
        data.to_vec(),
    )
    .unwrap()
}

fn next(chunk: u16) -> Option<PeSignal> {
    Some(PeSignal::SendNext {
        request_id: id(1),
        chunk: ChunkNo::of(chunk),
    })
}

// M2-103 §15.3: a Set whose header has `flowControl` true has each chunk
// acknowledged with ACK 0x11, and the last is not: the Reply to Set is its ACK.
#[test]
fn flow_control_acknowledges_each_chunk_of_a_set_but_the_last() {
    let now = Instant::now();
    let mut r = reassembler();
    let header = flow_request(true);
    let first = with_header(header.clone(), 3, 1, b"ab");
    assert_eq!(pending(push(&mut r, KIND, first, now)), next(1));
    assert_eq!(
        pending(push(
            &mut r,
            KIND,
            with_header(header.clone(), 3, 2, b"cd"),
            now
        )),
        next(2)
    );
    match push(&mut r, KIND, with_header(header, 3, 3, b"ef"), now) {
        Ok(PeStep::Complete { send, .. }) => assert_eq!(send, None),
        other => panic!("{other:?}"),
    }
}

// M2-103 §15.2: a Get reply under Flow Control is acknowledged on every chunk,
// the last included (the Initiator's ACK says all chunks arrived). The reply's
// header does not say, so the caller does.
#[test]
fn flow_control_acknowledges_every_chunk_of_a_get_reply() {
    let now = Instant::now();
    let mut r = reassembler();
    let kind = PeMessageKind::GET_REPLY;
    let reply = PeHeader::Reply(crate::PeReply::new(crate::PeStatus::Ok))
        .encode(MaxSysexSize::of(512))
        .unwrap();
    let on = |r: &mut PeReassembler, c| r.push(peer(1), kind, c, PeFlowControl::On, now);
    assert_eq!(
        pending(on(&mut r, with_header(reply.clone(), 2, 1, b"ab"))),
        next(1)
    );
    match on(&mut r, with_header(reply, 2, 2, b"cd")) {
        Ok(PeStep::Complete { send, message }) => {
            assert_eq!(send, next(2));
            assert_eq!(message.body, b"abcd");
        }
        other => panic!("{other:?}"),
    }
}

// M2-103 §15.2: without `flowControl` the Responder sends the data with no ACK
// between chunks, so none is sent.
#[test]
fn no_flow_control_means_no_acknowledgement() {
    let now = Instant::now();
    let mut r = reassembler();
    let header = flow_request(false);
    assert_eq!(
        pending(push(&mut r, KIND, with_header(header, 2, 1, b"ab"), now)),
        None
    );
}

// M2-103 §15.4: a receiver that cannot use a chunk asks for the most recent one
// again, naming the last it received intact.
#[test]
fn a_resend_request_names_the_last_good_chunk() {
    let now = Instant::now();
    let mut r = reassembler();
    assert_eq!(r.resend_request(peer(1), KIND, id(1)), None);
    pending(push(&mut r, KIND, chunk(1, 4, 1, b"ab"), now));
    pending(push(&mut r, KIND, chunk(1, 4, 2, b"cd"), now));
    assert_eq!(
        r.resend_request(peer(1), KIND, id(1)),
        Some(PeSignal::Resend {
            request_id: id(1),
            last_good: Some(ChunkNo::of(2))
        })
    );
}

// M2-103 §7: the first chunk's header is read; one that is not a header
// ends the message with NAK 0x41, and nothing is buffered.
#[test]
fn a_first_chunk_with_a_bad_header_is_malformed() {
    let mut r = reassembler();
    let bad = PeChunk::new(
        id(1),
        ChunkTotal::Known(ChunkNo::of(2)),
        ChunkPosition::Number(ChunkNo::FIRST),
        b"{oops".to_vec(),
        b"ab".to_vec(),
    )
    .unwrap();
    let e = refused(push(&mut r, KIND, bad, Instant::now()));
    assert!(matches!(e.error, PeReassemblyError::Header(_)));
    assert_eq!(e.send, Some(PeSignal::Malformed { request_id: id(1) }));
    assert_eq!(r.buffered(), PeBuffered::default());
}

#[cfg(feature = "serde")]
#[test]
fn the_stream_and_buffer_limits_are_range_checked_values() {
    use tutti_fixtures::{RangeContract, check_range_contract};
    check_range_contract(&RangeContract {
        new: StreamLimit::new,
        get: StreamLimit::get,
        of: None,
        valid: &[1, 128],
        invalid: &[0, 129, 255],
    });
    check_range_contract(&RangeContract {
        new: BufferLimit::new,
        get: BufferLimit::get,
        of: None,
        valid: &[1, usize::MAX],
        invalid: &[0],
    });
}
