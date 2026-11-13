//! System Exclusive 8 (M2-104 section 7.8): the packet shapes of Table 19, the
//! Stream ID's independence, section 7.8.1's abort and no-prohibition note,
//! and the assembler's drop counting, checked against packets built from the
//! spec's fields as well as against the writer.

use tutti_ump::{
    Data128, Message, MidiChannel, MidiGroup, Sysex7Packet, Sysex8Assembler, Sysex8Message,
    Sysex8Step, SysexStatus, Ump, Voice1,
};

const G0: MidiGroup = MidiGroup::of(0);

fn assembler(lanes: usize) -> Sysex8Assembler<Vec<u8>> {
    Sysex8Assembler::new(vec![0; 64 * lanes], 64, lanes)
}

/// A packet from the spec's bit positions (Figure 85): message type 5, group,
/// status in 23-20, `# of bytes` in 19-16, stream id in byte 3, data after.
fn raw(group: u32, status: u32, count: u32, stream: u32, data: &[u8]) -> Ump {
    let mut bytes = [0u8; 13];
    bytes[..data.len()].copy_from_slice(data);
    let w = |i: usize| u32::from_be_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
    Ump([
        0x5000_0000
            | (group << 24)
            | (status << 20)
            | (count << 16)
            | (stream << 8)
            | u32::from(bytes[0]),
        w(1),
        w(5),
        w(9),
    ])
}

const COMPLETE: u32 = 0;
const START: u32 = 1;
const CONTINUE: u32 = 2;
const END: u32 = 3;

/// A packet with the right `# of bytes` for `data`.
fn pkt(status: u32, stream: u32, data: &[u8]) -> Ump {
    raw(0, status, data.len() as u32 + 1, stream, data)
}

/// The messages a stream of UMPs completes.
fn feed(
    a: &mut Sysex8Assembler<Vec<u8>>,
    umps: impl IntoIterator<Item = Ump>,
) -> Vec<(u8, u8, Vec<u8>)> {
    umps.into_iter()
        .filter_map(|u| match a.push(u) {
            Sysex8Step::Complete(m) => Some((m.group.get(), m.stream_id, m.payload.to_vec())),
            _ => None,
        })
        .collect()
}

fn payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| 0x80 | i as u8).collect()
}

/// Section 7.8, Table 19: a payload that fits one UMP (13 bytes after the
/// Stream ID) is a Complete; otherwise a Start, Continues and an End, 13 bytes
/// each but the last. The Stream ID is in every packet, and `# of bytes`
/// counts it.
#[test]
fn packets_follow_table_19_at_boundary_sizes() {
    // (payload length, statuses, `# of bytes` of each packet)
    let cases: [(usize, &[u32], &[u32]); 8] = [
        (0, &[COMPLETE], &[1]),
        (1, &[COMPLETE], &[2]),
        (13, &[COMPLETE], &[14]),
        (14, &[START, END], &[14, 2]),
        (26, &[START, END], &[14, 14]),
        (27, &[START, CONTINUE, END], &[14, 14, 2]),
        (39, &[START, CONTINUE, END], &[14, 14, 14]),
        (40, &[START, CONTINUE, CONTINUE, END], &[14, 14, 14, 2]),
    ];
    for (len, statuses, counts) in cases {
        let data = payload(len);
        let message = Sysex8Message {
            group: MidiGroup::of(3),
            stream_id: 9,
            payload: &data,
        };
        let umps: Vec<Ump> = message.packets().collect();
        assert_eq!(message.packets().len(), umps.len(), "{len}");
        let words: Vec<(u32, u32, u32)> = umps
            .iter()
            .map(|u| {
                let w = u.0[0];
                ((w >> 20) & 0xF, (w >> 16) & 0xF, (w >> 8) & 0xFF)
            })
            .collect();
        let want: Vec<(u32, u32, u32)> = statuses
            .iter()
            .zip(counts)
            .map(|(&s, &c)| (s, c, 9))
            .collect();
        assert_eq!(words, want, "{len}");
        assert!(umps.iter().all(|u| u.0[0] >> 24 == 0x53), "{len}");
    }
}

/// Split then assemble gives the original at every boundary size, whatever the
/// byte values: SysEx8 data is 8-bit.
#[test]
fn a_written_message_reads_back_whole() {
    let mut a = assembler(1);
    for len in [0, 1, 12, 13, 14, 15, 26, 27, 28, 63, 64] {
        let data = payload(len);
        let message = Sysex8Message {
            group: MidiGroup::of(5),
            stream_id: 200,
            payload: &data,
        };
        assert_eq!(feed(&mut a, message.packets()), [(5, 200, data)], "{len}");
    }
    assert_eq!(a.dropped(), 0);
}

