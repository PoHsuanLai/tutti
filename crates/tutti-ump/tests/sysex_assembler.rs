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
    SysexAssembler::new(vec![0; SysexAssembler::room_for(LONGEST, BLOCK)], LONGEST)
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
    let mut asm = SysexAssembler::new(vec![0; SysexAssembler::room_for(20, BLOCK)], 20);
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

/// **Other messages pass through an open run** (`NotSysex`) and do not
/// end it.
#[test]
fn other_messages_do_not_end_a_run() {
    let body = payload(20);
    let pkts = packets(MidiGroup::FIRST, &body);
    let note = Ump([0x4090_3C00, 0x8000_0000, 0, 0]);
    let mut asm = assembler();
    assert!(feed(&mut asm, &pkts[..2]).is_empty());
    assert_eq!(asm.push(note), SysexStep::NotSysex);
    assert_eq!(feed(&mut asm, &pkts[2..]), vec![framed(&body)]);
}

/// **An assembler with no room drops (and counts) every message**, rather
/// than writing past its buffer: with a zero message cap, and with a cap the
/// empty buffer cannot hold.
#[test]
fn an_assembler_with_no_room_drops_everything() {
    for max_message in [0, 100] {
        let mut asm = SysexAssembler::new(Vec::<u8>::new(), max_message);
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
