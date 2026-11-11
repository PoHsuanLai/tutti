//! Every message against words derived by hand from M2-104 Appendix F and §7.
//!
//! Each case writes the packet's fields into the figure's bit positions in a
//! comment, then the words. Nothing here calls the codec to produce an
//! expected value.

use tutti_ump::{
    Accidental, Address, Alteration, AlterationKind, Attribute, Bank, BlockFilter, BlockSelect,
    CCNumber, ChordBass, ChordName, ChordPitch, ChordType, Data128, DeltaTicks, DeviceIdentity,
    Direction, DiscoveryFilter, EndpointInfo, FirstGroup, Flex, FlexAddress, FlexTextKind, Form,
    FunctionBlockId, FunctionBlockInfo, GroupSpan, KeySignature, Letter, Message, Metronome,
    Midi1Port, MidiChannel, MidiGroup, MixedDataHeader, NoteNumber, Protocol,
    RegisteredPerNoteController, SharpsFlats, Stream, StreamConfig, Sysex7Packet, Sysex8Packet,
    SysexStatus, System, TimeSignature, U4, U7, U14, UiHint, Ump, UmpVersion, Utility, Voice1,
    Voice2, decode,
};

fn g(n: u8) -> MidiGroup {
    MidiGroup::of(n)
}
fn addr(group: u8, channel: u8) -> Address {
    Address {
        group: g(group),
        channel: MidiChannel::of(channel),
    }
}
fn note(n: u8) -> NoteNumber {
    NoteNumber::new(n).unwrap()
}
fn u7(v: u8) -> U7 {
    U7::new(v).unwrap()
}
fn u14(v: u16) -> U14 {
    U14::new(v).unwrap()
}
fn alt(kind: AlterationKind, degree: u8) -> Alteration {
    Alteration {
        kind,
        degree: U4::new(degree).unwrap(),
    }
}
fn none() -> Alteration {
    alt(AlterationKind::None, 0)
}

/// Decode gives the message, and encode gives the words back.
fn pin(words: [u32; 4], message: Message) {
    assert_eq!(decode(Ump(words)), Ok(message), "decode {words:08x?}");
    assert_eq!(Ump::from(message), Ump(words), "encode {message:?}");
}

// F.1.1, Table 26. Bits 31-28 MT, 27-24 reserved, 23-20 status, then data.
#[test]
fn utility() {
    pin([0x0000_0000, 0, 0, 0], Message::Utility(Utility::Noop));
    // JR Clock: status 1, time in bits 15-0.
    pin(
        [0x0010_1234, 0, 0, 0],
        Message::Utility(Utility::JrClock(0x1234)),
    );
    // JR Timestamp: status 2.
    pin(
        [0x0020_ABCD, 0, 0, 0],
        Message::Utility(Utility::JrTimestamp(0xABCD)),
    );
    // Delta Clockstamp Ticks Per Quarter Note: status 3, 480 = 0x01E0.
    pin(
        [0x0030_01E0, 0, 0, 0],
        Message::Utility(Utility::TicksPerQuarterNote(480)),
    );
    // Delta Clockstamp: status 4, 20 bits in 19-0.
    pin(
        [0x0041_2345, 0, 0, 0],
        Message::Utility(Utility::DeltaClockstamp(DeltaTicks::new(0x1_2345).unwrap())),
    );
    pin(
        [0x004F_FFFF, 0, 0, 0],
        Message::Utility(Utility::DeltaClockstamp(DeltaTicks::new(0xF_FFFF).unwrap())),
    );
}

// F.1.2, Table 27, group 5: MT 1, group in 27-24, status byte in 23-16.
#[test]
fn system() {
    let sys = |s| Message::System(g(5), s);
    // MTC: status F1, 0nnndddd = 0101_1010.
    pin([0x15F1_5A00, 0, 0, 0], sys(System::TimeCode(u7(0x5A))));
    // Song Position: LSB 0x45 then MSB 0x46, so (0x46 << 7) | 0x45 = 0x2345.
    pin(
        [0x15F2_4546, 0, 0, 0],
        sys(System::SongPosition(u14(0x2345))),
    );
    pin([0x15F3_0900, 0, 0, 0], sys(System::SongSelect(u7(9))));
    pin([0x15F6_0000, 0, 0, 0], sys(System::TuneRequest));
    pin([0x15F8_0000, 0, 0, 0], sys(System::TimingClock));
    pin([0x15FA_0000, 0, 0, 0], sys(System::Start));
    pin([0x15FB_0000, 0, 0, 0], sys(System::Continue));
    pin([0x15FC_0000, 0, 0, 0], sys(System::Stop));
    pin([0x15FE_0000, 0, 0, 0], sys(System::ActiveSensing));
    pin([0x15FF_0000, 0, 0, 0], sys(System::Reset));
}

