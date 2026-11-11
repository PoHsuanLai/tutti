//! `SysexAssembler`: SysEx7 packets in, complete `0xF0 .. 0xF7` messages out,
//! inside a buffer the caller provides.
//!
//! The assembler holds no `Vec`: it cannot allocate by construction, and the
//! two tests that push through `assert_no_alloc` pin it.

use assert_no_alloc::{AllocDisabler, assert_no_alloc};
use tutti_ump::{Message, MidiGroup, Sysex7Packet, SysexAssembler, SysexStatus, SysexStep, Ump};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

/// The plugin layer's sizes: a longest message of 64 KiB of payload and two
/// delimiters, and a block of 256 packets.
const MAX_PAYLOAD: usize = 64 * 1024;
const LONGEST: usize = MAX_PAYLOAD + 2;
const BLOCK: usize = 256;

fn assembler() -> SysexAssembler<Vec<u8>> {
    assembler_with(1)
}

/// The plugin layer's sizes with `lanes` open messages at once.
fn assembler_with(lanes: usize) -> SysexAssembler<Vec<u8>> {
    SysexAssembler::new(
        vec![0; SysexAssembler::room_for(LONGEST, BLOCK, lanes)],
        LONGEST,
        lanes,
    )
}

/// The packet of `status` carrying `chunk` (at most six bytes).
fn packet(group: MidiGroup, status: SysexStatus, chunk: &[u8]) -> Ump {
    Ump::from(Message::Sysex7(
        group,
        Sysex7Packet::new(status, chunk).unwrap(),
    ))
}

/// `payload` as SysEx7 packets: a lone `Complete` (an empty payload is one
/// empty packet), or `Start`, `Continue`*, `End`, six bytes to a packet.
fn packets(group: MidiGroup, payload: &[u8]) -> Vec<Ump> {
    if payload.len() <= 6 {
        return vec![packet(group, SysexStatus::Complete, payload)];
    }
    let chunks: Vec<&[u8]> = payload.chunks(6).collect();
    let last = chunks.len() - 1;
    chunks
        .into_iter()
        .enumerate()
        .map(|(i, chunk)| {
            let status = match i {
                0 => SysexStatus::Start,
                i if i == last => SysexStatus::End,
                _ => SysexStatus::Continue,
            };
            packet(group, status, chunk)
        })
        .collect()
}

/// `payload` between its delimiters.
fn framed(payload: &[u8]) -> Vec<u8> {
    let mut out = vec![0xF0];
    out.extend_from_slice(payload);
    out.push(0xF7);
    out
}

/// Feed `events`, collecting each completed message's bytes.
fn feed(asm: &mut SysexAssembler<Vec<u8>>, events: &[Ump]) -> Vec<Vec<u8>> {
    let mut done = Vec::new();
    for e in events {
        if let SysexStep::Complete(r) = asm.push(*e) {
            done.push(asm.message(r).bytes().to_vec());
        }
    }
    done
}

fn payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 0x7F) as u8).collect()
}

/// **A multi-packet message completes as one framed message on its
/// `END`**, with every payload byte in order.
#[test]
fn a_multi_packet_message_completes_on_its_end() {
    let body = payload(40);
    let mut asm = assembler();
    let pkts = packets(MidiGroup::FIRST, &body);
    assert!(pkts.len() > 2, "START, CONTINUE…, END");
    let done = feed(&mut asm, &pkts);
    assert_eq!(done, vec![framed(&body)]);
    assert_eq!(asm.dropped(), 0);
}

/// **A message spans a block boundary**: an open run survives
/// `begin_block`, and completes in the next block.
#[test]
fn an_open_run_carries_across_blocks() {
    let body = payload(30);
    let pkts = packets(MidiGroup::FIRST, &body);
    let mut asm = assembler();
    // A single first, so the run does not start at offset 0 and the
    // carry has to move it.
    let one = packets(MidiGroup::FIRST, &[1, 2]);
    assert_eq!(feed(&mut asm, &one), vec![framed(&[1, 2])]);
    let (first, rest) = pkts.split_at(2);
    assert!(feed(&mut asm, first).is_empty());
    asm.begin_block();
    assert_eq!(feed(&mut asm, rest), vec![framed(&body)]);
    assert_eq!(asm.dropped(), 0);
}

