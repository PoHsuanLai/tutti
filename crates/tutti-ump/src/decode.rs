//! `decode`: UMP words to a [`Message`].
//!
//! The bit positions of every message are in this file and in `encode.rs`,
//! written out per message against M2-104 Appendix F and §7. (`Ump::words` and
//! `Ump::is_note_off` read the message type, status and velocity bits they
//! need, and a test ties them to this decoder.)
//!
//! Reserved bits (the `r` bits and fields marked reserved) are ignored, so a
//! sender that sets them is read as if it had not (§2.1.3: a receiver does not
//! depend on reserved bits being zero). A reserved *value* in an
//! enumerated or bounded field (a direction of `0b00`, a protocol of `0x03`, a
//! count past its maximum, an attribute type of 4 and up) is kept raw in the
//! field, so the message stays readable and re-encodes exactly. [`Unknown`] is
//! for what cannot be framed: a reserved message type, an undefined status or
//! opcode, a count that does not fit its packet, and a MIDI 1.0 data byte with
//! its high bit set where the figure draws that bit as `0` (System messages,
//! SysEx 7 data), which is part of the byte being 7-bit, not a reserved bit.

use crate::field::{
    DeltaTicks, FirstGroup, FunctionBlockId, GroupSpan, NoteNumber, SharpsFlats, U4, U7, U14,
};
use crate::message::{
    Accidental, Address, Alteration, AlterationKind, Attribute, Bank, BlockFilter, BlockSelect,
    ChordBass, ChordName, ChordPitch, ChordType, Data128, DeviceIdentity, Direction,
    DiscoveryFilter, EndpointInfo, Flex, FlexAddress, FlexTextKind, Form, FunctionBlockInfo,
    KeySignature, Letter, Message, Metronome, Midi1Port, MixedDataHeader, Protocol, Stream,
    StreamConfig, Sysex7Packet, Sysex8Packet, SysexStatus, System, TimeSignature, UiHint,
    UmpVersion, Unknown, Utility, Voice1, Voice2,
};
use crate::{CCNumber, MidiChannel, MidiGroup, Ump};

/// Decodes one packet.
///
/// Never panics, whatever the words. The words past the message type's size
/// are not read.
///
/// # Errors
///
/// [`Unknown`], holding the words, for a reserved message type, an undefined
/// status, or a reserved value in a defined field.
pub fn decode(ump: Ump) -> Result<Message, Unknown> {
    let w = ump.0;
    let message = match w[0] >> 28 {
        0x0 => utility(w[0]).map(Message::Utility),
        0x1 => system(w[0]).map(|s| Message::System(group(w[0]), s)),
        0x2 => voice1(w[0]).map(|v| Message::Midi1(address(w[0]), v)),
        0x3 => sysex7(w).map(|p| Message::Sysex7(group(w[0]), p)),
        0x4 => voice2(w).map(|v| Message::Midi2(address(w[0]), v)),
        0x5 => data128(w).map(|d| Message::Data128(group(w[0]), d)),
        0xD => flex(w).map(|f| Message::Flex(group(w[0]), f)),
        0xF => stream(w).map(Message::Stream),
        _ => None,
    };
    message.ok_or(Unknown(ump))
}

// The decoder reads the header of a type it already matched, so the Group and
// Channel fields are there: masked, as `Ump::group` and `Ump::address` read them.
fn group(w0: u32) -> MidiGroup {
    MidiGroup::masked((w0 >> 24) as u8)
}

fn address(w0: u32) -> Address {
    Address {
        group: group(w0),
        channel: MidiChannel::masked((w0 >> 16) as u8),
    }
}

/// Byte `n` of a word, 0 the least significant.
fn byte(w: u32, n: u32) -> u8 {
    (w >> (8 * n)) as u8
}

/// A 7-bit field whose top bit is a reserved `r` bit: ignored.
fn u7(w: u32, n: u32) -> U7 {
    U7::masked(byte(w, n))
}

