//! Every table of M2-104 v1.1.2 that maps a number to a named value and that
//! `spec_words.rs` leaves partly open, pinned by literal numbers copied from
//! the table, in both directions. A wrong number used consistently by
//! `decode` and `encode` passes every round trip, so these are the only tests
//! that see it. None of them uses the `midi2` oracle.
//!
//! Tables here: 16 (Flex text, all 18 statuses), 13 and 15 (the accidental),
//! and the Function Block Info fields of §7.1.8 (User Interface Hint, Direction,
//! MIDI 1.0) and the Protocol field of §7.1.6 (Figures 18 and 19).
//! Tables pinned elsewhere: Registered Per-Note Controllers (Table 22,
//! `spec_words.rs`), the MIDI 1.0 controller roster (`cc_number.rs`).
//! Every other number-to-name table is held by `spec_words.rs`.
//!
//! Mutation record. Each swap of two neighbouring numbers was applied to
//! `decode.rs` and `encode.rs` together (a symmetric mistake), and to each
//! alone, in a copy with `tests/midi2_oracle.rs` and the `midi2` dev-dependency
//! deleted; each failed a test, and was restored:
//! - Flex text statuses 0x05/0x06 (Composer, Lyricist), 0x06/0x07, 0x07/0x08,
//!   0x08/0x09, 0x09/0x0A, 0x0A/0x0B -> `flex_text_statuses_are_table_16` when
//!   symmetric; alone, that test and the round-trip properties
//!   (`properties`, which reach every kind through the widened generator);
//! - `Accidental` 0x1/0x2 (Sharp, DoubleSharp) and 0x2/0xE (DoubleSharp,
//!   DoubleFlat) -> `accidentals_are_tables_13_and_15`;
//! - `UiHint` 1/2 (Receiver, Sender) -> `function_block_fields`;
//! - `Direction` 0b01/0b10 and 0b10/0b11, `Midi1Port` 1/2 and 0/1 ->
//!   `function_block_fields`; `Protocol` 0x01/0x02 ->
//!   `protocols_are_figures_18_and_19`.

use tutti_ump::{
    Accidental, Alteration, AlterationKind, ChordBass, ChordName, ChordPitch, ChordType, Direction,
    FirstGroup, Flex, FlexAddress, FlexTextKind, Form, FunctionBlockId, FunctionBlockInfo,
    GroupSpan, Letter, Message, Midi1Port, MidiGroup, Protocol, Stream, StreamConfig, U4, UiHint,
    Ump, decode,
};

/// Decode gives the message, and encode gives the words back.
fn pin(words: [u32; 4], message: Message) {
    assert_eq!(decode(Ump(words)), Ok(message), "decode {words:08x?}");
    assert_eq!(Ump::from(message), Ump(words), "encode {message:?}");
}

// Table 16, as (Status Bank, Status, Message). Complete (Format 0), Address 1
// (Group), group 0, no text: word 1 is `0xD010_bbss` for bank `bb`, status `ss`.
#[test]
fn flex_text_statuses_are_table_16() {
    use FlexTextKind as K;
    let table: [(u32, K); 18] = [
        (0x0100, K::UnknownMetadata),
        (0x0101, K::ProjectName),
        (0x0102, K::CompositionName),
        (0x0103, K::ClipName),
        (0x0104, K::Copyright),
        (0x0105, K::Composer),
        (0x0106, K::Lyricist),
        (0x0107, K::Arranger),
        (0x0108, K::Publisher),
        (0x0109, K::PrimaryPerformer),
        (0x010A, K::AccompanyingPerformer),
        (0x010B, K::RecordingDate),
        (0x010C, K::RecordingLocation),
        (0x0200, K::UnknownPerformance),
        (0x0201, K::Lyrics),
        (0x0202, K::LyricsLanguage),
        (0x0203, K::Ruby),
        (0x0204, K::RubyLanguage),
    ];
    for (bank_status, kind) in table {
        pin(
            [0xD010_0000 | bank_status, 0, 0, 0],
            Message::Flex(
                MidiGroup::FIRST,
                Flex::Text {
                    address: FlexAddress::Group,
                    kind,
                    form: Form::Complete,
                    bytes: [0; 12],
                },
            ),
        );
    }
    // Banks 0x03 to 0xFF are reserved.
    for reserved in [0xD010_0301, 0xD010_FF01] {
        assert!(decode(Ump([reserved, 0, 0, 0])).is_err(), "{reserved:08x}");
    }
    // Statuses past the last defined one in each text bank are reserved.
    for undefined in [0xD010_010D, 0xD010_0205, 0xD010_01FF, 0xD010_02FF] {
        assert!(
            decode(Ump([undefined, 0, 0, 0])).is_err(),
            "{undefined:08x}"
        );
    }
}