/// **A message past capacity is dropped whole and counted once**; the
/// next message is delivered intact.
#[test]
fn a_message_past_capacity_is_dropped_whole_and_counted() {
    let mut asm = SysexAssembler::new(vec![0; SysexAssembler::room_for(20, BLOCK, 1)], 20, 1);
    let long = payload(19); // 21 framed bytes: one past capacity
    assert!(feed(&mut asm, &packets(MidiGroup::FIRST, &long)).is_empty());
    assert_eq!(asm.dropped(), 1);
    let fits = payload(18); // exactly 20 framed bytes
    assert_eq!(
        feed(&mut asm, &packets(MidiGroup::FIRST, &fits)),
        vec![framed(&fits)]
    );
    assert_eq!(asm.dropped(), 1);
    assert!(asm.bytes().len() <= 20);
}

/// **A `START` with no `END` before the next `START` drops the open
/// message, counted**, and the second message is delivered whole.
#[test]
fn a_start_before_the_open_runs_end_drops_it() {
    let a = payload(20);
    let b = payload(15);
    let pa = packets(MidiGroup::FIRST, &a);
    let pb = packets(MidiGroup::FIRST, &b);
    let mut asm = assembler();
    // A's START and a CONTINUE, then B whole.
    assert!(feed(&mut asm, &pa[..2]).is_empty());
    assert_eq!(feed(&mut asm, &pb), vec![framed(&b)]);
    assert_eq!(asm.dropped(), 1);
    // A's tail is an orphan now: skipped, not counted again.
    assert!(feed(&mut asm, &pa[2..]).is_empty());
    assert_eq!(asm.dropped(), 1);
}

/// **A `SINGLE` on another group completes beside an open run without
/// disturbing it**, and on the run's own group it ends the run.
#[test]
fn a_single_beside_an_open_run() {
    let body = payload(25);
    let pkts = packets(MidiGroup::FIRST, &body);
    let other = MidiGroup::of(3);
    let mut asm = assembler();
    assert!(feed(&mut asm, &pkts[..2]).is_empty());
    assert_eq!(
        feed(&mut asm, &packets(other, &[9, 8, 7])),
        vec![framed(&[9, 8, 7])]
    );
    assert_eq!(feed(&mut asm, &pkts[2..]), vec![framed(&body)]);
    assert_eq!(asm.dropped(), 0);

    // On the run's own group a SINGLE ends the run.
    assert!(feed(&mut asm, &pkts[..2]).is_empty());
    assert_eq!(
        feed(&mut asm, &packets(MidiGroup::FIRST, &[5])),
        vec![framed(&[5])]
    );
    assert_eq!(asm.dropped(), 1);
}

/// **A UMP of the open message's Group ends it (M2-104 §7.7.1)**, dropped and
/// counted once, whatever kind it is that is not a Continue or a System Real
/// Time message: the packets left over are an orphan and are skipped.
#[test]
fn another_message_on_the_group_ends_the_run() {
    let body = payload(20);
    let pkts = packets(MidiGroup::FIRST, &body);
    let on_the_group = [
        ("MIDI 1.0 note", Ump([0x2090_3C40, 0, 0, 0])),
        ("MIDI 2.0 note", Ump([0x4090_3C00, 0x8000_0000, 0, 0])),
        ("System Common", Ump([0x10F1_0000, 0, 0, 0])),
        (
            "System Real Time status below 0xF8",
            Ump([0x10F6_0000, 0, 0, 0]),
        ),
        ("Flex Data", Ump([0xD010_0000, 0, 0, 0])),
        ("SysEx8", Ump([0x5001_0000, 0, 0, 0])),
        ("Mixed Data Set", Ump([0x5008_0000, 0, 0, 0])),
    ];
    for (what, other) in on_the_group {
        let mut asm = assembler();
        assert!(feed(&mut asm, &pkts[..2]).is_empty());
        assert_eq!(asm.push(other), SysexStep::NotSysex, "{what}");
        assert_eq!(asm.dropped(), 1, "{what} ends the message, counted");
        assert!(
            feed(&mut asm, &pkts[2..]).is_empty(),
            "{what}: the tail is an orphan"
        );
        assert_eq!(asm.dropped(), 1, "{what}: the orphan is not counted again");
        // The Group is free for the next message.
        assert_eq!(feed(&mut asm, &pkts), vec![framed(&body)], "{what}");
    }
}

