//! Reserved bits and reserved values, against M2-104 §2.1.3 and the per-message
//! sections.
//!
//! A reserved bit is ignored on decode and zero on encode. A packet is
//! canonical when `Ump::from(decode(x)) == x`: its reserved bits are zero, and
//! a field another field makes meaningless has its canonical value (attribute
//! data under type 0, bank bytes under a clear Bank Valid bit, and a chord bass
//! accidental under bass note 0, which is -8). A reserved value in an
//! enumerated or bounded field is kept raw and the message re-encodes exactly.
//! An undefined status, a reserved message type, a count that cannot be framed
//! and a `0ddddddd` data byte with its top bit set are `Unknown`, which keeps
//! the words.
//!
//! Mutation record (each planted, seen to fail at the named test, restored):
//! - drop the `& 0x7f` in `decode::u7` -> `reserved_bits_are_ignored`;
//! - make `data7` mask instead of refuse -> `what_cannot_be_framed_is_unknown`;
//! - let `data128` accept a `# of bytes` of 0 -> `what_cannot_be_framed_is_unknown`;
//! - make `direction` return `None` for 0 in `decode` (the old rule) ->
//!   `an_inactive_function_block_with_zeroed_fields_is_still_read`;
//! - decode a reserved attribute type as `Attribute::None` -> `reserved_values_are_kept`;
//! - let `utility` return `Noop` for status 5 -> `undefined_statuses_are_unknown`;
//! - make `Unknown` drop word 4 -> `unknown_keeps_every_word`.

