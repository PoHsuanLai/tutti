//! `sysex7_packets` and `SysexMessage::packets`: one payload as SysEx7 packets
//! (M2-104 section 7.7), and `Sysex7Packet::data`.
//!
//! Mutation record per case, each run against `Sysex7Packets::next` unless
//! it says otherwise.

use tutti_ump::{
    Message, MidiGroup, Sysex7Packet, SysexAssembler, SysexMessage, SysexStatus, SysexStep, U7,
    Ump, decode, sysex7_packets,
};

fn payload(len: usize) -> Vec<U7> {
    (0..len).map(|i| U7::of((i % 128) as u8)).collect()
}

fn raw(payload: &[U7]) -> Vec<u8> {
    payload.iter().map(|b| b.get()).collect()
}

/// Each packet as its status and bytes, read back through `decode`.
fn decoded(group: MidiGroup, p: &[U7]) -> Vec<(SysexStatus, Vec<u8>)> {
    sysex7_packets(group, p)
        .map(|ump| {
            let Ok(Message::Sysex7(g, packet)) = decode(ump) else {
                panic!("not a SysEx7 packet");
            };
            assert_eq!(g, group);
            (packet.status(), packet.data().to_vec())
        })
        .collect()
}

use SysexStatus::{Complete, Continue, End, Start};

/// Up to six bytes is one `Complete`; empty is one `Complete` of no bytes.
///
/// Mutation: an empty payload yielding no packet, or `> 6` written `>= 6`
/// -> fails here.
#[test]
fn a_short_payload_is_one_complete_packet() {
    assert_eq!(decoded(MidiGroup::of(0), &[]), [(Complete, vec![])]);
    let one = payload(1);
    assert_eq!(decoded(MidiGroup::of(0), &one), [(Complete, raw(&one))]);
    let six = payload(6);
    assert_eq!(decoded(MidiGroup::of(0), &six), [(Complete, raw(&six))]);
}

/// Seven bytes: `Start` with six, `End` with one.
///
/// Mutation: the last packet's status `Continue` instead of `End`, or the
/// first `Complete` -> fails here.
#[test]
fn seven_bytes_are_start_and_end() {
    let p = payload(7);
    let r = raw(&p);
    assert_eq!(
        decoded(MidiGroup::of(0), &p),
        [(Start, r[..6].to_vec()), (End, r[6..].to_vec())]
    );
}

/// Twelve bytes: `Start` and `End`, six each; thirteen: a `Continue` between.
///
/// Mutation: `div_ceil` replaced by floor in `size_hint`, or an `End` of zero
/// bytes after a full last chunk -> fails here.
#[test]
fn twelve_and_thirteen_bytes() {
    let p = payload(12);
    let r = raw(&p);
    assert_eq!(
        decoded(MidiGroup::of(0), &p),
        [(Start, r[..6].to_vec()), (End, r[6..].to_vec())]
    );
    let p = payload(13);
    let r = raw(&p);
    assert_eq!(
        decoded(MidiGroup::of(0), &p),
        [
            (Start, r[..6].to_vec()),
            (Continue, r[6..12].to_vec()),
            (End, r[12..].to_vec())
        ]
    );
}

/// A long payload: `Start`, `Continue`*, `End`, six bytes each but the last.
///
/// Mutation: the middle status `Start` -> fails here.
#[test]
fn a_long_payload_runs_start_continue_end() {
    let p = payload(1000);
    let got = decoded(MidiGroup::of(0), &p);
    assert_eq!(got.len(), 1000usize.div_ceil(6));
    assert_eq!(got[0].0, Start);
    assert_eq!(got.last().unwrap().0, End);
    assert!(got[1..got.len() - 1].iter().all(|(s, _)| *s == Continue));
    assert!(got[..got.len() - 1].iter().all(|(_, b)| b.len() == 6));
    assert_eq!(got.last().unwrap().1.len(), 1000 - 6 * 166);
    let joined: Vec<u8> = got.into_iter().flat_map(|(_, b)| b).collect();
    assert_eq!(joined, raw(&p));
}

/// `len()` is the packet count at every point, and the iterator is fused.
///
/// Mutation: `size_hint` returning 1 for an exhausted iterator, or not
/// shrinking as packets are taken -> fails here.
#[test]
fn len_counts_the_packets_left_and_the_iterator_is_fused() {
    for len in 0..=40 {
        let p = payload(len);
        let mut it = sysex7_packets(MidiGroup::of(0), &p);
        let mut left = len.div_ceil(6).max(1);
        assert_eq!(it.len(), left, "length {len}");
        while it.next().is_some() {
            left -= 1;
            assert_eq!(it.len(), left, "length {len}");
        }
        assert_eq!(it.next(), None);
        assert_eq!(it.next(), None);
        assert_eq!(it.len(), 0);
    }
}