// F.1.3, Table 28, group 3 channel 5: MT 2, group, opcode in 23-20, channel in
// 19-16, byte 3 in 15-8, byte 4 in 7-0.
#[test]
fn midi1_channel_voice() {
    let v = |v| Message::Midi1(addr(3, 5), v);
    pin(
        [0x2385_3C40, 0, 0, 0],
        v(Voice1::NoteOff {
            note: note(60),
            velocity: u7(64),
        }),
    );
    pin(
        [0x2395_3C7F, 0, 0, 0],
        v(Voice1::NoteOn {
            note: note(60),
            velocity: u7(127),
        }),
    );
    // Velocity 0 is a Note On to the codec; translation reads it as an off.
    pin(
        [0x2395_3C00, 0, 0, 0],
        v(Voice1::NoteOn {
            note: note(60),
            velocity: u7(0),
        }),
    );
    pin(
        [0x23A5_3C10, 0, 0, 0],
        v(Voice1::PolyPressure {
            note: note(60),
            pressure: u7(16),
        }),
    );
    // CC 74 = 0x4A, value 0x20.
    pin(
        [0x23B5_4A20, 0, 0, 0],
        v(Voice1::ControlChange {
            controller: CCNumber::of(74),
            value: u7(0x20),
        }),
    );
    pin([0x23C5_0900, 0, 0, 0], v(Voice1::ProgramChange(u7(9))));
    pin([0x23D5_3300, 0, 0, 0], v(Voice1::ChannelPressure(u7(0x33))));
    // Pitch Bend 0x2345: LSB 0x45 in byte 3, MSB 0x46 in byte 4.
    pin([0x23E5_4546, 0, 0, 0], v(Voice1::PitchBend(u14(0x2345))));
    pin([0x23E5_0040, 0, 0, 0], v(Voice1::PitchBend(u14(0x2000))));
}

// F.2.1, Table 29, group 2: MT 3, status in 23-20, # of bytes in 19-16, data
// from byte 3.
#[test]
fn sysex7() {
    let p = |status, data: &[u8]| Message::Sysex7(g(2), Sysex7Packet::new(status, data).unwrap());
    pin(
        [0x3203_7E7F, 0x0900_0000, 0, 0],
        p(SysexStatus::Complete, &[0x7E, 0x7F, 0x09]),
    );
    pin([0x3200_0000, 0, 0, 0], p(SysexStatus::Complete, &[]));
    pin(
        [0x3216_0102, 0x0304_0506, 0, 0],
        p(SysexStatus::Start, &[1, 2, 3, 4, 5, 6]),
    );
    pin(
        [0x3226_0102, 0x0304_0506, 0, 0],
        p(SysexStatus::Continue, &[1, 2, 3, 4, 5, 6]),
    );
    pin([0x3231_7F00, 0, 0, 0], p(SysexStatus::End, &[0x7F]));
}

// F.3.1, Table 31, group 4: MT 5, status in 23-20, `# of bytes` in 19-16 (it
// counts the stream id), stream id in byte 3, data from byte 4. §7.8.
#[test]
fn sysex8() {
    let p = |status, stream, data: &[u8]| {
        Message::Data128(
            g(4),
            Data128::Sysex8(Sysex8Packet::new(status, stream, data).unwrap()),
        )
    };
    // Stream 7, data AA BB: `# of bytes` = 3.
    pin(
        [0x5403_07AA, 0xBB00_0000, 0, 0],
        p(SysexStatus::Complete, 7, &[0xAA, 0xBB]),
    );
    // The stream id alone: `# of bytes` = 1, which §7.8.1 names for an End.
    pin([0x5431_0700, 0, 0, 0], p(SysexStatus::End, 7, &[]));
    // Thirteen data bytes fill the packet: `# of bytes` = 14.
    pin(
        [0x541E_0701, 0x0203_0405, 0x0607_0809, 0x0A0B_0C0D],
        p(
            SysexStatus::Start,
            7,
            &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13],
        ),
    );
    pin([0x542E_0701, 0x0203_0405, 0x0607_0809, 0x0A0B_0C0D], {
        p(
            SysexStatus::Continue,
            7,
            &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13],
        )
    });
    // §7.8.1: an End with `# of bytes` 0xF aborts the stream.
    pin(
        [0x543F_0700, 0, 0, 0],
        Message::Data128(g(4), Data128::Sysex8Abort { stream_id: 7 }),
    );
}

