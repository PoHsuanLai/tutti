use super::*;
use tutti_fixtures::midi::{jr_timestamp, note_on, timing_clock};
use tutti_midi_types::ci::{CI_DEVICE_ID_FUNCTION_BLOCK, CI_VERSION, CiCategories, Muid};
use tutti_midi_types::ci::{CiHeader, CiMessage, DiscoveryData, ci_to_sysex7, sysex7_to_ci};
use tutti_midi_types::sysex7_fragments;
use tutti_ump::{Message, System, U7};
use tutti_ump::{MidiChannel, MidiGroup};

fn discovery(mfr: [u8; 3]) -> CiMessage {
    CiMessage::Discovery {
        header: CiHeader {
            device_id: CI_DEVICE_ID_FUNCTION_BLOCK,
            ci_version: CI_VERSION,
            source: Muid(0x0123_4567),
            destination: Muid::BROADCAST,
        },
        is_reply: false,
        data: DiscoveryData::new(
            mfr,
            0x1234,
            0x0055,
            [1, 0, 0, 0],
            CiCategories::PROFILE_CONFIGURATION,
            512,
        ),
    }
}

#[test]
fn single_packet_completes_immediately() {
    // A tiny payload fragments to one SINGLE packet.
    let mut packets = Vec::new();
    sysex7_fragments(MidiGroup::FIRST, &[0x01, 0x02, 0x03], &mut packets);
    assert_eq!(packets.len(), 1, "small payload is a single packet");

    let mut r = Sysex7PacketReassembler::new();
    let run = r.push(&packets[0]).expect("single completes");
    assert_eq!(run.len(), 1);
}

#[test]
fn multi_packet_ci_message_round_trips() {
    // A full CI Discovery is larger than 6 bytes → START + CONTINUE… + END.
    let msg = discovery([0x00, 0x21, 0x09]);
    let mut packets = Vec::new();
    ci_to_sysex7(MidiGroup::FIRST, &msg, &mut packets);
    assert!(packets.len() > 1, "CI discovery spans multiple packets");

    let mut r = Sysex7PacketReassembler::new();
    let mut completed = None;
    for p in &packets {
        if let Some(run) = r.push(p) {
            completed = Some(run);
        }
    }
    let run = completed.expect("run completes on the END packet");
    assert_eq!(sysex7_to_ci(&run), Some(msg));
}

#[test]
fn interleaved_groups_do_not_corrupt() {
    let a = discovery([1, 2, 3]);
    let b = discovery([4, 5, 6]);
    let mut pa = Vec::new();
    let mut pb = Vec::new();
    ci_to_sysex7(MidiGroup::FIRST, &a, &mut pa);
    ci_to_sysex7(MidiGroup::of(1), &b, &mut pb);

    // Interleave the two multi-packet runs on groups 0 and 1.
    let mut r = Sysex7PacketReassembler::new();
    let mut got_a = None;
    let mut got_b = None;
    let max = pa.len().max(pb.len());
    for i in 0..max {
        if let Some(p) = pa.get(i)
            && let Some(run) = r.push(p)
        {
            got_a = Some(run);
        }
        if let Some(p) = pb.get(i)
            && let Some(run) = r.push(p)
        {
            got_b = Some(run);
        }
    }
    assert_eq!(sysex7_to_ci(&got_a.expect("a completes")), Some(a));
    assert_eq!(sysex7_to_ci(&got_b.expect("b completes")), Some(b));
}

// NOTE: "a non-SysEx7 event yields nothing" is the weaker half of
// `same_group_traffic_terminates_the_run` below, which pushes the identical
// note-on and asserts both the `None` *and* that the run in flight was
// dropped — the consequence that actually matters.

/// A multi-packet run on `group`, for termination tests.
fn multi_packet_run(group: MidiGroup) -> Vec<Ump> {
    let mut packets = Vec::new();
    ci_to_sysex7(group, &discovery([1, 2, 3]), &mut packets);
    assert!(packets.len() > 2, "need START + CONTINUE + END");
    packets
}

