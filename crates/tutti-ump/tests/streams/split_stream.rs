//! `split_stream`: a buffer of concatenated packets cut by M2-104 Table 4.
//!
//! The packets are written as words, from the table, not built through
//! `Message`: the walker must not depend on the encoder it is read back with.

use tutti_ump::{Ump, split_stream};

/// A JR Timestamp (type 0, 1 word, Table 4).
const JR: u32 = 0x0020_1234;
/// A MIDI 2.0 Note On, group 0 channel 3 note 60 (type 4, 2 words).
const NOTE: [u32; 2] = [0x4093_3C00, 0x8000_0000];
/// A System Timing Clock (type 1, 1 word).
const CLOCK: u32 = 0x10F8_0000;

#[test]
fn mixed_lengths_are_walked_by_type() {
    let words = [JR, NOTE[0], NOTE[1], CLOCK];
    let split: Vec<Ump> = split_stream(&words).collect();
    assert_eq!(
        split,
        vec![
            Ump([JR, 0, 0, 0]),
            Ump([NOTE[0], NOTE[1], 0, 0]),
            Ump([CLOCK, 0, 0, 0]),
        ]
    );
}

#[test]
fn a_truncated_tail_is_dropped() {
    // A 2-word message with only its first word present is not a packet.
    assert_eq!(split_stream(&[NOTE[0]]).count(), 0);
    // A complete message before the truncated tail still comes out.
    let split: Vec<Ump> = split_stream(&[CLOCK, NOTE[0]]).collect();
    assert_eq!(split, vec![Ump([CLOCK, 0, 0, 0])]);
}

#[test]
fn no_words_no_packets() {
    assert_eq!(split_stream(&[]).count(), 0);
}

#[test]
fn every_type_has_its_table_4_size() {
    // M2-104 section 2.1.4, Table 4: a stream of one packet per type, each
    // padded to its size with a nonzero filler, comes out as 16 packets.
    let sizes = [1, 1, 1, 2, 2, 4, 1, 1, 2, 2, 2, 3, 3, 4, 4, 4];
    let mut words = Vec::new();
    for (mt, n) in sizes.iter().enumerate() {
        words.push((mt as u32) << 28);
        words.extend(std::iter::repeat_n(0x1111_1111, n - 1));
    }
    let split: Vec<Ump> = split_stream(&words).collect();
    assert_eq!(split.len(), 16, "one packet per message type");
    for (mt, packet) in split.iter().enumerate() {
        assert_eq!(packet.message_type() as usize, mt);
        assert_eq!(packet.words().len(), sizes[mt], "type {mt:#x}");
    }
}

#[test]
fn a_reserved_type_does_not_desync_the_stream() {
    // A type 0xB packet (3 words) then a note: counting 4 words for the 0xB
    // would eat the note's first word.
    let words = [0xB000_0000, 0, 0, NOTE[0], NOTE[1]];
    let split: Vec<Ump> = split_stream(&words).collect();
    assert_eq!(split.len(), 2);
    assert_eq!(split[1], Ump([NOTE[0], NOTE[1], 0, 0]));
}

/// M2-104 Table 4: a packet's size is a function of its message type, and
/// `Ump::from_words` reads one packet from the front of a slice by it, the
/// rest of the packet zero and anything past it unread; a slice that ends
/// inside the packet has none.
#[test]
fn from_words_reads_one_packet_of_the_size_its_type_gives() {
    // Message type and its size in words, the table.
    let sizes: [(u32, usize); 16] = [
        (0x0, 1),
        (0x1, 1),
        (0x2, 1),
        (0x3, 2),
        (0x4, 2),
        (0x5, 4),
        (0x6, 1),
        (0x7, 1),
        (0x8, 2),
        (0x9, 2),
        (0xA, 2),
        (0xB, 3),
        (0xC, 3),
        (0xD, 4),
        (0xE, 4),
        (0xF, 4),
    ];
    for (ty, size) in sizes {
        // The first word has the type; the others are the payload (any).
        let stream = [
            ty << 28 | 0x00AB_CDEF,
            0x1111_1111,
            0x2222_2222,
            0x3333_3333,
            0x4444_4444,
        ];
        let packet = Ump::from_words(&stream).unwrap();
        let mut want = [0u32; 4];
        want[..size].copy_from_slice(&stream[..size]);
        assert_eq!(packet, Ump(want), "type {ty:#x}");
        // One word short of the packet is no packet.
        assert_eq!(Ump::from_words(&stream[..size - 1]), None, "type {ty:#x}");
        assert_eq!(
            Ump::from_words(&stream[..size]),
            Some(packet),
            "type {ty:#x}"
        );
    }
    assert_eq!(Ump::from_words(&[]), None);
}

/// A stream is its packets read one after another: `split_stream` is
/// `from_words` applied to what is left.
#[test]
fn split_stream_is_from_words_walked_along_the_slice() {
    let words = [JR, NOTE[0], NOTE[1], CLOCK];
    let mut rest = &words[..];
    let mut walked = Vec::new();
    while let Some(packet) = Ump::from_words(rest) {
        rest = &rest[packet.words().len()..];
        walked.push(packet);
    }
    assert_eq!(walked, split_stream(&words).collect::<Vec<_>>());
}

/// The packet's words in hexadecimal, as the specification writes them: eight
/// digits a word, the meaningful words only, a space between.
#[test]
fn a_packet_displays_its_meaningful_words_in_hex() {
    let one = Ump([JR, 0xFFFF_FFFF, 0, 0]);
    assert_eq!(one.to_string(), "00201234");
    let two = Ump([NOTE[0], NOTE[1], 0, 0]);
    assert_eq!(format!("{two}"), "40933C00 80000000");
    assert_eq!(format!("{two:x}"), "40933c00 80000000");
    assert_eq!(format!("{two:#x}"), "0x40933c00 0x80000000");
    assert_eq!(format!("{two:X}"), "40933C00 80000000");
}
