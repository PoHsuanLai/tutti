//! Reserved bits and reserved values, against M2-104 §2.1.3 and the per-message
//! sections.
//!
//! A reserved bit is ignored on decode and zero on encode, so
//! `Ump::from(decode(x)) == x` holds for a packet with its reserved bits zero
//! and does not otherwise. A reserved value, an undefined status, or a
//! reserved message type is `Unknown`, and `Unknown` keeps the words.
//!
//! Mutation record (each planted, seen to fail at the named test, restored):
//! - drop the `& 0x7f` in `decode::u7` -> `reserved_bits_are_ignored`;
//! - widen `registered_per_note` to every number -> `reserved_values_are_unknown`;
//! - make `data7` mask instead of refuse -> `reserved_values_are_unknown`;
//! - let `data128` accept a `# of bytes` of 0 -> `reserved_values_are_unknown`;
//! - let `utility` return `Noop` for status 5 -> `undefined_statuses_are_unknown`;
//! - make `Unknown` drop word 4 -> `unknown_keeps_every_word`.

use tutti_ump::{Message, Ump, Unknown, decode};

/// `dirty` decodes to the message `clean` encodes, and re-encoding gives
/// `clean`: the reserved bits are read as zero and written as zero.
fn ignores(dirty: [u32; 4], clean: [u32; 4]) {
    let message = decode(Ump(clean)).unwrap_or_else(|u| panic!("clean {clean:08x?} is {u:?}"));
    assert_eq!(decode(Ump(dirty)), Ok(message), "dirty {dirty:08x?}");
    assert_eq!(Ump::from(message), Ump(clean));
    assert_ne!(dirty, clean);
}

fn unknown(words: [u32; 4]) {
    assert_eq!(decode(Ump(words)), Err(Unknown(Ump(words))), "{words:08x?}");
}

const ALL: u32 = 0xFFFF_FFFF;

