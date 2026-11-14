//! The ACK and NAK of chunked Property Exchange (M2-101 Tables 14 and 16, M2-103
//! §12 and §15). Each test names the property of the spec it holds.

use super::*;

fn id() -> PeRequestId {
    PeRequestId::of(0x2A)
}

fn ack(signal: PeSignal) -> Ack {
    match signal.answer(PeTransaction::Get) {
        PeAnswer::Ack(ack) => ack,
        other => panic!("not an ACK: {other:?}"),
    }
}

fn nak(signal: PeSignal, transaction: PeTransaction) -> Nak {
    match signal.answer(transaction) {
        PeAnswer::Nak(nak) => nak,
        other => panic!("not a NAK: {other:?}"),
    }
}

fn raw(details: &[U7; 5]) -> [u8; 5] {
    details.map(U7::get)
}

// M2-103 §12.2.1, Table 61: ACK 0x10, the status data the wait in tenths of a
// second (0x1E is 3 s, 0x3C 6 s, 0x7F 12.7 s), then the Request ID and the chunk
// number, least significant byte first.
#[test]
fn a_timeout_wait_ack_lays_out_table_61() {
    let wait = ack(PeSignal::TimeoutWait {
        request_id: id(),
        wait: TimeoutWait::of(0x1E),
        chunk: Some(ChunkNo::of(0x81)),
    });
    assert_eq!(wait.ack_sub_id2.get(), 0x34);
    assert_eq!(wait.status_code.get(), 0x10);
    assert_eq!(wait.status_data.get(), 0x1E);
    assert_eq!(raw(&wait.details), [0x2A, 0x01, 0x01, 0, 0]);
    assert_eq!(TimeoutWait::of(0x1E).duration(), Duration::from_secs(3));
    assert_eq!(TimeoutWait::of(0x3C).duration(), Duration::from_secs(6));
    assert_eq!(
        TimeoutWait::of(0x7F).duration(),
        Duration::from_millis(12_700)
    );
    assert_eq!(TimeoutWait::new(0x80), None);
}

// M2-103 §12.2.1, Table 61: a Timeout Wait for a reply not begun has chunk 0.
#[test]
fn a_timeout_wait_without_a_chunk_writes_zero() {
    let wait = ack(PeSignal::TimeoutWait {
        request_id: id(),
        wait: TimeoutWait::of(1),
        chunk: None,
    });
    assert_eq!(raw(&wait.details), [0x2A, 0, 0, 0, 0]);
}

// M2-103 §15.1, Table 94: ACK 0x11, status data 0, the Request ID and the number
// of the chunk received.
#[test]
fn a_flow_control_ack_lays_out_table_94() {
    let next = ack(PeSignal::SendNext {
        request_id: id(),
        chunk: ChunkNo::of(300),
    });
    assert_eq!(next.status_code.get(), 0x11);
    assert_eq!(next.status_data.get(), 0);
    // 300 is 0b10_0101100: low seven bits 0x2C, then 2.
    assert_eq!(raw(&next.details), [0x2A, 0x2C, 0x02, 0, 0]);
}

// M2-103 §15.4, Table 97: NAK 0x12, the Request ID and the last successful chunk.
#[test]
fn a_resend_nak_lays_out_table_97() {
    let resend = nak(
        PeSignal::Resend {
            request_id: id(),
            last_good: Some(ChunkNo::of(7)),
        },
        PeTransaction::Set,
    );
    assert_eq!(resend.nak_sub_id2.get(), 0x36);
    assert_eq!(resend.status_code.get(), 0x12);
    assert_eq!(raw(&resend.details), [0x2A, 7, 0, 0, 0]);
}

// M2-103 §12.1.1, §12.1.2: NAK 0x20, the status data 0 when the Responder ends
// the transaction and 1 when the Initiator does.
#[test]
fn a_terminate_nak_says_who_ends_the_transaction() {
    let by = |by| {
        nak(
            PeSignal::Terminate {
                request_id: id(),
                by,
            },
            PeTransaction::Get,
        )
    };
    assert_eq!(by(PeTerminator::Responder).status_code.get(), 0x20);
    assert_eq!(by(PeTerminator::Responder).status_data.get(), 0);
    assert_eq!(by(PeTerminator::Initiator).status_data.get(), 1);
}