/// Section 7.8: Start and Continue with fewer than 14 bytes are not a message
/// end, and a Start or Continue with only the Stream ID is allowed.
#[test]
fn a_short_start_or_continue_does_not_end_the_message() {
    let mut a = assembler(1);
    let done = feed(
        &mut a,
        [
            pkt(START, 0, &[1, 2, 3]),
            pkt(CONTINUE, 0, &[]),
            pkt(CONTINUE, 0, &[4]),
            pkt(END, 0, &[5]),
        ],
    );
    assert_eq!(done, [(0, 0, vec![1, 2, 3, 4, 5])]);
}

/// Section 7.8.1: an End whose `# of bytes` is 1 (the Stream ID alone, data
/// zero) closes a message whose data is valid.
#[test]
fn an_end_of_one_byte_closes_a_valid_message() {
    let mut a = assembler(1);
    let done = feed(&mut a, [pkt(START, 0, &[9; 13]), raw(0, END, 1, 0, &[])]);
    assert_eq!(done, [(0, 0, vec![9; 13])]);
    assert_eq!(a.dropped(), 0);
}

/// Section 7.8.1: an End with `# of bytes` 0xF aborts the stream's message:
/// nothing is delivered and it is counted; the stream is usable after.
#[test]
fn an_end_of_fifteen_aborts_the_message_and_counts_it() {
    let mut a = assembler(1);
    let done = feed(
        &mut a,
        [
            pkt(START, 4, &[1; 13]),
            pkt(CONTINUE, 4, &[2; 13]),
            raw(0, END, 0xF, 4, &[]),
        ],
    );
    assert!(done.is_empty());
    assert_eq!(a.dropped(), 1);
    assert_eq!(feed(&mut a, [pkt(COMPLETE, 4, &[7])]), [(0, 4, vec![7])]);
    // An abort of a stream with nothing open has nothing to drop.
    feed(&mut a, [raw(0, END, 0xF, 4, &[])]);
    assert_eq!(a.dropped(), 1);
}

/// Section 7.8: `# of bytes` is 1 to 14 (the Stream ID is mandatory), 0xF only
/// in an End. A packet that breaks it is no packet: an open message goes on.
#[test]
fn a_packet_with_an_invalid_byte_count_is_ignored() {
    let mut a = assembler(1);
    let mut umps = vec![pkt(START, 0, &[1])];
    umps.push(raw(0, CONTINUE, 0, 0, &[]));
    umps.push(raw(0, CONTINUE, 0xF, 0, &[]));
    umps.push(raw(0, COMPLETE, 0, 0, &[]));
    umps.push(pkt(END, 0, &[2]));
    assert_eq!(feed(&mut a, umps), [(0, 0, vec![1, 2])]);
    assert_eq!(a.dropped(), 0);
}

/// Section 7.8, Stream id: streams of one Group interleave, each its own
/// message; so do Groups, with the same Stream ID.
#[test]
fn streams_and_groups_are_independent() {
    let mut a = assembler(4);
    let at = |group, status, stream, data: &[u8]| {
        raw(group, status, data.len() as u32 + 1, stream, data)
    };
    let done = feed(
        &mut a,
        [
            at(0, START, 0, &[1]),
            at(0, START, 1, &[10]),
            at(1, START, 0, &[100]),
            at(0, CONTINUE, 1, &[11]),
            at(0, CONTINUE, 0, &[2]),
            at(1, END, 0, &[101]),
            at(0, END, 0, &[3]),
            at(0, END, 1, &[12]),
        ],
    );
    assert_eq!(
        done,
        [
            (1, 0, vec![100, 101]),
            (0, 0, vec![1, 2, 3]),
            (0, 1, vec![10, 11, 12])
        ]
    );
    assert_eq!(a.dropped(), 0);
}

/// Section 7.8.1, note: SysEx8 may be interspersed with other UMPs, unlike
/// SysEx7 (section 7.7.1): a voice message, a SysEx7 packet and a Mixed Data Set
/// header of the same Group inside a message end nothing.
#[test]
fn other_messages_inside_a_message_end_nothing() {
    let mut a = assembler(1);
    let note = Ump::from(Message::Midi1(
        tutti_ump::Address {
            group: G0,
            channel: MidiChannel::of(0),
        },
        Voice1::NoteOn {
            note: tutti_ump::NoteNumber::of(60),
            velocity: tutti_ump::U7::of(64),
        },
    ));
    let sx7 = Ump::from(Message::Sysex7(
        G0,
        Sysex7Packet::new(SysexStatus::Complete, &[1]).unwrap(),
    ));
    let mds = Ump([0x5081_0010, 1, 0x0041_0000, 0]);
    let mut done = Vec::new();
    for ump in [pkt(START, 0, &[1]), note, sx7, mds, pkt(END, 0, &[2])] {
        match a.push(ump) {
            Sysex8Step::Complete(m) => done.push(m.payload.to_vec()),
            Sysex8Step::NotSysex8 | Sysex8Step::Held => {}
        }
    }
    assert_eq!(done, [vec![1, 2]]);
    assert_eq!(a.dropped(), 0);
}