// §7.9, Figure 86, group 4: MT 5, status 8 and 9, MDS id in 19-16.
#[test]
fn mixed_data_set() {
    pin(
        // word 1: id 5, 0x0020 valid bytes; word 2: 2 chunks, chunk 1;
        // word 3: manufacturer 0x8213, device 0xFFFF; word 4: sub ids.
        [0x5485_0020, 0x0002_0001, 0x8213_FFFF, 0x0102_0304],
        Message::Data128(
            g(4),
            Data128::MixedDataHeader(MixedDataHeader {
                mds_id: U4::new(5).unwrap(),
                chunk_bytes: 0x20,
                chunks: 2,
                chunk: 1,
                manufacturer: 0x8213,
                device: 0xFFFF,
                sub_id_1: 0x0102,
                sub_id_2: 0x0304,
            }),
        ),
    );
    pin(
        // id 5, fourteen payload bytes: two in word 1, then three words.
        [0x5495_0102, 0x0304_0506, 0x0708_090A, 0x0B0C_0D0E],
        Message::Data128(
            g(4),
            Data128::MixedDataPayload {
                mds_id: U4::new(5).unwrap(),
                bytes: [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14],
            },
        ),
    );
}

// F.2.2, Table 30, group 1 channel 2: MT 4, opcode in 23-20, channel in 19-16,
// byte 3 (index) in 15-8, byte 4 in 7-0; word 2 is the data.
#[test]
fn midi2_note_and_pressure() {
    let v = |v| Message::Midi2(addr(1, 2), v);
    // Note On, note 60: attribute type 3 (pitch 7.9) in byte 4; word 2 is the
    // velocity in the high half and the attribute data in the low half.
    pin(
        [0x4192_3C03, 0xABCD_1234, 0, 0],
        v(Voice2::NoteOn {
            note: note(60),
            velocity: 0xABCD,
            attribute: Attribute::Pitch(0x1234),
        }),
    );
    pin(
        [0x4182_3C00, 0x8000_0000, 0, 0],
        v(Voice2::NoteOff {
            note: note(60),
            velocity: 0x8000,
            attribute: Attribute::None,
        }),
    );
    pin(
        [0x4192_3C01, 0x0000_BEEF, 0, 0],
        v(Voice2::NoteOn {
            note: note(60),
            velocity: 0,
            attribute: Attribute::Manufacturer(0xBEEF),
        }),
    );
    pin(
        [0x4182_3C02, 0x1111_2222, 0, 0],
        v(Voice2::NoteOff {
            note: note(60),
            velocity: 0x1111,
            attribute: Attribute::Profile(0x2222),
        }),
    );
    pin(
        [0x41A2_3C00, 0x1122_3344, 0, 0],
        v(Voice2::PolyPressure {
            note: note(60),
            value: 0x1122_3344,
        }),
    );
    pin(
        [0x4162_3C00, 0x8000_0000, 0, 0],
        v(Voice2::PerNotePitchBend {
            note: note(60),
            value: 0x8000_0000,
        }),
    );
}