/// A data byte drawn as `0ddddddd`: refused when the top bit is set.
fn data7(w: u32, n: u32) -> Option<U7> {
    U7::new(byte(w, n))
}

fn note(w: u32, n: u32) -> NoteNumber {
    NoteNumber::masked(byte(w, n))
}

fn flag(w: u32, bit: u32) -> bool {
    (w >> bit) & 1 == 1
}

/// The Format field at bit `lsb`: 26 in a Stream message, 22 in Flex Data.
fn form(w0: u32, lsb: u32) -> Form {
    match (w0 >> lsb) & 0x3 {
        0 => Form::Complete,
        1 => Form::Start,
        2 => Form::Continue,
        _ => Form::End,
    }
}

// ---- Utility ---------------------------------------------------------------

fn utility(w0: u32) -> Option<Utility> {
    let low16 = w0 as u16;
    match (w0 >> 20) & 0xf {
        0x0 => Some(Utility::Noop),
        0x1 => Some(Utility::JrClock(low16)),
        0x2 => Some(Utility::JrTimestamp(low16)),
        0x3 => Some(Utility::TicksPerQuarterNote(low16)),
        0x4 => Some(Utility::DeltaClockstamp(DeltaTicks::masked(w0 & 0xf_ffff))),
        _ => None,
    }
}

// ---- System ----------------------------------------------------------------

fn system(w0: u32) -> Option<System> {
    match byte(w0, 2) {
        0xF1 => Some(System::TimeCode(data7(w0, 1)?)),
        0xF2 => Some(System::SongPosition(U14::masked(
            u16::from(data7(w0, 1)?.get()) | (u16::from(data7(w0, 0)?.get()) << 7),
        ))),
        0xF3 => Some(System::SongSelect(data7(w0, 1)?)),
        0xF6 => Some(System::TuneRequest),
        0xF8 => Some(System::TimingClock),
        0xFA => Some(System::Start),
        0xFB => Some(System::Continue),
        0xFC => Some(System::Stop),
        0xFE => Some(System::ActiveSensing),
        0xFF => Some(System::Reset),
        _ => None,
    }
}

// ---- MIDI 1.0 channel voice ------------------------------------------------

fn voice1(w0: u32) -> Option<Voice1> {
    let d1 = u7(w0, 1);
    let d2 = u7(w0, 0);
    match (w0 >> 20) & 0xf {
        0x8 => Some(Voice1::NoteOff {
            note: note(w0, 1),
            velocity: d2,
        }),
        0x9 => Some(Voice1::NoteOn {
            note: note(w0, 1),
            velocity: d2,
        }),
        0xA => Some(Voice1::PolyPressure {
            note: note(w0, 1),
            pressure: d2,
        }),
        0xB => Some(Voice1::ControlChange {
            controller: CCNumber::masked(d1.get()),
            value: d2,
        }),
        0xC => Some(Voice1::ProgramChange(d1)),
        0xD => Some(Voice1::ChannelPressure(d1)),
        0xE => Some(Voice1::PitchBend(U14::masked(
            u16::from(d1.get()) | (u16::from(d2.get()) << 7),
        ))),
        _ => None,
    }
}

// ---- System Exclusive 7 ----------------------------------------------------

fn sysex_status(nibble: u32) -> Option<SysexStatus> {
    match nibble {
        0x0 => Some(SysexStatus::Complete),
        0x1 => Some(SysexStatus::Start),
        0x2 => Some(SysexStatus::Continue),
        0x3 => Some(SysexStatus::End),
        _ => None,
    }
}

fn sysex7(w: [u32; 4]) -> Option<Sysex7Packet> {
    let status = sysex_status((w[0] >> 20) & 0xf)?;
    let len = ((w[0] >> 16) & 0xf) as u8;
    if len > 6 {
        return None;
    }
    let bytes = [
        byte(w[0], 1),
        byte(w[0], 0),
        byte(w[1], 3),
        byte(w[1], 2),
        byte(w[1], 1),
        byte(w[1], 0),
    ];
    // Bytes past `# of bytes` are reserved; those within it are data.
    Sysex7Packet::new(status, &bytes[..usize::from(len)])
}

