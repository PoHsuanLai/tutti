use super::*;
use tutti_fixtures::midi::{note_on, timing_clock};
use tutti_midi_types::sysex8_fragments;
use tutti_midi_types::ump::{SYSEX8_BYTES_ABORT, SYSEX8_STATUS_END};
use tutti_ump::{MidiChannel, MidiGroup};

fn payload_of(ev: Option<Sysex8Event>) -> Vec<u8> {
    match ev {
        Some(Sysex8Event::Message { payload, .. }) => payload,
        other => panic!("expected a completed message, got {other:?}"),
    }
}

#[test]
fn single_packet_completes_immediately() {
    let mut out = Vec::new();
    sysex8_fragments(MidiGroup::FIRST, 0x11, &[1, 2, 3], &mut out);
    assert_eq!(out.len(), 1);

    let mut r = Sysex8PacketReassembler::new();
    assert_eq!(payload_of(r.push(&out[0])), vec![1, 2, 3]);
}

#[test]
fn multi_packet_run_reassembles() {
    let data: Vec<u8> = (0u8..30).collect();
    let mut out = Vec::new();
    sysex8_fragments(MidiGroup::FIRST, 0x55, &data, &mut out);
    assert_eq!(out.len(), 3);

    let mut r = Sysex8PacketReassembler::new();
    assert!(r.push(&out[0]).is_none());
    assert!(r.push(&out[1]).is_none());
    assert_eq!(payload_of(r.push(&out[2])), data);
}

#[test]
fn interleaved_streams_on_one_group_stay_separate() {
    // The case the Stream ID exists for, and the one a per-group buffer
    // gets wrong: two messages in flight on group 0 at the same time.
    let a: Vec<u8> = (0u8..30).collect();
    let b: Vec<u8> = (100u8..130).collect();
    let (mut pa, mut pb) = (Vec::new(), Vec::new());
    sysex8_fragments(MidiGroup::FIRST, 0x01, &a, &mut pa);
    sysex8_fragments(MidiGroup::FIRST, 0x02, &b, &mut pb);

    // Start(1) Start(2) Continue(1) Continue(2) End(2) End(1)
    let mut r = Sysex8PacketReassembler::new();
    assert!(r.push(&pa[0]).is_none());
    assert!(r.push(&pb[0]).is_none());
    assert_eq!(r.in_flight_count(), 2, "both streams in flight at once");
    assert!(r.push(&pa[1]).is_none());
    assert!(r.push(&pb[1]).is_none());
    assert_eq!(payload_of(r.push(&pb[2])), b, "stream 2 completes first");
    assert_eq!(payload_of(r.push(&pa[2])), a, "stream 1 unaffected");
}

#[test]
fn unrelated_traffic_does_not_terminate_a_run() {
    // §7.8.1 permits interspersing — unlike SysEx7, this must NOT terminate.
    let data: Vec<u8> = (0u8..30).collect();
    let mut out = Vec::new();
    sysex8_fragments(MidiGroup::FIRST, 0x7F, &data, &mut out);

    let mut r = Sysex8PacketReassembler::new();
    r.push(&out[0]);
    assert!(
        r.push(&note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000))
            .is_none()
    );
    assert_eq!(r.in_flight_count(), 1, "note-on must not terminate SysEx8");
    r.push(&out[1]);
    assert_eq!(payload_of(r.push(&out[2])), data);
}

#[test]
fn sender_abort_is_reported_not_silently_accepted() {
    // M2-104 §7.8: `# of bytes` = 0xF in an End UMP aborts the message. The
    // buffered data is explicitly untrustworthy.
    let data: Vec<u8> = (0u8..30).collect();
    let mut out = Vec::new();
    sysex8_fragments(MidiGroup::FIRST, 0x09, &data, &mut out);

    let mut r = Sysex8PacketReassembler::new();
    r.push(&out[0]);
    r.push(&out[1]);

    // Hand-build the abort: End status with the 0xF byte count.
    let mut abort = out[2];
    abort.0[0] = (abort.0[0] & !0x00FF_0000)
        | ((SYSEX8_STATUS_END as u32) << 20)
        | ((SYSEX8_BYTES_ABORT as u32) << 16);
    assert!(is_sysex8_abort(&abort));

    assert_eq!(
        r.push(&abort),
        Some(Sysex8Event::Aborted {
            stream_id: 0x09,
            reason: Sysex8Abort::SenderAborted,
        })
    );
    assert_eq!(r.in_flight_count(), 0, "buffer discarded");
}

#[test]
fn an_unterminated_run_cannot_grow_without_bound() {
    let data: Vec<u8> = (0u8..30).collect();
    let mut out = Vec::new();
    sysex8_fragments(MidiGroup::FIRST, 0x03, &data, &mut out);

    let mut r = Sysex8PacketReassembler::with_max_bytes(64);
    r.push(&out[0]);
    let cont = out[1];
    let mut aborted = false;
    for _ in 0..1000 {
        if let Some(Sysex8Event::Aborted {
            reason: Sysex8Abort::TooLong,
            ..
        }) = r.push(&cont)
        {
            aborted = true;
            break;
        }
    }
    assert!(aborted, "the over-long run is reported, not buffered");
    assert_eq!(r.in_flight_count(), 0);
}

#[test]
fn non_sysex8_event_is_ignored() {
    let mut r = Sysex8PacketReassembler::new();
    assert!(
        r.push(&note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000))
            .is_none()
    );
    assert!(r.push(&timing_clock(MidiGroup::FIRST)).is_none());
}