#[test]
fn midi2_per_note_and_controllers() {
    let v = |v| Message::Midi2(addr(1, 2), v);
    // Registered Per-Note Controller 3 (pitch 7.25), assignable 0xC8: the
    // controller number is a full byte.
    pin(
        [0x4102_3C03, 0x3C00_0000, 0, 0],
        v(Voice2::RegisteredPerNote {
            note: note(60),
            controller: RegisteredPerNoteController::Pitch,
            value: 0x3C00_0000,
        }),
    );
    pin(
        [0x4112_3CC8, 0x0000_0001, 0, 0],
        v(Voice2::AssignablePerNote {
            note: note(60),
            controller: 0xC8,
            value: 1,
        }),
    );
    // RPN bank 1 index 2; NRPN the same with opcode 3.
    pin(
        [0x4122_0102, 0xDEAD_BEEF, 0, 0],
        v(Voice2::Registered {
            bank: u7(1),
            index: u7(2),
            value: 0xDEAD_BEEF,
        }),
    );
    pin(
        [0x4132_0102, 0xDEAD_BEEF, 0, 0],
        v(Voice2::Assignable {
            bank: u7(1),
            index: u7(2),
            value: 0xDEAD_BEEF,
        }),
    );
    // Relative controllers carry a two's complement: -1 is all ones.
    pin(
        [0x4142_0102, 0xFFFF_FFFF, 0, 0],
        v(Voice2::RelativeRegistered {
            bank: u7(1),
            index: u7(2),
            delta: -1,
        }),
    );
    pin(
        [0x4152_0102, 0x0000_0010, 0, 0],
        v(Voice2::RelativeAssignable {
            bank: u7(1),
            index: u7(2),
            delta: 16,
        }),
    );
    pin(
        [0x41B2_0700, 0x1122_3344, 0, 0],
        v(Voice2::ControlChange {
            controller: CCNumber::of(7),
            value: 0x1122_3344,
        }),
    );
    pin(
        [0x41D2_0000, 0xC000_0000, 0, 0],
        v(Voice2::ChannelPressure(0xC000_0000)),
    );
    pin(
        [0x41E2_0000, 0x8000_0000, 0, 0],
        v(Voice2::PitchBend(0x8000_0000)),
    );
    // Per-Note Management: option flags in byte 4, D is bit 1 and S bit 0.
    pin(
        [0x41F2_3C03, 0, 0, 0],
        v(Voice2::PerNoteManagement {
            note: note(60),
            detach: true,
            reset: true,
        }),
    );
    pin(
        [0x41F2_3C02, 0, 0, 0],
        v(Voice2::PerNoteManagement {
            note: note(60),
            detach: true,
            reset: false,
        }),
    );
    pin(
        [0x41F2_3C01, 0, 0, 0],
        v(Voice2::PerNoteManagement {
            note: note(60),
            detach: false,
            reset: true,
        }),
    );
}

// M2-104 section 7.4: the messages that address a note (statuses 0x0, 0x1, 0x6,
// 0x8, 0x9, 0xA and 0xF) carry it in byte 3 of the first word, here 60; the
// channel-wide ones (statuses 0x2 to 0x5, 0xB to 0xE) have no note, whatever
// their byte 3 holds.
#[test]
fn the_note_a_message_addresses() {
    let words = |status: u32| [0x4100_0000 | status << 20 | 0x2_0000 | 60 << 8, 0, 0, 0];
    // Status 0x7 is undefined (Table 30).
    for status in (0x0..=0xF).filter(|&status| status != 0x7) {
        let voice = match decode(Ump(words(status))) {
            Ok(Message::Midi2(_, voice)) => voice,
            other => panic!("status {status:#x}: {other:?}"),
        };
        let addresses_a_note = matches!(status, 0x0 | 0x1 | 0x6 | 0x8 | 0x9 | 0xA | 0xF);
        assert_eq!(
            voice.note(),
            addresses_a_note.then(|| note(60)),
            "status {status:#x}: {voice:?}"
        );
    }
}

// Figure 64: word 1 option flags in 7-0 with the Bank Valid bit B at bit 0;
// word 2 is program (30-24), bank MSB (14-8), bank LSB (6-0).
#[test]
fn midi2_program_change() {
    let v = |v| Message::Midi2(addr(1, 2), v);
    pin(
        [0x41C2_0001, 0x0900_3424, 0, 0],
        v(Voice2::ProgramChange {
            program: u7(9),
            bank: Some(Bank {
                msb: u7(0x34),
                lsb: u7(0x24),
            }),
        }),
    );
    pin(
        [0x41C2_0000, 0x0900_0000, 0, 0],
        v(Voice2::ProgramChange {
            program: u7(9),
            bank: None,
        }),
    );
}