/// **What the spec lets sit inside a message does not end it**: a System Real
/// Time message on its Group, a Groupless Utility or UMP Stream message, and
/// any message on another Group.
#[test]
fn what_may_be_interspersed_does_not_end_the_run() {
    let body = payload(20);
    let pkts = packets(MidiGroup::FIRST, &body);
    let interspersed = [
        ("clock", Ump([0x10F8_0000, 0, 0, 0])),
        ("active sensing", Ump([0x10FE_0000, 0, 0, 0])),
        ("reset", Ump([0x10FF_0000, 0, 0, 0])),
        ("JR Clock", Ump([0x0010_0000, 0, 0, 0])),
        ("NOOP", Ump([0x0000_0000, 0, 0, 0])),
        ("Endpoint Discovery", Ump([0xF000_0101, 0, 0, 0])),
        ("note on Group 2", Ump([0x4290_3C00, 0x8000_0000, 0, 0])),
        ("System Common on Group 2", Ump([0x11F1_0000, 0, 0, 0])),
        ("Flex Data on Group 2", Ump([0xD210_0000, 0, 0, 0])),
    ];
    let mut asm = assembler();
    assert!(feed(&mut asm, &pkts[..2]).is_empty());
    for (what, other) in interspersed {
        assert_eq!(asm.push(other), SysexStep::NotSysex, "{what}");
    }
    assert_eq!(feed(&mut asm, &pkts[2..]), vec![framed(&body)]);
    assert_eq!(asm.dropped(), 0);
}

/// **Two Groups' messages interleave** (M2-104 §2.1.2, §7.7.1), each
/// assembled apart, even across a block boundary.
#[test]
fn groups_interleave_their_messages() {
    let a = payload(40);
    let b: Vec<u8> = payload(33).iter().rev().copied().collect();
    let (ga, gb) = (MidiGroup::of(2), MidiGroup::of(9));
    let (pa, pb) = (packets(ga, &a), packets(gb, &b));
    let mut asm = assembler_with(2);
    let mut done = Vec::new();
    for (i, (x, y)) in pa.iter().zip(&pb).enumerate() {
        if i == 2 {
            asm.begin_block();
        }
        done.extend(feed(&mut asm, &[*x, *y]));
    }
    let longer = if pa.len() > pb.len() { &pa } else { &pb };
    let common = pa.len().min(pb.len());
    done.extend(feed(&mut asm, &longer[common..]));
    // B is the shorter, so it completes first.
    assert_eq!(done, vec![framed(&b), framed(&a)]);
    assert_eq!(asm.dropped(), 0);
}

/// **A `Start` with every lane held by another Group is dropped whole and
/// counted once**; the held message completes, and the Group can send again.
#[test]
fn a_start_with_no_free_lane_is_dropped() {
    let a = payload(20);
    let b = payload(20);
    let (pa, pb) = (packets(MidiGroup::of(1), &a), packets(MidiGroup::of(2), &b));
    let mut asm = assembler_with(1);
    assert!(feed(&mut asm, &pa[..2]).is_empty());
    assert!(feed(&mut asm, &pb).is_empty(), "no lane for Group 3");
    assert_eq!(asm.dropped(), 1);
    assert_eq!(feed(&mut asm, &pa[2..]), vec![framed(&a)]);
    // The lane is free again.
    assert_eq!(feed(&mut asm, &pb), vec![framed(&b)]);
    assert_eq!(asm.dropped(), 1);
}