// ---- Data 128 --------------------------------------------------------------

fn data128(w: [u32; 4]) -> Option<Data128> {
    let low = ((w[0] >> 16) & 0xf) as u8;
    match (w[0] >> 20) & 0xf {
        status @ 0x0..=0x3 => {
            let stream_id = byte(w[0], 1);
            // §7.8.1 says an End with `# of bytes` 0xF aborts and has all data
            // bytes zero, and is silent on the stream id. Keeping the id (byte 3)
            // is an inference: a sender that zeroes it aborts stream 0.
            if status == 0x3 && low == 0xF {
                return Some(Data128::Sysex8Abort { stream_id });
            }
            // `# of bytes` counts the stream id: 1 to 14.
            if !(1..=14).contains(&low) {
                return None;
            }
            let mut bytes = [0u8; 13];
            bytes[0] = byte(w[0], 0);
            for (i, word) in w[1..].iter().enumerate() {
                bytes[1 + 4 * i..5 + 4 * i].copy_from_slice(&word.to_be_bytes());
            }
            let p = Sysex8Packet::new(
                sysex_status(status)?,
                stream_id,
                &bytes[..usize::from(low) - 1],
            )?;
            Some(Data128::Sysex8(p))
        }
        0x8 => Some(Data128::MixedDataHeader(MixedDataHeader {
            mds_id: U4::masked(low),
            chunk_bytes: w[0] as u16,
            chunks: (w[1] >> 16) as u16,
            chunk: w[1] as u16,
            manufacturer: (w[2] >> 16) as u16,
            device: w[2] as u16,
            sub_id_1: (w[3] >> 16) as u16,
            sub_id_2: w[3] as u16,
        })),
        0x9 => {
            let mut bytes = [0u8; 14];
            bytes[..2].copy_from_slice(&(w[0] as u16).to_be_bytes());
            for (i, word) in w[1..].iter().enumerate() {
                bytes[2 + 4 * i..6 + 4 * i].copy_from_slice(&word.to_be_bytes());
            }
            Some(Data128::MixedDataPayload {
                mds_id: U4::masked(low),
                bytes,
            })
        }
        _ => None,
    }
}

// ---- MIDI 2.0 channel voice ------------------------------------------------

fn attribute(kind: u8, data: u16) -> Attribute {
    match kind {
        // Type 0 carries no data, and the receiver ignores the field (§7.4.14).
        0x00 => Attribute::None,
        0x01 => Attribute::Manufacturer(data),
        0x02 => Attribute::Profile(data),
        0x03 => Attribute::Pitch(data),
        _ => Attribute::Reserved { kind, data },
    }
}

fn voice2(w: [u32; 4]) -> Option<Voice2> {
    let (w0, data) = (w[0], w[1]);
    let index = byte(w0, 0);
    match (w0 >> 20) & 0xf {
        0x0 => Some(Voice2::RegisteredPerNote {
            note: note(w0, 1),
            controller: index.into(),
            value: data,
        }),
        0x1 => Some(Voice2::AssignablePerNote {
            note: note(w0, 1),
            controller: index,
            value: data,
        }),
        0x2 => Some(Voice2::Registered {
            bank: u7(w0, 1),
            index: u7(w0, 0),
            value: data,
        }),
        0x3 => Some(Voice2::Assignable {
            bank: u7(w0, 1),
            index: u7(w0, 0),
            value: data,
        }),
        0x4 => Some(Voice2::RelativeRegistered {
            bank: u7(w0, 1),
            index: u7(w0, 0),
            delta: data as i32,
        }),
        0x5 => Some(Voice2::RelativeAssignable {
            bank: u7(w0, 1),
            index: u7(w0, 0),
            delta: data as i32,
        }),
        0x6 => Some(Voice2::PerNotePitchBend {
            note: note(w0, 1),
            value: data,
        }),
        0x8 => Some(Voice2::NoteOff {
            note: note(w0, 1),
            velocity: (data >> 16) as u16,
            attribute: attribute(index, data as u16),
        }),
        0x9 => Some(Voice2::NoteOn {
            note: note(w0, 1),
            velocity: (data >> 16) as u16,
            attribute: attribute(index, data as u16),
        }),
        0xA => Some(Voice2::PolyPressure {
            note: note(w0, 1),
            value: data,
        }),
        0xB => Some(Voice2::ControlChange {
            controller: CCNumber::masked(byte(w0, 1)),
            value: data,
        }),
        0xC => Some(Voice2::ProgramChange {
            program: u7(data, 3),
            bank: flag(w0, 0).then(|| Bank {
                msb: u7(data, 1),
                lsb: u7(data, 0),
            }),
        }),
        0xD => Some(Voice2::ChannelPressure(data)),
        0xE => Some(Voice2::PitchBend(data)),
        0xF => Some(Voice2::PerNoteManagement {
            note: note(w0, 1),
            detach: flag(w0, 1),
            reset: flag(w0, 0),
        }),
        _ => None,
    }
}