// §7.5, Figures 69 to 75, group 6: word 1 is MT D, group, form (23-22),
// address (21-20), channel (19-16), status bank (15-8), status (7-0).
#[test]
fn flex_setup() {
    let f = |f| Message::Flex(g(6), f);
    // Set Tempo, 120 bpm: 0.5 s per quarter = 50,000,000 units of 10 ns.
    pin(
        [0xD610_0000, 0x02FA_F080, 0, 0],
        f(Flex::SetTempo(50_000_000)),
    );
    // 3/4: numerator 3, denominator 2 (a quarter), 8 thirty-seconds in 24
    // clocks.
    pin(
        [0xD610_0001, 0x0302_0800, 0, 0],
        f(Flex::SetTimeSignature(TimeSignature {
            numerator: 3,
            denominator: 2,
            thirty_seconds: 8,
        })),
    );
    // Figure 73, 5/4 as 3+2 with eighth-note subdivisions.
    pin(
        [0xD610_0002, 0x1803_0200, 0x0200_0000, 0],
        f(Flex::SetMetronome(Metronome {
            clocks_per_click: 24,
            accent_1: 3,
            accent_2: 2,
            accent_3: 0,
            subdivision_1: 2,
            subdivision_2: 0,
        })),
    );
}

#[test]
fn flex_key_and_chord() {
    let f = |f| Message::Flex(g(6), f);
    let key = |sf: i8, tonic| KeySignature {
        sharps_flats: SharpsFlats::new(sf),
        tonic,
    };
    // Five sharps, tonic D (4): D sharp. Nibbles 5 and 4 lead word 2.
    pin(
        [0xD610_0005, 0x5400_0000, 0, 0],
        f(Flex::SetKeySignature(
            FlexAddress::Group,
            key(5, Some(Letter::D)),
        )),
    );
    // Four flats is 0xC. Addressed to channel 3: address 0, channel nibble 3.
    pin(
        [0xD603_0005, 0xC400_0000, 0, 0],
        f(Flex::SetKeySignature(
            FlexAddress::Channel(MidiChannel::of(3)),
            key(-4, Some(Letter::D)),
        )),
    );
    // -8 and tonic 0: unknown.
    pin(
        [0xD610_0005, 0x8000_0000, 0, 0],
        f(Flex::SetKeySignature(FlexAddress::Group, key(-8, None))),
    );

    // C major add 9, bass is the tonic. Word 2: sharps/flats 0, tonic 3,
    // type 0x01, alteration 1 = add (1) degree 9, alteration 2 none. Word 4:
    // bass sharps/flats -8, bass note 0.
    pin(
        [0xD610_0006, 0x0301_1900, 0x0000_0000, 0x8000_0000],
        f(Flex::SetChordName(
            FlexAddress::Group,
            ChordName {
                tonic_accidental: Accidental::Natural,
                tonic: Some(Letter::C),
                chord_type: ChordType::new(0x01).unwrap(),
                alterations: [alt(AlterationKind::Add, 9), none(), none(), none()],
                bass: ChordBass {
                    pitch: None,
                    chord_type: ChordType::new(0).unwrap(),
                    alterations: [none(), none()],
                },
            },
        )),
    );
    // E flat minor 7th (0x09), raise 5 and lower 7, over B flat major with the
    // third subtracted. Flat is 0xF; alteration 3 and 4 fill word 3.
    pin(
        [0xD610_0006, 0xF509_0000, 0x3547_0000, 0xF201_2300],
        f(Flex::SetChordName(
            FlexAddress::Group,
            ChordName {
                tonic_accidental: Accidental::Flat,
                tonic: Some(Letter::E),
                chord_type: ChordType::new(0x09).unwrap(),
                alterations: [
                    none(),
                    none(),
                    alt(AlterationKind::Raise, 5),
                    alt(AlterationKind::Lower, 7),
                ],
                bass: ChordBass {
                    pitch: Some(ChordPitch {
                        accidental: Accidental::Flat,
                        letter: Letter::B,
                    }),
                    chord_type: ChordType::new(0x01).unwrap(),
                    alterations: [alt(AlterationKind::Subtract, 3), none()],
                },
            },
        )),
    );
}