/// **Every Group can hold a message at once** with sixteen lanes, without
/// allocating.
#[test]
fn sixteen_groups_assemble_at_once() {
    let bodies: Vec<Vec<u8>> = (0..16).map(|g| payload(20 + g)).collect();
    let per_group: Vec<Vec<Ump>> = bodies
        .iter()
        .enumerate()
        .map(|(g, body)| packets(MidiGroup::of(g as u8), body))
        .collect();
    let mut asm = SysexAssembler::new(vec![0; SysexAssembler::room_for(64, BLOCK, 16)], 64, 16);
    let mut done = 0usize;
    assert_no_alloc(|| {
        let longest = per_group.iter().map(Vec::len).max().unwrap();
        for i in 0..longest {
            for packets in &per_group {
                if let Some(p) = packets.get(i)
                    && let SysexStep::Complete(r) = asm.push(*p)
                {
                    let g = packets[0].group().unwrap().get() as usize;
                    assert_eq!(asm.message(r).payload(), bodies[g].as_slice());
                    done += 1;
                }
            }
        }
    });
    assert_eq!(done, 16);
    assert_eq!(asm.dropped(), 0);
}

/// **An assembler with no room drops (and counts) every message**, rather
/// than writing past its buffer: with a zero message cap, and with a cap the
/// empty buffer cannot hold.
#[test]
fn an_assembler_with_no_room_drops_everything() {
    for max_message in [0, 100] {
        let mut asm = SysexAssembler::new(Vec::<u8>::new(), max_message, 1);
        assert!(feed(&mut asm, &packets(MidiGroup::FIRST, &[1])).is_empty());
        assert!(feed(&mut asm, &packets(MidiGroup::FIRST, &payload(20))).is_empty());
        assert_eq!(asm.dropped(), 2, "max_message {max_message}");
        assert_eq!(asm.bytes().len(), 0);
    }
}

/// **The longest message fits, carried across blocks of a full MIDI cap
/// each, without allocating**, and the buffer's address never moves.
#[test]
fn the_longest_message_fits_without_allocating() {
    let body = payload(MAX_PAYLOAD);
    let pkts = packets(MidiGroup::FIRST, &body);
    let mut asm = assembler();
    let base = asm.bytes().as_ptr();
    let singles: Vec<Ump> = (0..BLOCK)
        .flat_map(|_| packets(MidiGroup::of(1), &[1, 2, 3, 4, 5, 6]))
        .collect();
    let mut done = 0usize;
    assert_no_alloc(|| {
        // Packets one MIDI block at a time, the final block's room filled
        // with singles on another group.
        for chunk in pkts.chunks(BLOCK - 1) {
            asm.begin_block();
            for p in chunk {
                if let SysexStep::Complete(r) = asm.push(*p) {
                    done += 1;
                    assert_eq!(r.len(), LONGEST);
                }
            }
        }
        for s in &singles[..BLOCK / 2] {
            if let SysexStep::Complete(_) = asm.push(*s) {
                done += 1;
            }
        }
    });
    assert_eq!(asm.dropped(), 0);
    assert_eq!(done, 1 + BLOCK / 2);
    assert_eq!(asm.bytes().as_ptr(), base, "the buffer never moved");
}

/// **More packets in one block than the buffer has room for drop the
/// message that would overflow it, counted, without allocating.** A
/// complete longest message fills the message part of the buffer; the
/// next message, short of the message cap but past the room, is dropped.
#[test]
fn more_packets_than_room_drop_without_allocating() {
    let longest = packets(MidiGroup::FIRST, &payload(MAX_PAYLOAD));
    // Twice a block of packets: more than the room left after `longest`.
    let next = packets(MidiGroup::FIRST, &payload(2 * BLOCK * 6));
    assert!(next.len() > BLOCK);
    let mut asm = assembler();
    let mut done = 0usize;
    assert_no_alloc(|| {
        asm.begin_block();
        for p in longest.iter().chain(&next) {
            if let SysexStep::Complete(_) = asm.push(*p) {
                done += 1;
            }
        }
    });
    assert_eq!(done, 1, "the longest message, not the one past the room");
    assert_eq!(asm.dropped(), 1);
}