// ---- Flex Data -------------------------------------------------------------

/// A note letter field: `None` for 0, the letter for 1 to 7, and the raw value
/// for 8 to 15, which are reserved.
fn letter(nibble: u32) -> Option<Letter> {
    match nibble & 0xf {
        0 => None,
        1 => Some(Letter::A),
        2 => Some(Letter::B),
        3 => Some(Letter::C),
        4 => Some(Letter::D),
        5 => Some(Letter::E),
        6 => Some(Letter::F),
        7 => Some(Letter::G),
        n => Some(Letter::Reserved(n as u8)),
    }
}

fn accidental(nibble: u32) -> Accidental {
    match nibble & 0xf {
        0x0 => Accidental::Natural,
        0x1 => Accidental::Sharp,
        0x2 => Accidental::DoubleSharp,
        0xF => Accidental::Flat,
        0xE => Accidental::DoubleFlat,
        n => Accidental::Reserved(n as u8),
    }
}

fn alteration(nibbles: u32) -> Alteration {
    let kind = match (nibbles >> 4) & 0xf {
        0 => AlterationKind::None,
        1 => AlterationKind::Add,
        2 => AlterationKind::Subtract,
        3 => AlterationKind::Raise,
        4 => AlterationKind::Lower,
        n => AlterationKind::Reserved(n as u8),
    };
    Alteration {
        kind,
        degree: U4::masked((nibbles & 0xf) as u8),
    }
}

fn flex_text_kind(bank: u8, status: u8) -> Option<FlexTextKind> {
    use FlexTextKind as K;
    match (bank, status) {
        (1, 0x00) => Some(K::UnknownMetadata),
        (1, 0x01) => Some(K::ProjectName),
        (1, 0x02) => Some(K::CompositionName),
        (1, 0x03) => Some(K::ClipName),
        (1, 0x04) => Some(K::Copyright),
        (1, 0x05) => Some(K::Composer),
        (1, 0x06) => Some(K::Lyricist),
        (1, 0x07) => Some(K::Arranger),
        (1, 0x08) => Some(K::Publisher),
        (1, 0x09) => Some(K::PrimaryPerformer),
        (1, 0x0A) => Some(K::AccompanyingPerformer),
        (1, 0x0B) => Some(K::RecordingDate),
        (1, 0x0C) => Some(K::RecordingLocation),
        (2, 0x00) => Some(K::UnknownPerformance),
        (2, 0x01) => Some(K::Lyrics),
        (2, 0x02) => Some(K::LyricsLanguage),
        (2, 0x03) => Some(K::Ruby),
        (2, 0x04) => Some(K::RubyLanguage),
        _ => None,
    }
}