/// Every packet carries the group it was asked for.
///
/// Mutation: group 0 written into the words -> fails here.
#[test]
fn the_group_is_on_every_packet() {
    let p = payload(20);
    for g in [0u8, 5, 15] {
        let group = MidiGroup::of(g);
        let umps: Vec<Ump> = sysex7_packets(group, &p).collect();
        assert_eq!(umps.len(), 4);
        for ump in umps {
            assert_eq!((ump.words()[0] >> 24) & 0xF, u32::from(g));
            let Ok(Message::Sysex7(got, _)) = decode(ump) else {
                panic!()
            };
            assert_eq!(got, group);
        }
    }
}

/// The writer's packets assemble back into the framed message, for every
/// length, through `SysexMessage::packets`.
///
/// Mutation: a dropped `Continue` (skip the second packet), or the last
/// chunk one byte short -> fails here.
#[test]
fn the_assembler_gives_back_the_message() {
    for len in 0..=40 {
        let p = payload(len);
        let mut framed = vec![0xF0];
        framed.extend(raw(&p));
        framed.push(0xF7);
        let message = SysexMessage::new(&framed).unwrap();
        let group = MidiGroup::of(3);

        let mut asm = SysexAssembler::new(vec![0u8; 128], 64, 1);
        asm.begin_block();
        let mut out = None;
        for ump in message.packets(group) {
            if let SysexStep::Complete(r) = asm.push(ump) {
                assert!(out.replace(asm.message(r).bytes().to_vec()).is_none());
            }
        }
        assert_eq!(out.as_deref(), Some(framed.as_slice()), "length {len}");
        assert_eq!(asm.dropped(), 0);
        // The two entry points agree.
        assert!(message.packets(group).eq(sysex7_packets(group, &p)));
    }
}

/// `data` is the same bytes as `bytes`.
///
/// Mutation: `data` returning the whole six-byte array -> fails here.
#[test]
fn data_is_bytes_as_plain_bytes() {
    for len in 0..=6 {
        let p = payload(len);
        let packet = Sysex7Packet::new(Complete, &raw(&p)).unwrap();
        assert_eq!(packet.data().len(), len);
        assert!(
            packet
                .data()
                .iter()
                .zip(packet.bytes())
                .all(|(a, b)| *a == b.get())
        );
    }
}

/// A `SysexRef` made by hand reads back its range and length.
///
/// Mutation: `new` swapping `start` and `len`, or `range` ending at `len`
/// instead of `start + len` -> fails here.
#[test]
fn a_ref_made_by_hand_names_its_range() {
    let r = tutti_ump::SysexRef::new(10, 4);
    assert_eq!(r.range(), 10..14);
    assert_eq!(r.len(), 4);
    assert!(!r.is_empty());
}

/// The words of a single packet from M2-104 Table 18: type 3, group, status 0,
/// byte count, then the bytes.
///
/// Mutation (run): the count nibble shifted by 20 instead of 16 -> each case
/// fails.
#[test]
fn a_single_packet_is_the_spec_words() {
    let words = |group, payload: &[u8]| {
        let bytes: Vec<U7> = payload.iter().map(|&b| U7::of(b)).collect();
        let mut packets = sysex7_packets(MidiGroup::of(group), &bytes);
        assert_eq!(packets.len(), 1);
        packets.next().unwrap().words().to_vec()
    };
    assert_eq!(
        words(3, &[0x7E, 0x7F, 0x06, 0x01]),
        [0x3304_7E7F, 0x0601_0000]
    );
    assert_eq!(words(0, &[]), [0x3000_0000, 0]);
    assert_eq!(words(15, &[1, 2, 3, 4, 5, 6]), [0x3F06_0102, 0x0304_0506]);
}

/// `Sysex7Packets::checked` is `sysex7_packets` for a payload of plain bytes,
/// and refuses one with a byte of 128 or more instead of masking it.
///
/// Mutation: `b < 0x80` written `b <= 0x80`, or the check dropped -> fails.
#[test]
fn checked_bytes_are_the_same_packets_and_a_high_bit_is_refused() {
    use tutti_ump::Sysex7Packets;
    for len in 0..=20 {
        let p = payload(len);
        let plain = raw(&p);
        let checked = Sysex7Packets::checked(MidiGroup::of(2), &plain).expect("7-bit bytes");
        assert_eq!(
            checked.collect::<Vec<Ump>>(),
            sysex7_packets(MidiGroup::of(2), &p).collect::<Vec<Ump>>(),
            "{len} bytes"
        );
    }
    assert!(Sysex7Packets::checked(MidiGroup::of(0), &[1, 0x80, 2]).is_none());
    assert!(Sysex7Packets::checked(MidiGroup::of(0), &[0xF7]).is_none());
    assert!(Sysex7Packets::checked(MidiGroup::of(0), &[0x7F]).is_some());
}