// Tables 13 (tonic) and 15 (bass): the 4-bit two's complement values. Word 2
// holds the tonic: accidental in 31-28, note C (3) in 27-24, type 0x01 in
// 23-16. Word 4 holds the bass the same way.
#[test]
fn accidentals_are_tables_13_and_15() {
    let table: [(u32, Accidental); 5] = [
        (0x0, Accidental::Natural),
        (0x1, Accidental::Sharp),
        (0x2, Accidental::DoubleSharp),
        (0xF, Accidental::Flat),
        (0xE, Accidental::DoubleFlat),
    ];
    let no_alteration = Alteration {
        kind: AlterationKind::None,
        degree: U4::new(0).unwrap(),
    };
    let major = ChordType::new(0x01).unwrap();
    let clear = ChordType::new(0x00).unwrap();
    let chord = |tonic_accidental, pitch| {
        Message::Flex(
            MidiGroup::FIRST,
            Flex::SetChordName(
                FlexAddress::Group,
                ChordName {
                    tonic_accidental,
                    tonic: Some(Letter::C),
                    chord_type: major,
                    alterations: [no_alteration; 4],
                    bass: ChordBass {
                        pitch,
                        chord_type: clear,
                        alterations: [no_alteration; 2],
                    },
                },
            ),
        )
    };
    for (nibble, accidental) in table {
        // Tonic, with the bass the tonic itself (sharps/flats -8, note 0).
        pin(
            [0xD010_0006, nibble << 28 | 0x0301_0000, 0, 0x8000_0000],
            chord(accidental, None),
        );
        // Bass: C with this accidental over a natural C chord.
        pin(
            [0xD010_0006, 0x0301_0000, 0, nibble << 28 | 0x0300_0000],
            chord(
                Accidental::Natural,
                Some(ChordPitch {
                    accidental,
                    letter: Letter::C,
                }),
            ),
        );
    }
}

// §7.1.8, Figure 22. Block 0, inactive; word 2 first group 0, one group.
// Word 1 is `0xF011_0000 | ui_hint << 4 | midi1 << 2 | direction`.
#[test]
fn function_block_fields() {
    let info = |direction, ui_hint, midi1| {
        Message::Stream(Stream::FunctionBlockInfo(FunctionBlockInfo {
            active: false,
            id: FunctionBlockId::new(0).unwrap(),
            direction,
            ui_hint,
            midi1,
            first_group: FirstGroup::new(0).unwrap(),
            groups_spanned: GroupSpan::new(1).unwrap(),
            midi_ci_version: 0,
            max_sysex8_streams: 0,
        }))
    };
    let word2 = 0x0001_0000;
    // User Interface Hint: 0b00 unknown, 0b01 receiver, 0b10 sender, 0b11 both.
    for (bits, hint) in [
        (0b00, UiHint::Unknown),
        (0b01, UiHint::Receiver),
        (0b10, UiHint::Sender),
        (0b11, UiHint::Both),
    ] {
        pin(
            [0xF011_0001 | bits << 4, word2, 0, 0],
            info(Direction::Input, hint, Midi1Port::No),
        );
    }
    // Direction: 0b01 input, 0b10 output, 0b11 bidirectional (0b00 reserved).
    for (bits, direction) in [
        (0b01, Direction::Input),
        (0b10, Direction::Output),
        (0b11, Direction::Bidirectional),
        (0b00, Direction::Reserved(0)),
    ] {
        pin(
            [0xF011_0000 | bits, word2, 0, 0],
            info(direction, UiHint::Unknown, Midi1Port::No),
        );
    }
    // MIDI 1.0: 0x00 not, 0x01 yes unrestricted, 0x02 yes restricted to
    // 31.25 kbps, 0x03 reserved.
    for (bits, port) in [
        (0x00, Midi1Port::No),
        (0x01, Midi1Port::Unrestricted),
        (0x02, Midi1Port::Restricted),
        (0x03, Midi1Port::Reserved(3)),
    ] {
        pin(
            [0xF011_0001 | bits << 2, word2, 0, 0],
            info(Direction::Input, UiHint::Unknown, port),
        );
    }
}

// §7.1.6, Figures 18 and 19: the protocol is the byte in bits 15-8, 0x01 for
// MIDI 1.0 and 0x02 for MIDI 2.0.
#[test]
fn protocols_are_figures_18_and_19() {
    for (shift, protocol) in [(0x01, Protocol::Midi1), (0x02, Protocol::Midi2)] {
        let config = StreamConfig {
            protocol,
            rx_jr: false,
            tx_jr: false,
        };
        // Status 0x05 is the request, 0x06 the notification.
        pin(
            [0xF005_0000 | shift << 8, 0, 0, 0],
            Message::Stream(Stream::StreamConfigurationRequest(config)),
        );
        pin(
            [0xF006_0000 | shift << 8, 0, 0, 0],
            Message::Stream(Stream::StreamConfigurationNotification(config)),
        );
    }
}