/// Section 7.8: a message is a Complete, or a Start through an End. A Start
/// while the stream is open drops the open message, counted, and begins anew.
#[test]
fn a_start_inside_a_message_drops_it() {
    let mut a = assembler(1);
    let done = feed(
        &mut a,
        [pkt(START, 0, &[1]), pkt(START, 0, &[2]), pkt(END, 0, &[3])],
    );
    assert_eq!(done, [(0, 0, vec![2, 3])]);
    assert_eq!(a.dropped(), 1);
}

/// A Complete inside an open message on its stream drops the open one,
/// counted, and is itself delivered.
#[test]
fn a_complete_inside_a_message_drops_it() {
    let mut a = assembler(1);
    let done = feed(&mut a, [pkt(START, 0, &[1]), pkt(COMPLETE, 0, &[2])]);
    assert_eq!(done, [(0, 0, vec![2])]);
    assert_eq!(a.dropped(), 1);
    // And nothing is left open: an End now is an orphan.
    assert!(feed(&mut a, [pkt(END, 0, &[3])]).is_empty());
    assert_eq!(a.dropped(), 2);
}

/// Section 7.8: a message begins with a Start. A Continue or End with none
/// open is dropped; a message's orphan packets are counted once.
#[test]
fn packets_with_no_start_are_dropped_and_counted_once() {
    let mut a = assembler(1);
    let done = feed(
        &mut a,
        [
            pkt(CONTINUE, 0, &[1]),
            pkt(CONTINUE, 0, &[2]),
            pkt(END, 0, &[3]),
        ],
    );
    assert!(done.is_empty());
    assert_eq!(a.dropped(), 1);
    assert!(feed(&mut a, [pkt(END, 0, &[4])]).is_empty());
    assert_eq!(a.dropped(), 2);
    assert_eq!(feed(&mut a, [pkt(COMPLETE, 0, &[5])]), [(0, 0, vec![5])]);
}

/// A message longer than the buffer is dropped whole, never cut, counted once,
/// its remaining packets skipped up to its End.
#[test]
fn an_over_long_message_is_dropped_whole_and_counted_once() {
    let mut a = Sysex8Assembler::new(vec![0; 20], 20, 1);
    let data = payload(21);
    let message = Sysex8Message {
        group: G0,
        stream_id: 0,
        payload: &data,
    };
    assert!(feed_any(&mut a, message.packets()).is_empty());
    assert_eq!(a.dropped(), 1);
    // The limit itself fits.
    let fits = payload(20);
    let message = Sysex8Message {
        group: G0,
        stream_id: 0,
        payload: &fits,
    };
    assert_eq!(feed_any(&mut a, message.packets()), [fits]);
    assert_eq!(a.dropped(), 1);
}

fn feed_any(a: &mut Sysex8Assembler<Vec<u8>>, umps: impl IntoIterator<Item = Ump>) -> Vec<Vec<u8>> {
    feed(a, umps).into_iter().map(|(_, _, d)| d).collect()
}

/// With one lane, a second stream's Start is dropped, counted; the first
/// message is intact.
#[test]
fn a_start_with_no_free_lane_is_dropped_and_counted() {
    let mut a = assembler(1);
    let done = feed(
        &mut a,
        [pkt(START, 0, &[1]), pkt(START, 1, &[9]), pkt(END, 0, &[2])],
    );
    assert_eq!(done, [(0, 0, vec![1, 2])]);
    assert_eq!(a.dropped(), 1);
}

/// A message open at `lost_tail` or `reset` is dropped, counted; nothing is
/// spliced onto it afterwards.
#[test]
fn lost_tail_and_reset_drop_open_messages() {
    let mut a = assembler(2);
    feed(&mut a, [pkt(START, 0, &[1]), pkt(START, 1, &[2])]);
    a.lost_tail();
    assert_eq!(a.dropped(), 2);
    assert!(feed(&mut a, [pkt(END, 0, &[3])]).is_empty());
    feed(&mut a, [pkt(START, 0, &[1])]);
    a.reset();
    assert_eq!(a.dropped(), 4);
}

/// UMPs that are not SysEx8 are for the caller; a SysEx8 packet is not.
#[test]
fn only_sysex8_packets_are_taken() {
    let mut a = assembler(1);
    assert_eq!(a.push(Ump([0x0000_0000; 4])), Sysex8Step::NotSysex8);
    assert_eq!(
        a.push(Ump([0x5081_0010, 1, 0x0041_0000, 0])),
        Sysex8Step::NotSysex8
    );
    assert_eq!(a.push(pkt(START, 0, &[1])), Sysex8Step::Held);
    // Round trips through `Message` agree with the raw builder.
    let Ok(Message::Data128(_, Data128::Sysex8(p))) = pkt(START, 0, &[1]).decode() else {
        unreachable!()
    };
    assert_eq!(
        (p.status(), p.stream_id(), p.bytes()),
        (SysexStatus::Start, 0, &[1][..])
    );
}