fn flex(w: [u32; 4]) -> Option<Flex> {
    let w0 = w[0];
    let (bank, status) = (byte(w0, 1), byte(w0, 0));
    let address = match (w0 >> 20) & 0x3 {
        0 => FlexAddress::Channel(MidiChannel::masked(((w0 >> 16) & 0xf) as u8)),
        1 => FlexAddress::Group,
        n => FlexAddress::Reserved(n as u8),
    };
    // The setup messages are Complete, and the first three Group messages.
    let complete = form(w0, 22) == Form::Complete;
    match (bank, status) {
        (0x00, 0x00) if complete && address == FlexAddress::Group => Some(Flex::SetTempo(w[1])),
        (0x00, 0x01) if complete && address == FlexAddress::Group => {
            Some(Flex::SetTimeSignature(TimeSignature {
                numerator: byte(w[1], 3),
                denominator: byte(w[1], 2),
                thirty_seconds: byte(w[1], 1),
            }))
        }
        (0x00, 0x02) if complete && address == FlexAddress::Group => {
            Some(Flex::SetMetronome(Metronome {
                clocks_per_click: byte(w[1], 3),
                accent_1: byte(w[1], 2),
                accent_2: byte(w[1], 1),
                accent_3: byte(w[1], 0),
                subdivision_1: byte(w[2], 3),
                subdivision_2: byte(w[2], 2),
            }))
        }
        (0x00, 0x05) if complete => {
            // A signed nibble: `-8` is "unknown or non-standard".
            let sharps_flats = ((w[1] >> 28) as i8) << 4 >> 4;
            Some(Flex::SetKeySignature(
                address,
                KeySignature {
                    sharps_flats: SharpsFlats::new(sharps_flats),
                    tonic: letter(w[1] >> 24),
                },
            ))
        }
        (0x00, 0x06) if complete => Some(Flex::SetChordName(address, chord_name(w))),
        (0x01 | 0x02, _) => {
            let kind = flex_text_kind(bank, status)?;
            let mut bytes = [0u8; 12];
            for (i, word) in w[1..].iter().enumerate() {
                bytes[4 * i..4 * i + 4].copy_from_slice(&word.to_be_bytes());
            }
            Some(Flex::Text {
                address,
                kind,
                form: form(w0, 22),
                bytes,
            })
        }
        _ => None,
    }
}

fn chord_name(w: [u32; 4]) -> ChordName {
    // Bass note `0` is the chord's own tonic (Table 15 asks for sharps/flats
    // `-8` with it, and the accidental is not read).
    let bass_pitch = letter(w[3] >> 24).map(|letter| ChordPitch {
        accidental: accidental(w[3] >> 28),
        letter,
    });
    ChordName {
        tonic_accidental: accidental(w[1] >> 28),
        tonic: letter(w[1] >> 24),
        chord_type: ChordType::wire(byte(w[1], 2)),
        alterations: [
            alteration(w[1] >> 8),
            alteration(w[1]),
            alteration(w[2] >> 24),
            alteration(w[2] >> 16),
        ],
        bass: ChordBass {
            pitch: bass_pitch,
            chord_type: ChordType::wire(byte(w[3], 2)),
            alterations: [alteration(w[3] >> 8), alteration(w[3])],
        },
    }
}

// ---- UMP Stream ------------------------------------------------------------

/// The 14 text bytes of an Endpoint Name or Product Instance Id packet: the
/// low two bytes of word 0, then words 1 to 3.
fn text14(w: [u32; 4]) -> [u8; 14] {
    let mut bytes = [0u8; 14];
    bytes[..2].copy_from_slice(&(w[0] as u16).to_be_bytes());
    for (i, word) in w[1..].iter().enumerate() {
        bytes[2 + 4 * i..6 + 4 * i].copy_from_slice(&word.to_be_bytes());
    }
    bytes
}