use tutti_ump::{
    Accidental, Address, Alteration, AlterationKind, Attribute, BlockSelect, ChordBass, ChordName,
    ChordPitch, ChordType, Direction, EndpointInfo, Flex, FlexAddress, FlexTextKind, Form,
    KeySignature, Letter, Message, Midi1Port, MidiChannel, MidiGroup, NoteNumber, Protocol,
    RegisteredPerNoteController, SharpsFlats, Stream, StreamConfig, U4, U7, Ump, UmpVersion,
    Unknown, Utility, Voice2, decode,
};

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
    // Chord Name with an all-zero bass block: bass note 0 is the tonic, its
    // accidental is not read, and the canonical accidental is the field's -8.
    ignores(
        [0xDA00_0006, 0x0301_0000, 0, 0],
        [0xDA00_0006, 0x0301_0000, 0, 0x8000_0000],
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

/// A reserved value in an enumerated or bounded field is kept: the message is
/// read, with the raw value, and re-encodes to the same words.
fn kept(words: [u32; 4], message: Message) {
    assert_eq!(decode(Ump(words)), Ok(message), "decode {words:08x?}");
    assert_eq!(Ump::from(message), Ump(words), "encode {message:?}");
}

#[test]
fn reserved_values_are_kept() {
    let g = MidiGroup::of;
    let ch = MidiChannel::of;
    let n = |v| NoteNumber::new(v).unwrap();
    let u7 = |v| U7::new(v).unwrap();
    let none = Alteration {
        kind: AlterationKind::None,
        degree: U4::new(0).unwrap(),
    };
    let flex = |f| Message::Flex(g(6), f);
    let stream = Message::Stream;

    // A Delta Clockstamp Ticks Per Quarter Note of 0 is reserved (§7.2.3.1).
    kept(
        [0x0030_0000, 0, 0, 0],
        Message::Utility(Utility::TicksPerQuarterNote(0)),
    );
    // Attribute types 4 to 255 are reserved (Table 8): the data is kept too.
    kept(
        [0x4192_3C04, 0xABCD_1234, 0, 0],
        Message::Midi2(
            Address {
                group: g(1),
                channel: ch(2),
            },
            Voice2::NoteOn {
                note: n(60),
                velocity: 0xABCD,
                attribute: Attribute::Reserved {
                    kind: 4,
                    data: 0x1234,
                },
            },
        ),
    );
    kept(
        [0x4182_3CFF, 0x0001_FFFF, 0, 0],
        Message::Midi2(
            Address {
                group: g(1),
                channel: ch(2),
            },
            Voice2::NoteOff {
                note: n(60),
                velocity: 1,
                attribute: Attribute::Reserved {
                    kind: 0xFF,
                    data: 0xFFFF,
                },
            },
        ),
    );
    // A Registered Per-Note Controller number is an index, read as one whether
    // or not Appendix A defines it, like an RPN, an NRPN or a CC index.
    for controller in [0u32, 4, 9, 12, 69, 80, 96, 255] {
        kept(
            [0x4102_3C00 | controller, 7, 0, 0],
            Message::Midi2(
                Address {
                    group: g(1),
                    channel: ch(2),
                },
                Voice2::RegisteredPerNote {
                    note: n(60),
                    controller: RegisteredPerNoteController::Reserved(controller as u8),
                    value: 7,
                },
            ),
        );
    }
    // Flex Address 2 and 3 are reserved (Table 10).
    kept(
        [0xD620_0005, 0x5400_0000, 0, 0],
        flex(Flex::SetKeySignature(
            FlexAddress::Reserved(2),
            KeySignature {
                sharps_flats: SharpsFlats::new(5),
                tonic: Some(Letter::D),
            },
        )),
    );
    kept(
        [0xD630_0101, 0x4800_0000, 0, 0],
        flex(Flex::Text {
            address: FlexAddress::Reserved(3),
            kind: FlexTextKind::ProjectName,
            form: Form::Complete,
            bytes: *b"H\0\0\0\0\0\0\0\0\0\0\0",
        }),
    );
    // Key Signature tonic 8 to 15 is reserved.
    kept(
        [0xD610_0005, 0x5800_0000, 0, 0],
        flex(Flex::SetKeySignature(
            FlexAddress::Group,
            KeySignature {
                sharps_flats: SharpsFlats::new(5),
                tonic: Some(Letter::Reserved(8)),
            },
        )),
    );
    // Chord Name: an accidental outside Tables 13 and 15, a tonic of 8 and up,
    // a chord type past 0x1B, an alteration type of 5 and up, a bass note with
    // the "same as tonic" accidental, a bass chord type past 0x1B.
    kept(
        [0xD610_0006, 0x3801_0000, 0, 0x8000_0000],
        flex(Flex::SetChordName(
            FlexAddress::Group,
            ChordName {
                tonic_accidental: Accidental::Reserved(3),
                tonic: Some(Letter::Reserved(8)),
                chord_type: ChordType::new(1).unwrap(),
                alterations: [none; 4],
                bass: ChordBass {
                    pitch: None,
                    chord_type: ChordType::new(0).unwrap(),
                    alterations: [none; 2],
                },
            },
        )),
    );
    kept(
        [0xD610_0006, 0x0301_5000, 0x5000_0000, 0x8000_0000],
        flex(Flex::SetChordName(
            FlexAddress::Group,
            ChordName {
                tonic_accidental: Accidental::Natural,
                tonic: Some(Letter::C),
                chord_type: ChordType::new(1).unwrap(),
                alterations: [
                    Alteration {
                        kind: AlterationKind::Reserved(5),
                        degree: U4::new(0).unwrap(),
                    },
                    none,
                    Alteration {
                        kind: AlterationKind::Reserved(5),
                        degree: U4::new(0).unwrap(),
                    },
                    none,
                ],
                bass: ChordBass {
                    pitch: None,
                    chord_type: ChordType::new(0).unwrap(),
                    alterations: [none; 2],
                },
            },
        )),
    );
    kept(
        [0xD610_0006, 0x0301_0000, 0, 0x8100_0000],
        flex(Flex::SetChordName(
            FlexAddress::Group,
            ChordName {
                tonic_accidental: Accidental::Natural,
                tonic: Some(Letter::C),
                chord_type: ChordType::new(1).unwrap(),
                alterations: [none; 4],
                bass: ChordBass {
                    pitch: Some(ChordPitch {
                        accidental: Accidental::Reserved(8),
                        letter: Letter::A,
                    }),
                    chord_type: ChordType::new(0).unwrap(),
                    alterations: [none; 2],
                },
            },
        )),
    );
    // Endpoint Info: 33 to 127 Function Blocks are reserved (§7.1.2).
    for count in [33u8, 127] {
        kept(
            [0xF001_0101, u32::from(count) << 24, 0, 0],
            stream(Stream::EndpointInfo(EndpointInfo {
                version: UmpVersion { major: 1, minor: 1 },
                static_function_blocks: false,
                function_blocks: u7(count),
                midi2: false,
                midi1: false,
                rx_jr: false,
                tx_jr: false,
            })),
        );
    }
    // Stream Configuration: protocols other than 1 and 2.
    for protocol in [0u8, 3, 0xFF] {
        kept(
            [0xF005_0000 | u32::from(protocol) << 8, 0, 0, 0],
            stream(Stream::StreamConfigurationRequest(StreamConfig {
                protocol: Protocol::Reserved(protocol),
                rx_jr: false,
                tx_jr: false,
            })),
        );
    }
    // Function Block Discovery: a block number above 31 that is not 0xFF is
    // kept as the number it is.
    for block in [32u8, 0xFE] {
        let words = [0xF010_0003 | u32::from(block) << 8, 0, 0, 0];
        let Ok(
            message @ Message::Stream(Stream::FunctionBlockDiscovery {
                block: BlockSelect::One(id),
                ..
            }),
        ) = decode(Ump(words))
        else {
            panic!("{words:08x?}");
        };
        assert_eq!(id.get(), block);
        assert_eq!(Ump::from(message), Ump(words));
    }
    // A chord type past 0x1B, as a type and as a bass type.
    for words in [
        [0xD610_0006u32, 0x031C_0000, 0, 0x8000_0000],
        [0xD610_0006, 0x0301_0000, 0, 0x801C_0000],
    ] {
        let Ok(message @ Message::Flex(_, Flex::SetChordName(_, c))) = decode(Ump(words)) else {
            panic!()
        };
        assert!(c.chord_type.get() == 0x1C || c.bass.chord_type.get() == 0x1C);
        assert_eq!(Ump::from(message), Ump(words));
    }
}

#[test]
fn an_inactive_function_block_with_zeroed_fields_is_still_read() {
    // The message a device sends for a block it has switched off: direction
    // 0 is reserved, and the active flag and the id are still there to read.
    let Ok(Message::Stream(Stream::FunctionBlockInfo(info))) = decode(Ump([0xF011_0500, 0, 0, 0]))
    else {
        panic!("a Function Block Info with zeroed fields is a Function Block Info");
    };
    assert!(!info.active);
    assert_eq!(info.id.get(), 5);
    assert_eq!(info.direction, Direction::Reserved(0));
    assert_eq!(info.groups_spanned.get(), 0);
    assert_eq!(
        Ump::from(Message::Stream(Stream::FunctionBlockInfo(info))),
        Ump([0xF011_0500, 0, 0, 0])
    );
}

#[test]
fn a_later_revisions_reserved_codes_leave_the_message_readable() {
    // Direction 0, MIDI 1.0 value 3, a block id above 31, a first group above
    // 15 and a span of 0 or above 16, one at a time.
    for (words, check) in [
        ([0xF011_8300u32, 0x0402_0110u32], 0),
        ([0xF011_830F, 0x0402_0110], 1),
        ([0xF011_A303, 0x0402_0110], 2),
        ([0xF011_8303, 0x1002_0110], 3),
        ([0xF011_8303, 0x0400_0110], 4),
        ([0xF011_8303, 0x0411_0110], 5),
    ] {
        let Ok(Message::Stream(Stream::FunctionBlockInfo(info))) =
            decode(Ump([words[0], words[1], 0, 0]))
        else {
            panic!("{words:08x?} is a Function Block Info");
        };
        assert_eq!(
            Ump::from(Message::Stream(Stream::FunctionBlockInfo(info))).0[..2],
            words
        );
        match check {
            0 => assert_eq!(info.direction, Direction::Reserved(0)),
            1 => assert_eq!(info.midi1, Midi1Port::Reserved(3)),
            2 => assert_eq!(info.id.get(), 0x23),
            3 => assert_eq!(info.first_group.get(), 0x10),
            4 => assert_eq!(info.groups_spanned.get(), 0),
            _ => assert_eq!(info.groups_spanned.get(), 0x11),
        }
    }
    // Function Block Name: a block above 31.
    assert!(matches!(
        decode(Ump([0xF012_2000, 0, 0, 0])),
        Ok(Message::Stream(Stream::FunctionBlockName { .. }))
    ));
}

#[test]
fn what_cannot_be_framed_is_unknown() {
    // A MIDI 1.0 data byte drawn `0ddddddd` with the top bit set is malformed:
    // MTC, Song Position, Song Select, SysEx 7 data within `# of bytes`.
    unknown([0x15F1_DA00, 0, 0, 0]);
    unknown([0x15F2_C546, 0, 0, 0]);
    unknown([0x15F2_45C6, 0, 0, 0]);
    unknown([0x15F3_8900, 0, 0, 0]);
    unknown([0x3201_8000, 0, 0, 0]);
    unknown([0x3203_0001, 0x8000_0000, 0, 0]);
    unknown([0x3206_0000, 0x0000_0080, 0, 0]);
    // SysEx 8: `# of bytes` 0 is not valid, 0xF is only for an End (§7.8).
    unknown([0x5400_0700, 0, 0, 0]);
    unknown([0x540F_0700, 0, 0, 0]);
    unknown([0x541F_0700, 0, 0, 0]);
    unknown([0x542F_0700, 0, 0, 0]);
    // Flex: Format is 0 and Address is 1 for the setup messages (§7.5.3 to
    // §7.5.5), which a Reserved address or a Channel one is not.
    unknown([0xD650_0000, 0, 0, 0]);
    unknown([0xD600_0000, 0, 0, 0]);
    unknown([0xD600_0001, 0, 0, 0]);
    unknown([0xD600_0002, 0, 0, 0]);
    unknown([0xD620_0000, 0, 0, 0]);
    // Stream messages that are not multi-packet have Format 0.
    unknown([0xF400_0101, 0, 0, 0]);
    unknown([0xF420_0000, 0, 0, 0]);
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

/// Every one of the 256 numbers round-trips through the controller type, and a
/// number Appendix A defines is never held as `Reserved`, so equal numbers are
/// equal values.
///
/// Mutation record: `From<u8>` mapping 74 to `Reserved(74)` -> this test;
/// `number` returning the discriminant instead of the wire number for
/// `Reserved` -> this test.
#[test]
fn a_registered_per_note_controller_number_round_trips() {
    for n in 0..=255u8 {
        let c = RegisteredPerNoteController::from(n);
        assert_eq!(c.number(), n);
        assert_eq!(u8::from(c), n);
        if let RegisteredPerNoteController::Reserved(r) = c {
            assert_eq!(r, n);
            let defined = [
                1, 2, 3, 7, 8, 10, 11, 70, 71, 72, 73, 74, 75, 76, 77, 78, 79,
            ]
            .contains(&n)
                || (91..=95).contains(&n);
            assert!(!defined, "{n} is defined but held as Reserved");
        }
    }
}