/// **A tail lost upstream drops the open message, counted, and the rest
/// of its packets in a later block are not spliced onto it.**
#[test]
fn a_lost_tail_drops_the_open_message() {
    let body = payload(60);
    let pkts = packets(MidiGroup::FIRST, &body);
    let mut asm = assembler();
    assert!(feed(&mut asm, &pkts[..3]).is_empty());
    asm.lost_tail(); // pkts[3..5] never arrive
    asm.begin_block();
    assert!(feed(&mut asm, &pkts[5..]).is_empty());
    assert_eq!(asm.dropped(), 1);
    // The next message is whole.
    assert_eq!(feed(&mut asm, &pkts), vec![framed(&body)]);
    assert_eq!(asm.dropped(), 1);
}

/// **A lost tail and a reset drop every Group's open message**, each counted.
#[test]
fn a_lost_tail_and_a_reset_drop_every_groups_message() {
    let a = packets(MidiGroup::of(0), &payload(20));
    let b = packets(MidiGroup::of(5), &payload(20));
    for lost in [true, false] {
        let mut asm = assembler_with(2);
        assert!(feed(&mut asm, &a[..2]).is_empty());
        assert!(feed(&mut asm, &b[..2]).is_empty());
        if lost {
            asm.lost_tail();
        } else {
            asm.reset();
        }
        assert_eq!(asm.dropped(), 2);
        assert!(feed(&mut asm, &a[2..]).is_empty());
        assert!(feed(&mut asm, &b[2..]).is_empty());
        assert_eq!(asm.dropped(), 2);
    }
}

/// **A reset drops the open message, counted**, so its remaining packets
/// after the reset are skipped rather than completing it.
#[test]
fn a_reset_drops_the_open_message() {
    let body = payload(30);
    let pkts = packets(MidiGroup::FIRST, &body);
    let mut asm = assembler();
    assert!(feed(&mut asm, &pkts[..2]).is_empty());
    asm.reset();
    assert_eq!(asm.dropped(), 1);
    assert!(asm.bytes().is_empty());
    assert!(feed(&mut asm, &pkts[2..]).is_empty());
    assert_eq!(asm.dropped(), 1);
}

/// **A packet that is not a well-formed SysEx7 packet is `Held` and has no
/// effect**, even inside an open run; a packet of another type is `NotSysex`.
#[test]
fn a_malformed_packet_is_held_and_ignored() {
    let body = payload(20);
    let pkts = packets(MidiGroup::FIRST, &body);
    let mut asm = assembler();
    assert!(feed(&mut asm, &pkts[..2]).is_empty());
    // Seven bytes claimed (a packet holds six), then a data byte of 0x80.
    assert_eq!(asm.push(Ump([0x3017_0000, 0, 0, 0])), SysexStep::Held);
    assert_eq!(asm.push(Ump([0x3021_8000, 0, 0, 0])), SysexStep::Held);
    // A reserved status.
    assert_eq!(asm.push(Ump([0x3051_0000, 0, 0, 0])), SysexStep::Held);
    assert_eq!(asm.push(Ump([0x0000_0000, 0, 0, 0])), SysexStep::NotSysex);
    assert_eq!(feed(&mut asm, &pkts[2..]), vec![framed(&body)]);
    assert_eq!(asm.dropped(), 0);
}

/// **A completed message is a checked `SysexMessage`**, and its payload is
/// the bytes between the delimiters.
#[test]
fn a_completed_message_is_a_sysex_message() {
    let body = payload(40);
    let mut asm = assembler();
    for p in packets(MidiGroup::FIRST, &body) {
        if let SysexStep::Complete(r) = asm.push(p) {
            let message = asm.message(r);
            assert_eq!(message.payload(), body.as_slice());
            assert_eq!(r.len(), body.len() + 2);
            assert!(!r.is_empty());
        }
    }
}

/// **A new block forgets the last block's completed messages** and keeps the
/// buffer's use bounded.
#[test]
fn a_new_block_forgets_the_completed_messages() {
    let mut asm = assembler();
    assert_eq!(
        feed(&mut asm, &packets(MidiGroup::FIRST, &[1, 2])),
        vec![framed(&[1, 2])]
    );
    assert_eq!(asm.bytes(), framed(&[1, 2]));
    asm.begin_block();
    assert!(asm.bytes().is_empty());
}
