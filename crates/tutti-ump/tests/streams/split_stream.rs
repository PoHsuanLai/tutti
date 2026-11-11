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