#[test]
fn flex_text() {
    let t = |address, kind, form, bytes| {
        Message::Flex(
            g(6),
            Flex::Text {
                address,
                kind,
                form,
                bytes,
            },
        )
    };
    // Project Name (bank 1, status 1), Complete, group address: "Hi".
    pin(
        [0xD610_0101, 0x4869_0000, 0, 0],
        t(
            FlexAddress::Group,
            FlexTextKind::ProjectName,
            Form::Complete,
            *b"Hi\0\0\0\0\0\0\0\0\0\0",
        ),
    );
    // Copyright (bank 1, status 4), Start (form 1, bit 22), channel 2.
    pin(
        [0xD642_0104, 0x436F_7079, 0x7269_6768, 0x7420_3230],
        t(
            FlexAddress::Channel(MidiChannel::of(2)),
            FlexTextKind::Copyright,
            Form::Start,
            *b"Copyright 20",
        ),
    );
    // Lyrics (bank 2, status 1), End (form 3), group address.
    pin(
        [0xD6D0_0201, 0x6C61_0000, 0, 0],
        t(
            FlexAddress::Group,
            FlexTextKind::Lyrics,
            Form::End,
            *b"la\0\0\0\0\0\0\0\0\0\0",
        ),
    );
    // Continue is form 2.
    pin(
        [0xD690_0203, 0x6162_6364, 0x6566_6768, 0x696A_6B6C],
        t(
            FlexAddress::Group,
            FlexTextKind::Ruby,
            Form::Continue,
            *b"abcdefghijkl",
        ),
    );
    // The last status of each text bank.
    pin(
        [0xD610_010C, 0, 0, 0],
        t(
            FlexAddress::Group,
            FlexTextKind::RecordingLocation,
            Form::Complete,
            [0; 12],
        ),
    );
    pin(
        [0xD610_0204, 0, 0, 0],
        t(
            FlexAddress::Group,
            FlexTextKind::RubyLanguage,
            Form::Complete,
            [0; 12],
        ),
    );
}

// §7.1, Figures 10 to 25: MT F, form in 27-26, status in 25-16.
#[test]
fn stream_endpoint_messages() {
    let s = Message::Stream;
    let v11 = UmpVersion { major: 1, minor: 1 };
    // Discovery, version 1.1, all five filter bits (word 2, bits 4-0).
    pin(
        [0xF000_0101, 0x0000_001F, 0, 0],
        s(Stream::EndpointDiscovery {
            version: v11,
            filter: DiscoveryFilter {
                endpoint_info: true,
                device_identity: true,
                endpoint_name: true,
                product_instance_id: true,
                stream_configuration: true,
            },
        }),
    );
    // Only `n` (bit 2) and `s` (bit 4).
    pin(
        [0xF000_0101, 0x0000_0014, 0, 0],
        s(Stream::EndpointDiscovery {
            version: v11,
            filter: DiscoveryFilter {
                endpoint_info: false,
                device_identity: false,
                endpoint_name: true,
                product_instance_id: false,
                stream_configuration: true,
            },
        }),
    );
    pin([0xF020_0000, 0, 0, 0], s(Stream::StartOfClip));
    pin([0xF021_0000, 0, 0, 0], s(Stream::EndOfClip));
}

#[test]
fn stream_endpoint_info() {
    // Word 2: S in bit 31, function blocks in 30-24, M2 bit 9, M1 bit 8, RXJR
    // bit 1, TXJR bit 0. Static, 3 blocks, M2, M1, rx, tx.
    pin(
        [0xF001_0101, 0x8300_0303, 0, 0],
        Message::Stream(Stream::EndpointInfo(EndpointInfo {
            version: UmpVersion { major: 1, minor: 1 },
            static_function_blocks: true,
            function_blocks: u7(3),
            midi2: true,
            midi1: true,
            rx_jr: true,
            tx_jr: true,
        })),
    );
    // M2 only, no JR: bit 9 alone.
    pin(
        [0xF001_0101, 0x2000_0200, 0, 0],
        Message::Stream(Stream::EndpointInfo(EndpointInfo {
            version: UmpVersion { major: 1, minor: 1 },
            static_function_blocks: false,
            function_blocks: u7(32),
            midi2: true,
            midi1: false,
            rx_jr: false,
            tx_jr: false,
        })),
    );
}