#[test]
fn reserved_bits_are_ignored() {
    // Utility: the group nibble and the data of a NOOP are reserved.
    ignores([0x0F0F_FFFF, ALL, ALL, ALL], [0; 4]);
    ignores([0x0A11_1234, 0, 0, 0], [0x0010_1234, 0, 0, 0]);
    ignores([0x0A20_ABCD, 0, 0, 0], [0x0020_ABCD, 0, 0, 0]);
    // System: unused data bytes, and bit 7 of a data byte.
    ignores([0x15F6_FFFF, 0, 0, 0], [0x15F6_0000, 0, 0, 0]);
    ignores([0x15F3_09FF, 0, 0, 0], [0x15F3_0900, 0, 0, 0]);
    ignores([0x15F8_0000, ALL, ALL, ALL], [0x15F8_0000, 0, 0, 0]);
    // MIDI 1.0: bit 7 of each data byte, and the unused byte.
    ignores([0x2395_BCFF, 0, 0, 0], [0x2395_3C7F, 0, 0, 0]);
    ignores([0x23C5_89FF, 0, 0, 0], [0x23C5_0900, 0, 0, 0]);
    ignores([0x23D5_B3FF, 0, 0, 0], [0x23D5_3300, 0, 0, 0]);
    ignores([0x23E5_C5C6, 0, 0, 0], [0x23E5_4546, 0, 0, 0]);
    // SysEx 7: bytes past `# of bytes` and bit 7 of the ones within it.
    ignores([0x3202_7F7F, ALL, ALL, ALL], [0x3202_7F7F, 0, 0, 0]);
    ignores([0x3200_FFFF, ALL, ALL, ALL], [0x3200_0000, 0, 0, 0]);
    // SysEx 8: bytes past `# of bytes`; the stream id alone leaves all data
    // reserved.
    ignores([0x5401_07FF, ALL, ALL, ALL], [0x5401_0700, 0, 0, 0]);
    // §7.4.2: with attribute type 0 the attribute data is ignored.
    ignores(
        [0x4192_3C00, 0xABCD_1234, 0, 0],
        [0x4192_3C00, 0xABCD_0000, 0, 0],
    );
    // MIDI 2.0: the reserved byte of Poly Pressure, Control Change, Channel
    // Pressure and Pitch Bend, bit 7 of the note and controller bytes.
    ignores(
        [0x41A2_BCFF, 0x1122_3344, ALL, ALL],
        [0x41A2_3C00, 0x1122_3344, 0, 0],
    );
    ignores(
        [0x41B2_87FF, 0x1122_3344, 0, 0],
        [0x41B2_0700, 0x1122_3344, 0, 0],
    );
    ignores([0x41D2_FFFF, 5, 0, 0], [0x41D2_0000, 5, 0, 0]);
    ignores([0x41E2_FFFF, 5, 0, 0], [0x41E2_0000, 5, 0, 0]);
    ignores([0x4162_BCFF, 5, 0, 0], [0x4162_3C00, 5, 0, 0]);
    // The bank and index of an RPN are 7-bit.
    ignores([0x4122_8182, 5, 0, 0], [0x4122_0102, 5, 0, 0]);
    // Program Change: undefined option flags, the reserved byte of word 2 and
    // bit 7 of its 7-bit bytes; with B clear the bank bytes are ignored.
    ignores(
        [0x41C2_0001, 0x09AA_B4A4, 0, 0],
        [0x41C2_0001, 0x0900_3424, 0, 0],
    );
    ignores(
        [0x41C2_00FE, 0x89FF_FFFF, 0, 0],
        [0x41C2_0000, 0x0900_0000, 0, 0],
    );
    ignores(
        [0x41C2_FF01, 0x8900_0000, 0, 0],
        [0x41C2_0001, 0x0900_0000, 0, 0],
    );
    // Per-Note Management: the undefined option flags and the note's bit 7.
    ignores([0x41F2_BCFF, ALL, ALL, ALL], [0x41F2_3C03, 0, 0, 0]);
    // Flex: the channel nibble of a Group message, unused words and bytes.
    ignores(
        [0xD613_0000, 0x02FA_F080, ALL, ALL],
        [0xD610_0000, 0x02FA_F080, 0, 0],
    );
    ignores(
        [0xD610_0001, 0x0302_08FF, ALL, ALL],
        [0xD610_0001, 0x0302_0800, 0, 0],
    );
    ignores(
        [0xD610_0005, 0x54FF_FFFF, ALL, ALL],
        [0xD610_0005, 0x5400_0000, 0, 0],
    );
    ignores(
        [0xD610_0002, 0x1803_0200, 0x0200_FFFF, ALL],
        [0xD610_0002, 0x1803_0200, 0x0200_0000, 0],
    );
    ignores(
        [0xD610_0006, 0x0301_1900, 0x0000_FFFF, 0x8000_0000],
        [0xD610_0006, 0x0301_1900, 0x0000_0000, 0x8000_0000],
    );
    // Chord Name: with bass note 0 the bass accidental is not read.
    ignores(
        [0xD610_0006, 0x0301_1900, 0, 0x3000_0000],
        [0xD610_0006, 0x0301_1900, 0, 0x8000_0000],
    );
    // Stream: unused filter bits, capability bits, the unused words.
    ignores(
        [0xF000_0101, 0xFFFF_FFFF, ALL, ALL],
        [0xF000_0101, 0x0000_001F, 0, 0],
    );
    ignores(
        [0xF001_0101, 0x83FF_FFFF, ALL, ALL],
        [0xF001_0101, 0x8300_0303, 0, 0],
    );
    ignores([0xF005_02FC, ALL, ALL, ALL], [0xF005_0200, 0, 0, 0]);
    ignores([0xF010_FF0F, ALL, ALL, ALL], [0xF010_FF03, 0, 0, 0]);
    ignores(
        [0xF011_83FB, 0x0402_0110, ALL, ALL],
        [0xF011_833B, 0x0402_0110, 0, 0],
    );
    ignores(
        [0xF002_FFFF, 0xFF80_A0EB, 0x8102_8304, 0x8506_8708],
        [0xF002_0000, 0x0000_206B, 0x0102_0304, 0x0506_0708],
    );
    ignores([0xF020_FFFF, ALL, ALL, ALL], [0xF020_0000, 0, 0, 0]);
    ignores([0xF021_FFFF, ALL, ALL, ALL], [0xF021_0000, 0, 0, 0]);
}