fn stream(w: [u32; 4]) -> Option<Stream> {
    let w0 = w[0];
    let complete = form(w0, 26) == Form::Complete;
    let version = UmpVersion {
        major: byte(w0, 1),
        minor: byte(w0, 0),
    };
    match (w0 >> 16) & 0x3ff {
        0x00 if complete => Some(Stream::EndpointDiscovery {
            version,
            filter: DiscoveryFilter {
                endpoint_info: flag(w[1], 0),
                device_identity: flag(w[1], 1),
                endpoint_name: flag(w[1], 2),
                product_instance_id: flag(w[1], 3),
                stream_configuration: flag(w[1], 4),
            },
        }),
        0x01 if complete => Some(Stream::EndpointInfo(EndpointInfo {
            version,
            static_function_blocks: flag(w[1], 31),
            function_blocks: u7(w[1], 3),
            midi2: flag(w[1], 9),
            midi1: flag(w[1], 8),
            rx_jr: flag(w[1], 1),
            tx_jr: flag(w[1], 0),
        })),
        0x02 if complete => Some(Stream::DeviceIdentity(DeviceIdentity {
            manufacturer: [u7(w[1], 2), u7(w[1], 1), u7(w[1], 0)],
            family: [u7(w[2], 3), u7(w[2], 2)],
            model: [u7(w[2], 1), u7(w[2], 0)],
            revision: [u7(w[3], 3), u7(w[3], 2), u7(w[3], 1), u7(w[3], 0)],
        })),
        0x03 => Some(Stream::EndpointName(form(w0, 26), text14(w))),
        0x04 => Some(Stream::ProductInstanceId(form(w0, 26), text14(w))),
        0x05 if complete => Some(Stream::StreamConfigurationRequest(stream_config(w0))),
        0x06 if complete => Some(Stream::StreamConfigurationNotification(stream_config(w0))),
        0x10 if complete => Some(Stream::FunctionBlockDiscovery {
            block: match byte(w0, 1) {
                0xFF => BlockSelect::All,
                n => BlockSelect::One(FunctionBlockId::wire(n)),
            },
            filter: BlockFilter {
                info: flag(w0, 0),
                name: flag(w0, 1),
            },
        }),
        0x11 if complete => Some(Stream::FunctionBlockInfo(FunctionBlockInfo {
            active: flag(w0, 15),
            id: FunctionBlockId::wire(byte(w0, 1) & 0x7f),
            direction: match w0 & 0x3 {
                0b01 => Direction::Input,
                0b10 => Direction::Output,
                0b11 => Direction::Bidirectional,
                n => Direction::Reserved(n as u8),
            },
            ui_hint: match (w0 >> 4) & 0x3 {
                0 => UiHint::Unknown,
                1 => UiHint::Receiver,
                2 => UiHint::Sender,
                _ => UiHint::Both,
            },
            midi1: match (w0 >> 2) & 0x3 {
                0 => Midi1Port::No,
                1 => Midi1Port::Unrestricted,
                2 => Midi1Port::Restricted,
                n => Midi1Port::Reserved(n as u8),
            },
            first_group: FirstGroup::wire(byte(w[1], 3)),
            groups_spanned: GroupSpan::wire(byte(w[1], 2)),
            midi_ci_version: byte(w[1], 1),
            max_sysex8_streams: byte(w[1], 0),
        })),
        0x12 => Some(Stream::FunctionBlockName {
            block: FunctionBlockId::wire(byte(w0, 1)),
            form: form(w0, 26),
            bytes: {
                let mut b = [0u8; 13];
                b[0] = byte(w0, 0);
                for (i, word) in w[1..].iter().enumerate() {
                    b[1 + 4 * i..5 + 4 * i].copy_from_slice(&word.to_be_bytes());
                }
                b
            },
        }),
        0x20 if complete => Some(Stream::StartOfClip),
        0x21 if complete => Some(Stream::EndOfClip),
        _ => None,
    }
}

fn stream_config(w0: u32) -> StreamConfig {
    StreamConfig {
        protocol: match byte(w0, 1) {
            0x01 => Protocol::Midi1,
            0x02 => Protocol::Midi2,
            n => Protocol::Reserved(n),
        },
        rx_jr: flag(w0, 1),
        tx_jr: flag(w0, 0),
    }
}