#[test]
fn stream_identity_and_names() {
    let b = |v: [u8; 4]| v.map(u7);
    // Figure 14: each id byte is 7 bits in a byte of its own. Manufacturer in
    // word 2 bytes 2-0, family and model in word 3, revision in word 4.
    pin(
        [0xF002_0000, 0x0000_206B, 0x0102_0304, 0x0506_0708],
        Message::Stream(Stream::DeviceIdentity(DeviceIdentity {
            manufacturer: [u7(0x00), u7(0x20), u7(0x6B)],
            family: [u7(1), u7(2)],
            model: [u7(3), u7(4)],
            revision: b([5, 6, 7, 8]),
        })),
    );
    // Endpoint Name: two bytes in word 1 (15-8, 7-0), then twelve.
    let name = |form: Form, bytes: &[u8; 14]| Message::Stream(Stream::EndpointName(form, *bytes));
    pin(
        [0xF003_5475, 0x7474_6900, 0, 0],
        name(Form::Complete, b"Tutti\0\0\0\0\0\0\0\0\0"),
    );
    pin(
        [0xF403_5475, 0x7474_6900, 0, 0],
        name(Form::Start, b"Tutti\0\0\0\0\0\0\0\0\0"),
    );
    pin(
        [0xF803_5475, 0x7474_6900, 0, 0],
        name(Form::Continue, b"Tutti\0\0\0\0\0\0\0\0\0"),
    );
    pin(
        [0xFC03_5475, 0x7474_6900, 0, 0],
        name(Form::End, b"Tutti\0\0\0\0\0\0\0\0\0"),
    );
    pin(
        [0xF004_3132, 0x3334_3536, 0x3738_3930, 0x4142_4344],
        Message::Stream(Stream::ProductInstanceId(
            Form::Complete,
            *b"1234567890ABCD",
        )),
    );
}

#[test]
fn stream_configuration() {
    // Figures 18 and 19: protocol in bits 15-8, RXJR bit 1, TXJR bit 0.
    pin(
        [0xF005_0203, 0, 0, 0],
        Message::Stream(Stream::StreamConfigurationRequest(StreamConfig {
            protocol: Protocol::Midi2,
            rx_jr: true,
            tx_jr: true,
        })),
    );
    pin(
        [0xF006_0100, 0, 0, 0],
        Message::Stream(Stream::StreamConfigurationNotification(StreamConfig {
            protocol: Protocol::Midi1,
            rx_jr: false,
            tx_jr: false,
        })),
    );
    pin(
        [0xF005_0201, 0, 0, 0],
        Message::Stream(Stream::StreamConfigurationRequest(StreamConfig {
            protocol: Protocol::Midi2,
            rx_jr: false,
            tx_jr: true,
        })),
    );
}

#[test]
fn stream_function_blocks() {
    // Figure 20: block number in 15-8, filter in 7-0 (`i` bit 0, `n` bit 1).
    pin(
        [0xF010_FF03, 0, 0, 0],
        Message::Stream(Stream::FunctionBlockDiscovery {
            block: BlockSelect::All,
            filter: BlockFilter {
                info: true,
                name: true,
            },
        }),
    );
    pin(
        [0xF010_0501, 0, 0, 0],
        Message::Stream(Stream::FunctionBlockDiscovery {
            block: BlockSelect::One(FunctionBlockId::new(5).unwrap()),
            filter: BlockFilter {
                info: true,
                name: false,
            },
        }),
    );
    // Figure 22: active bit 15, block 14-8, UI hint 5-4, MIDI 1.0 3-2,
    // direction 1-0; word 2: first group, groups spanned, MIDI-CI version,
    // SysEx 8 streams, a byte each. 0x8000 | 0x0300 | 0x0030 | 0x0008 | 0x0003.
    pin(
        [0xF011_833B, 0x0402_0110, 0, 0],
        Message::Stream(Stream::FunctionBlockInfo(FunctionBlockInfo {
            active: true,
            id: FunctionBlockId::new(3).unwrap(),
            direction: Direction::Bidirectional,
            ui_hint: UiHint::Both,
            midi1: Midi1Port::Restricted,
            first_group: FirstGroup::new(4).unwrap(),
            groups_spanned: GroupSpan::new(2).unwrap(),
            midi_ci_version: 1,
            max_sysex8_streams: 16,
        })),
    );
    pin(
        [0xF011_0105, 0x0F10_0000, 0, 0],
        Message::Stream(Stream::FunctionBlockInfo(FunctionBlockInfo {
            active: false,
            id: FunctionBlockId::new(1).unwrap(),
            direction: Direction::Input,
            ui_hint: UiHint::Unknown,
            midi1: Midi1Port::Unrestricted,
            first_group: FirstGroup::new(15).unwrap(),
            groups_spanned: GroupSpan::new(16).unwrap(),
            midi_ci_version: 0,
            max_sysex8_streams: 0,
        })),
    );
    // Figure 23: block number in 15-8, then thirteen name bytes.
    pin(
        [0xF012_0241, 0x4200_0000, 0, 0],
        Message::Stream(Stream::FunctionBlockName {
            block: FunctionBlockId::new(2).unwrap(),
            form: Form::Complete,
            bytes: *b"AB\0\0\0\0\0\0\0\0\0\0\0",
        }),
    );
}