#[test]
fn reserved_message_types_are_unknown() {
    // M2-104 Table 4: 0x6 to 0xA, 0xB, 0xC and 0xE are reserved.
    for mt in [0x6u32, 0x7, 0x8, 0x9, 0xA, 0xB, 0xC, 0xE] {
        unknown([
            (mt << 28) | 0x0123_4567,
            0x89AB_CDEF,
            0x0F0F_0F0F,
            0xF0F0_F0F0,
        ]);
    }
}

#[test]
fn undefined_statuses_are_unknown() {
    // Utility status 5 to 15.
    for status in 5..16u32 {
        unknown([status << 20, 0, 0, 0]);
    }
    // System: 0xF0 and 0xF7 are not used by UMP, 0xF4, 0xF5, 0xF9 and 0xFD
    // are reserved (Table 17).
    for status in [0xF0u32, 0xF4, 0xF5, 0xF7, 0xF9, 0xFD] {
        unknown([0x1000_0000 | (status << 16), 0, 0, 0]);
    }
    // MIDI 1.0 opcodes 0 to 7 and 0xF.
    for opcode in (0..8u32).chain([0xF]) {
        unknown([0x2000_0000 | (opcode << 20), 0, 0, 0]);
    }
    // SysEx 7 status 4 to 15, and more than 6 bytes.
    for status in 4..16u32 {
        unknown([0x3000_0000 | (status << 20), 0, 0, 0]);
    }
    for count in 7..16u32 {
        unknown([0x3000_0000 | (count << 16), 0, 0, 0]);
    }
    // MIDI 2.0 opcode 7.
    unknown([0x4070_0000, 0, 0, 0]);
    // Data 128 status 4 to 7 and 0xA to 0xF.
    for status in (4..8u32).chain(0xA..16) {
        unknown([0x5000_0000 | (status << 20), 0, 0, 0]);
    }
    // Flex: status bank 3 and above, undefined statuses of banks 0 to 2.
    unknown([0xD610_0300, 0, 0, 0]);
    unknown([0xD610_FF00, 0, 0, 0]);
    for status in [0x03u32, 0x04, 0x07, 0xFF] {
        unknown([0xD610_0000 | status, 0, 0, 0]);
    }
    unknown([0xD610_010D, 0, 0, 0]);
    unknown([0xD610_0205, 0, 0, 0]);
    // Stream: 0x07, 0x13, 0x22, and the unused statuses.
    for status in [0x07u32, 0x0F, 0x13, 0x22, 0x3FF] {
        unknown([0xF000_0000 | (status << 16), 0, 0, 0]);
    }
}