// M2-101 Table 16 and M2-103 §12.2.3: 0x21 out of sequence, 0x41 malformed and
// 0x42 timeout have status data 0.
#[test]
fn the_failure_naks_carry_their_codes() {
    for (signal, code) in [
        (PeSignal::OutOfSequence { request_id: id() }, 0x21),
        (PeSignal::Malformed { request_id: id() }, 0x41),
        (PeSignal::TimedOut { request_id: id() }, 0x42),
    ] {
        let n = nak(signal, PeTransaction::Subscription);
        assert_eq!(n.nak_sub_id2.get(), 0x38);
        assert_eq!(n.status_code.get(), code);
        assert_eq!(n.status_data.get(), 0);
        assert_eq!(raw(&n.details)[0], 0x2A);
    }
}

// The reader holds every writer: the signals read back as they were written.
#[test]
fn every_signal_reads_back_from_its_message() {
    for transaction in [
        PeTransaction::Get,
        PeTransaction::Set,
        PeTransaction::Subscription,
    ] {
        for signal in [
            PeSignal::TimeoutWait {
                request_id: id(),
                wait: TimeoutWait::of(60),
                chunk: Some(ChunkNo::of(2)),
            },
            PeSignal::SendNext {
                request_id: id(),
                chunk: ChunkNo::of(16383),
            },
            PeSignal::Resend {
                request_id: id(),
                last_good: None,
            },
            PeSignal::Terminate {
                request_id: id(),
                by: PeTerminator::Initiator,
            },
            PeSignal::OutOfSequence { request_id: id() },
            PeSignal::Malformed { request_id: id() },
            PeSignal::TimedOut { request_id: id() },
        ] {
            let read = match signal.answer(transaction) {
                PeAnswer::Ack(a) => PeSignal::from_ack(&a),
                PeAnswer::Nak(n) => PeSignal::from_nak(&n),
            };
            assert_eq!(read, Some((transaction, signal)));
        }
    }
}

// M2-101 Tables 14 and 16: a code that is not Property Exchange's, and an ACK of
// another branch, are not read as a signal.
#[test]
fn other_acks_and_naks_are_not_signals() {
    let plain = Ack::new(U7::of(0x34), Ack::STATUS_ACK);
    assert_eq!(PeSignal::from_ack(&plain), None);
    let profile = Ack::new(U7::of(0x20), Ack::STATUS_TIMEOUT_WAIT);
    assert_eq!(PeSignal::from_ack(&profile), None);
    let busy = Nak::new(U7::of(0x34), Nak::STATUS_BUSY);
    assert_eq!(PeSignal::from_nak(&busy), None);
}

// M2-101 §8: the sub-ID#2 of each chunked message, and the inquiry an ACK names.
#[test]
fn the_kinds_are_the_sub_ids_of_section_8() {
    for (sub, kind, transaction, reply) in [
        (0x34, PeMessageKind::GET_INQUIRY, PeTransaction::Get, false),
        (0x35, PeMessageKind::GET_REPLY, PeTransaction::Get, true),
        (0x36, PeMessageKind::SET_INQUIRY, PeTransaction::Set, false),
        (0x37, PeMessageKind::SET_REPLY, PeTransaction::Set, true),
        (
            0x38,
            PeMessageKind::SUBSCRIPTION_INQUIRY,
            PeTransaction::Subscription,
            false,
        ),
        (
            0x39,
            PeMessageKind::SUBSCRIPTION_REPLY,
            PeTransaction::Subscription,
            true,
        ),
    ] {
        assert_eq!(PeMessageKind::from_sub_id2(sub), Some(kind));
        assert_eq!(kind.sub_id2(), sub);
        assert_eq!(kind.transaction, transaction);
        assert_eq!(kind.reply, reply);
    }
    assert_eq!(PeMessageKind::from_sub_id2(0x3F), None);
    assert_eq!(PeMessageKind::from_sub_id2(0x30), None);
}