// Cases the former `midi2` oracle found that are about the specification, not
// about that crate: each is pinned here with words from the figures and tables.
#[test]
fn what_the_oracle_found() {
    let f = |f| Message::Flex(g(6), f);
    // Table 14 defines chord type 0x1B (7 Suspended 4th), as a chord type and
    // as a bass chord type.
    pin(
        [0xD610_0006, 0x031B_0000, 0, 0x7 << 24 | 0x1B << 16],
        f(Flex::SetChordName(
            FlexAddress::Group,
            ChordName {
                tonic_accidental: Accidental::Natural,
                tonic: Some(Letter::C),
                chord_type: ChordType::new(0x1B).unwrap(),
                alterations: [none(), none(), none(), none()],
                bass: ChordBass {
                    pitch: Some(ChordPitch {
                        accidental: Accidental::Natural,
                        letter: Letter::G,
                    }),
                    chord_type: ChordType::new(0x1B).unwrap(),
                    alterations: [none(), none()],
                },
            },
        )),
    );
    // Text is checked as a whole after reassembly, not per packet: a Start
    // may end inside a character (the euro sign is E2 82 AC) and the Continue
    // begin with its last byte.
    let text = |form, bytes| {
        Message::Flex(
            g(6),
            Flex::Text {
                address: FlexAddress::Group,
                kind: FlexTextKind::Lyrics,
                form,
                bytes,
            },
        )
    };
    pin(
        [0xD650_0201, 0x6162_6364, 0x6566_6768, 0x696A_E282],
        text(Form::Start, *b"abcdefghij\xE2\x82"),
    );
    pin(
        [0xD690_0201, 0xAC00_0000, 0, 0],
        text(Form::Continue, *b"\xAC\0\0\0\0\0\0\0\0\0\0\0"),
    );
    // The same for the Stream text messages, whose bytes are kept as sent: a
    // Product Instance Id with a byte outside 32 to 126 is still a packet.
    pin(
        [0xFC04_C3A9, 0x4142_0000, 0, 0],
        Message::Stream(Stream::ProductInstanceId(
            Form::End,
            *b"\xC3\xA9AB\0\0\0\0\0\0\0\0\0\0",
        )),
    );
    // A lone End, Start or Continue is a packet of its own, whole messages are
    // for the layer above: the same for SysEx 7 and SysEx 8 (above) and the
    // Stream and Flex text forms (above).
}

// Appendix A, Table 22: every defined Registered Per-Note Controller number,
// as the literal low byte of the first word of a MIDI 2.0 message, group 1,
// channel 2, note 60: 0x4102_3C00 | number.
#[test]
fn registered_per_note_controllers_have_their_appendix_a_numbers() {
    use RegisteredPerNoteController as C;
    let table: [(u32, C); 22] = [
        (1, C::Modulation),
        (2, C::Breath),
        (3, C::Pitch),
        (7, C::Volume),
        (8, C::Balance),
        (10, C::Pan),
        (11, C::Expression),
        (70, C::SoundVariation),
        (71, C::Timbre),
        (72, C::ReleaseTime),
        (73, C::AttackTime),
        (74, C::Brightness),
        (75, C::DecayTime),
        (76, C::VibratoRate),
        (77, C::VibratoDepth),
        (78, C::VibratoDelay),
        (79, C::SoundController10),
        (91, C::ReverbSend),
        (92, C::Effects2Depth),
        (93, C::ChorusSend),
        (94, C::Effects4Depth),
        (95, C::Effects5Depth),
    ];
    for (number, controller) in table {
        pin(
            [0x4102_3C00 | number, 5, 0, 0],
            Message::Midi2(
                addr(1, 2),
                Voice2::RegisteredPerNote {
                    note: note(60),
                    controller,
                    value: 5,
                },
            ),
        );
    }
}