#[test]
fn reserved_values_are_unknown() {
    // Registered Per-Note Controller numbers Appendix A leaves reserved:
    // 0, 4 to 6, 9, 12 to 69, 80 to 90, 96 and above.
    for controller in [0u32, 4, 5, 6, 9, 12, 69, 80, 90, 96, 255] {
        unknown([0x4102_3C00 | controller, 0, 0, 0]);
    }
    for controller in [1u32, 2, 3, 7, 8, 10, 11, 70, 79, 91, 95] {
        assert!(
            decode(Ump([0x4102_3C00 | controller, 0, 0, 0])).is_ok(),
            "{controller}"
        );
    }
    // A MIDI 1.0 data byte drawn `0ddddddd` with the top bit set is malformed:
    // MTC, Song Position, Song Select, SysEx 7 data within `# of bytes`.
    unknown([0x15F1_DA00, 0, 0, 0]);
    unknown([0x15F2_C546, 0, 0, 0]);
    unknown([0x15F2_45C6, 0, 0, 0]);
    unknown([0x15F3_8900, 0, 0, 0]);
    unknown([0x3201_8000, 0, 0, 0]);
    unknown([0x3203_0001, 0x8000_0000, 0, 0]);
    unknown([0x3206_0000, 0x0000_0080, 0, 0]);
    // A Delta Clockstamp Ticks Per Quarter Note of 0 is reserved (§7.2.3.1).
    unknown([0x0030_0000, 0, 0, 0]);
    // Attribute types 4 to 255 are reserved (Table 8).
    unknown([0x4192_3C04, 0, 0, 0]);
    unknown([0x4182_3CFF, 0, 0, 0]);
    // SysEx 8: `# of bytes` 0 is not valid, 0xF is only for an End (§7.8).
    unknown([0x5400_0700, 0, 0, 0]);
    unknown([0x540F_0700, 0, 0, 0]);
    unknown([0x541F_0700, 0, 0, 0]);
    unknown([0x542F_0700, 0, 0, 0]);
    // Flex: Format is 0 and Address is 1 for the setup messages (§7.5.3 to
    // §7.5.5), Address 2 and 3 are reserved (Table 10).
    unknown([0xD650_0000, 0, 0, 0]);
    unknown([0xD600_0000, 0, 0, 0]);
    unknown([0xD600_0001, 0, 0, 0]);
    unknown([0xD600_0002, 0, 0, 0]);
    unknown([0xD620_0005, 0, 0, 0]);
    unknown([0xD630_0101, 0, 0, 0]);
    // Key Signature tonic 8 to 15 is reserved.
    unknown([0xD610_0005, 0x5800_0000, 0, 0]);
    // Chord Name: sharps/flats outside Tables 13 and 15, tonic 8 and up, chord
    // type past 0x1B, alteration type 5 and up, a bass note with the
    // "same as tonic" accidental or the reverse.
    unknown([0xD610_0006, 0x3301_0000, 0, 0x8000_0000]);
    unknown([0xD610_0006, 0x0801_0000, 0, 0x8000_0000]);
    unknown([0xD610_0006, 0x031C_0000, 0, 0x8000_0000]);
    unknown([0xD610_0006, 0x0301_5000, 0, 0x8000_0000]);
    unknown([0xD610_0006, 0x0301_0000, 0x5000_0000, 0x8000_0000]);
    unknown([0xD610_0006, 0x0301_0000, 0, 0x8100_0000]);
    unknown([0xD610_0006, 0x0301_0000, 0, 0x801C_0000]);
    // Stream messages that are not multi-packet have Format 0.
    unknown([0xF400_0101, 0, 0, 0]);
    unknown([0xF420_0000, 0, 0, 0]);
    // Endpoint Info: 33 to 127 Function Blocks are reserved (§7.1.2).
    unknown([0xF001_0101, 0x2100_0000, 0, 0]);
    unknown([0xF001_0101, 0x7F00_0000, 0, 0]);
    // Stream Configuration: protocols other than 1 and 2.
    unknown([0xF005_0000, 0, 0, 0]);
    unknown([0xF005_0300, 0, 0, 0]);
    unknown([0xF006_FF00, 0, 0, 0]);
    // Function Block Discovery: a block number above 31 that is not 0xFF.
    unknown([0xF010_2003, 0, 0, 0]);
    unknown([0xF010_FE03, 0, 0, 0]);
    // Function Block Info: direction 0, MIDI 1.0 value 3, block above 31, first
    // group above 15, spanned 0 or above 16.
    unknown([0xF011_8300, 0x0402_0110, 0, 0]);
    unknown([0xF011_830F, 0x0402_0110, 0, 0]);
    unknown([0xF011_A003 | 0x0300, 0x0402_0110, 0, 0]);
    unknown([0xF011_8303, 0x1002_0110, 0, 0]);
    unknown([0xF011_8303, 0x0400_0110, 0, 0]);
    unknown([0xF011_8303, 0x0411_0110, 0, 0]);
    // Function Block Name: a block above 31.
    unknown([0xF012_2000, 0, 0, 0]);
}

#[test]
fn unknown_keeps_every_word() {
    let words = [0x6123_4567, 0x89AB_CDEF, 0x0F0F_0F0F, 0xF0F0_F0F0];
    match decode(Ump(words)) {
        Err(Unknown(ump)) => assert_eq!(ump.0, words),
        other => panic!("{other:?}"),
    }
    // A defined type with a reserved value keeps the words past the size too.
    let words = [0x0050_0000, 1, 2, 3];
    assert_eq!(decode(Ump(words)), Err(Unknown(Ump(words))));
}

#[test]
fn a_defined_message_has_no_unknown_form() {
    // Spot check that the defined side is `Ok`, so the tests above cannot pass
    // by everything being `Unknown`.
    for words in [[0u32; 4], [0x2090_3C40, 0, 0, 0], [0xF020_0000, 0, 0, 0]] {
        assert!(matches!(
            decode(Ump(words)),
            Ok(Message::Utility(_) | Message::Midi1(..) | Message::Stream(_))
        ));
    }
}
