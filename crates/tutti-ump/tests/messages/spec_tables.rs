//! Every table of M2-104 v1.1.2 that maps a number to a named value and that
//! `spec_words.rs` leaves partly open, pinned by literal numbers copied from
//! the table, in both directions. A wrong number used consistently by
//! `decode` and `encode` passes every round trip, so these are the only tests
//! that see it. None of them reads another implementation.
//!
//! Beyond the tables, the last tests pin fields whose values `spec_words.rs`
//! leaves equal or only set together (the UMP version, the Endpoint Info and
//! Discovery flags, the Metronome bytes), where a swap would not show.
//!
//! Tables here: 16 (Flex text, all 18 statuses), 13 and 15 (the accidental),
//! and the Function Block Info fields of §7.1.8 (User Interface Hint, Direction,
//! MIDI 1.0) and the Protocol field of §7.1.6 (Figures 18 and 19).
//! Tables pinned elsewhere: Registered Per-Note Controllers (Table 22,
//! `spec_words.rs`), the MIDI 1.0 controller roster (`cc_number.rs`).
//! Every other number-to-name table is held by `spec_words.rs`.

use tutti_ump::{
    Accidental, Alteration, AlterationKind, ChordBass, ChordName, ChordPitch, ChordType, Direction,
    DiscoveryFilter, EndpointInfo, FirstGroup, Flex, FlexAddress, FlexTextKind, Form,
    FunctionBlockId, FunctionBlockInfo, GroupSpan, Letter, Message, Metronome, Midi1Port,
    MidiGroup, Protocol, Stream, StreamConfig, U4, U7, UiHint, Ump, UmpVersion, decode,
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

// §7.1.1 and §7.1.2, Figures 10 and 11: the version is major in bits 15-8 and
// minor in bits 7-0. `spec_words.rs` uses 1.1 throughout, which hides a swap.
#[test]
fn the_version_is_major_then_minor() {
    let version = UmpVersion { major: 1, minor: 2 };
    pin(
        [0xF000_0102, 0, 0, 0],
        Message::Stream(Stream::EndpointDiscovery {
            version,
            filter: DiscoveryFilter {
                endpoint_info: false,
                device_identity: false,
                endpoint_name: false,
                product_instance_id: false,
                stream_configuration: false,
            },
        }),
    );
    pin(
        [0xF001_0102, 0, 0, 0],
        Message::Stream(Stream::EndpointInfo(EndpointInfo {
            version,
            static_function_blocks: false,
            function_blocks: U7::new(0).unwrap(),
            midi2: false,
            midi1: false,
            rx_jr: false,
            tx_jr: false,
        })),
    );
}

// Figure 11: S bit 31, function blocks 30-24, M2 bit 9, M1 bit 8, RXJR bit 1,
// TXJR bit 0. Each alone, so that no two flags can trade places unseen.
#[test]
fn endpoint_info_flags_each_alone() {
    let none = EndpointInfo {
        version: UmpVersion { major: 1, minor: 1 },
        static_function_blocks: false,
        function_blocks: U7::new(0).unwrap(),
        midi2: false,
        midi1: false,
        rx_jr: false,
        tx_jr: false,
    };
    let case = |word: u32, info: EndpointInfo| {
        pin(
            [0xF001_0101, word, 0, 0],
            Message::Stream(Stream::EndpointInfo(info)),
        );
    };
    case(
        0x8000_0000,
        EndpointInfo {
            static_function_blocks: true,
            ..none
        },
    );
    case(
        0x0500_0000,
        EndpointInfo {
            function_blocks: U7::new(5).unwrap(),
            ..none
        },
    );
    case(
        0x0000_0200,
        EndpointInfo {
            midi2: true,
            ..none
        },
    );
    case(
        0x0000_0100,
        EndpointInfo {
            midi1: true,
            ..none
        },
    );
    case(
        0x0000_0002,
        EndpointInfo {
            rx_jr: true,
            ..none
        },
    );
    case(
        0x0000_0001,
        EndpointInfo {
            tx_jr: true,
            ..none
        },
    );
}

// Figure 10: the filter is bit 0 endpoint info, 1 device identity, 2 endpoint
// name, 3 product instance id, 4 stream configuration.
#[test]
fn discovery_filter_bits_each_alone() {
    let none = DiscoveryFilter {
        endpoint_info: false,
        device_identity: false,
        endpoint_name: false,
        product_instance_id: false,
        stream_configuration: false,
    };
    let case = |word: u32, filter: DiscoveryFilter| {
        pin(
            [0xF000_0101, word, 0, 0],
            Message::Stream(Stream::EndpointDiscovery {
                version: UmpVersion { major: 1, minor: 1 },
                filter,
            }),
        );
    };
    case(
        0x01,
        DiscoveryFilter {
            endpoint_info: true,
            ..none
        },
    );
    case(
        0x02,
        DiscoveryFilter {
            device_identity: true,
            ..none
        },
    );
    case(
        0x04,
        DiscoveryFilter {
            endpoint_name: true,
            ..none
        },
    );
    case(
        0x08,
        DiscoveryFilter {
            product_instance_id: true,
            ..none
        },
    );
    case(
        0x10,
        DiscoveryFilter {
            stream_configuration: true,
            ..none
        },
    );
}

// Figures 18 and 19: RXJR is bit 1 and TXJR bit 0, each alone.
#[test]
fn jitter_reduction_flags_each_alone() {
    for (bits, rx_jr, tx_jr) in [(0b10, true, false), (0b01, false, true)] {
        pin(
            [0xF005_0200 | bits, 0, 0, 0],
            Message::Stream(Stream::StreamConfigurationRequest(StreamConfig {
                protocol: Protocol::Midi2,
                rx_jr,
                tx_jr,
            })),
        );
    }
}

// Figure 73 has accent 2 equal to subdivision 1 and accent 3 equal to
// subdivision 2, which hides those swaps. Six different values: word 2 is
// clocks, accent 1, 2, 3; word 3 is subdivision 1 and 2 (§7.5.3).
#[test]
fn metronome_fields_in_order() {
    pin(
        [0xD010_0002, 0x1803_0405, 0x0607_0000, 0],
        Message::Flex(
            MidiGroup::FIRST,
            Flex::SetMetronome(Metronome {
                clocks_per_click: 24,
                accent_1: 3,
                accent_2: 4,
                accent_3: 5,
                subdivision_1: 6,
                subdivision_2: 7,
            }),
        ),
    );
}