#[test]
fn same_group_traffic_terminates_the_run() {
    // M2-104 §7.7.1: any same-group UMP other than a SysEx Continue or a
    // System Real Time message terminates the message in flight.
    let packets = multi_packet_run(MidiGroup::FIRST);
    let mut r = Sysex7PacketReassembler::new();
    r.push(&packets[0]); // START
    assert_eq!(r.in_flight_count(), 1);

    // A note-on on the same group terminates it.
    assert!(
        r.push(&note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000))
            .is_none()
    );
    assert_eq!(r.in_flight_count(), 0, "run terminated");

    // The END now has no buffer, so nothing corrupt is emitted.
    assert!(r.push(packets.last().unwrap()).is_none());
}

#[test]
fn real_time_and_groupless_and_other_groups_do_not_terminate() {
    let packets = multi_packet_run(MidiGroup::FIRST);
    let mut r = Sysex7PacketReassembler::new();
    for p in &packets[..packets.len() - 1] {
        r.push(p);
    }

    // §7.7.1's explicit carve-outs: System Real Time "may be inserted…in
    // order to maintain timing synchronization", and Groupless messages
    // (MT 0x0 / 0xF) plus other-group traffic "may be interspersed".
    assert!(r.push(&timing_clock(MidiGroup::FIRST)).is_none());
    assert!(r.push(&jr_timestamp(0x1234)).is_none()); // MT 0x0
    assert!(
        r.push(&note_on(MidiGroup::of(3), MidiChannel::FIRST, 60, 0x8000))
            .is_none()
    ); // group 3
    assert_eq!(r.in_flight_count(), 1, "run survives all three");

    // …and the message still completes intact.
    let run = r.push(packets.last().unwrap()).expect("completes");
    assert_eq!(sysex7_to_ci(&run), Some(discovery([1, 2, 3])));
}

#[test]
fn system_common_terminates_but_real_time_does_not() {
    // System Common (0xF1..=0xF6) shares MT 0x1 with Real Time but is not
    // exempt — the status byte decides, not the type nibble.
    let packets = multi_packet_run(MidiGroup::FIRST);
    let mut r = Sysex7PacketReassembler::new();
    r.push(&packets[0]);
    assert!(
        r.push(&Ump::from(Message::System(
            MidiGroup::FIRST,
            System::SongSelect(U7::of(3)),
        )))
        .is_none()
    );
    assert_eq!(r.in_flight_count(), 0, "System Common terminates");
}

#[test]
fn an_unterminated_run_cannot_grow_without_bound() {
    // START + endless CONTINUE is the DoS shape: it never completes, so
    // only a cap stops the buffer growing until the host is out of memory.
    let mut r = Sysex7PacketReassembler::with_max_bytes(64);
    let packets = multi_packet_run(MidiGroup::FIRST);
    r.push(&packets[0]);

    let continue_packet = packets[1];
    for _ in 0..1000 {
        assert!(r.push(&continue_packet).is_none());
    }
    assert_eq!(
        r.in_flight_count(),
        0,
        "the over-long run was abandoned, not buffered"
    );

    // The group resyncs on the next START.
    r.push(&packets[0]);
    assert_eq!(r.in_flight_count(), 1);
}

#[test]
fn a_message_within_the_cap_still_completes() {
    let packets = multi_packet_run(MidiGroup::FIRST);
    // Cap generous enough for this message's payload.
    let mut r = Sysex7PacketReassembler::with_max_bytes(packets.len() * 6);
    let mut completed = None;
    for p in &packets {
        if let Some(run) = r.push(p) {
            completed = Some(run);
        }
    }
    assert_eq!(
        sysex7_to_ci(&completed.expect("completes")),
        Some(discovery([1, 2, 3]))
    );
}
