//! `From<Message> for Ump`: a message to its canonical words, reserved bits
//! zero. The mirror of `decode.rs`.

use crate::field::U7;
use crate::message::{
    Accidental, Address, Alteration, AlterationKind, Attribute, BlockSelect, ChordName, ChordPitch,
    Data128, Direction, Flex, FlexAddress, FlexTextKind, Form, KeySignature, Letter, Message,
    Midi1Port, Protocol, Stream, StreamConfig, Sysex7Packet, SysexStatus, System, UiHint, Utility,
    Voice1, Voice2,
};
use crate::{MidiGroup, Ump};

impl From<Message> for Ump {
    fn from(message: Message) -> Ump {
        let w = match message {
            Message::Utility(u) => [utility(u), 0, 0, 0],
            Message::System(g, s) => [system(g, s), 0, 0, 0],
            Message::Midi1(a, v) => [voice1(a, v), 0, 0, 0],
            Message::Sysex7(g, p) => sysex7(g, p),
            Message::Midi2(a, v) => voice2(a, v),
            Message::Data128(g, d) => data128(g, d),
            Message::Flex(g, f) => flex(g, f),
            Message::Stream(s) => stream(s),
        };
        Ump(w)
    }
}

fn head(mt: u32, group: MidiGroup) -> u32 {
    (mt << 28) | (u32::from(group.get()) << 24)
}

fn address(mt: u32, a: Address, opcode: u32) -> u32 {
    head(mt, a.group) | (opcode << 20) | (u32::from(a.channel.get()) << 16)
}

fn u7(v: U7) -> u32 {
    u32::from(v.get())
}

fn form(f: Form) -> u32 {
    match f {
        Form::Complete => 0,
        Form::Start => 1,
        Form::Continue => 2,
        Form::End => 3,
    }
}

/// Four bytes, most significant first, as a word.
fn word(b: [u8; 4]) -> u32 {
    u32::from_be_bytes(b)
}

// ---- Utility ---------------------------------------------------------------

fn utility(u: Utility) -> u32 {
    match u {
        Utility::Noop => 0,
        Utility::JrClock(t) => (0x1 << 20) | u32::from(t),
        Utility::JrTimestamp(t) => (0x2 << 20) | u32::from(t),
        Utility::TicksPerQuarterNote(n) => (0x3 << 20) | u32::from(n.get()),
        Utility::DeltaClockstamp(t) => (0x4 << 20) | t.get(),
    }
}

// ---- System ----------------------------------------------------------------

fn system(g: MidiGroup, s: System) -> u32 {
    let h = head(0x1, g);
    match s {
        System::TimeCode(d) => h | (0xF1 << 16) | (u7(d) << 8),
        System::SongPosition(p) => {
            h | (0xF2 << 16) | ((u32::from(p.get()) & 0x7f) << 8) | (u32::from(p.get()) >> 7)
        }
        System::SongSelect(d) => h | (0xF3 << 16) | (u7(d) << 8),
        System::TuneRequest => h | (0xF6 << 16),
        System::TimingClock => h | (0xF8 << 16),
        System::Start => h | (0xFA << 16),
        System::Continue => h | (0xFB << 16),
        System::Stop => h | (0xFC << 16),
        System::ActiveSensing => h | (0xFE << 16),
        System::Reset => h | (0xFF << 16),
    }
}

// ---- MIDI 1.0 channel voice ------------------------------------------------

fn voice1(a: Address, v: Voice1) -> u32 {
    let (opcode, d1, d2) = match v {
        Voice1::NoteOff { note, velocity } => (0x8, u32::from(note.get()), u7(velocity)),
        Voice1::NoteOn { note, velocity } => (0x9, u32::from(note.get()), u7(velocity)),
        Voice1::PolyPressure { note, pressure } => (0xA, u32::from(note.get()), u7(pressure)),
        Voice1::ControlChange { controller, value } => {
            (0xB, u32::from(controller.get()), u7(value))
        }
        Voice1::ProgramChange(p) => (0xC, u7(p), 0),
        Voice1::ChannelPressure(p) => (0xD, u7(p), 0),
        Voice1::PitchBend(b) => (0xE, u32::from(b.get()) & 0x7f, u32::from(b.get()) >> 7),
    };
    address(0x2, a, opcode) | (d1 << 8) | d2
}

// ---- System Exclusive ------------------------------------------------------

fn sysex_status(s: SysexStatus) -> u32 {
    match s {
        SysexStatus::Complete => 0,
        SysexStatus::Start => 1,
        SysexStatus::Continue => 2,
        SysexStatus::End => 3,
    }
}

fn sysex7(g: MidiGroup, p: Sysex7Packet) -> [u32; 4] {
    let mut b = [0u8; 6];
    for (slot, v) in b.iter_mut().zip(p.bytes()) {
        *slot = v.get();
    }
    let n = p.bytes().len() as u32;
    [
        head(0x3, g)
            | (sysex_status(p.status()) << 20)
            | (n << 16)
            | (u32::from(b[0]) << 8)
            | u32::from(b[1]),
        word([b[2], b[3], b[4], b[5]]),
        0,
        0,
    ]
}

fn data128(g: MidiGroup, d: Data128) -> [u32; 4] {
    let h = head(0x5, g);
    match d {
        Data128::Sysex8(p) => {
            let mut b = [0u8; 13];
            b[..p.bytes().len()].copy_from_slice(p.bytes());
            // `# of bytes` counts the stream id.
            let n = p.bytes().len() as u32 + 1;
            [
                h | (sysex_status(p.status()) << 20)
                    | (n << 16)
                    | (u32::from(p.stream_id()) << 8)
                    | u32::from(b[0]),
                word([b[1], b[2], b[3], b[4]]),
                word([b[5], b[6], b[7], b[8]]),
                word([b[9], b[10], b[11], b[12]]),
            ]
        }
        Data128::Sysex8Abort { stream_id } => [
            h | (0x3 << 20) | (0xF << 16) | (u32::from(stream_id) << 8),
            0,
            0,
            0,
        ],
        Data128::MixedDataHeader(m) => [
            h | (0x8 << 20) | (u32::from(m.mds_id.get()) << 16) | u32::from(m.chunk_bytes),
            (u32::from(m.chunks) << 16) | u32::from(m.chunk),
            (u32::from(m.manufacturer) << 16) | u32::from(m.device),
            (u32::from(m.sub_id_1) << 16) | u32::from(m.sub_id_2),
        ],
        Data128::MixedDataPayload { mds_id, bytes: b } => [
            h | (0x9 << 20)
                | (u32::from(mds_id.get()) << 16)
                | (u32::from(b[0]) << 8)
                | u32::from(b[1]),
            word([b[2], b[3], b[4], b[5]]),
            word([b[6], b[7], b[8], b[9]]),
            word([b[10], b[11], b[12], b[13]]),
        ],
    }
}

// ---- MIDI 2.0 channel voice ------------------------------------------------

fn attribute(a: Attribute) -> (u32, u32) {
    match a {
        Attribute::None => (0x00, 0),
        Attribute::Manufacturer(d) => (0x01, u32::from(d)),
        Attribute::Profile(d) => (0x02, u32::from(d)),
        Attribute::Pitch(d) => (0x03, u32::from(d)),
    }
}

fn voice2(a: Address, v: Voice2) -> [u32; 4] {
    // (opcode, byte 3, byte 4, data word)
    let (opcode, b3, b4, data) = match v {
        Voice2::RegisteredPerNote {
            note,
            controller,
            value,
        } => (0x0, u32::from(note.get()), u32::from(controller), value),
        Voice2::AssignablePerNote {
            note,
            controller,
            value,
        } => (0x1, u32::from(note.get()), u32::from(controller), value),
        Voice2::Registered { bank, index, value } => (0x2, u7(bank), u7(index), value),
        Voice2::Assignable { bank, index, value } => (0x3, u7(bank), u7(index), value),
        Voice2::RelativeRegistered { bank, index, delta } => {
            (0x4, u7(bank), u7(index), delta as u32)
        }
        Voice2::RelativeAssignable { bank, index, delta } => {
            (0x5, u7(bank), u7(index), delta as u32)
        }
        Voice2::PerNotePitchBend { note, value } => (0x6, u32::from(note.get()), 0, value),
        Voice2::NoteOff {
            note,
            velocity,
            attribute: at,
        } => {
            let (kind, d) = attribute(at);
            (
                0x8,
                u32::from(note.get()),
                kind,
                (u32::from(velocity) << 16) | d,
            )
        }
        Voice2::NoteOn {
            note,
            velocity,
            attribute: at,
        } => {
            let (kind, d) = attribute(at);
            (
                0x9,
                u32::from(note.get()),
                kind,
                (u32::from(velocity) << 16) | d,
            )
        }
        Voice2::PolyPressure { note, value } => (0xA, u32::from(note.get()), 0, value),
        Voice2::ControlChange { controller, value } => (0xB, u32::from(controller.get()), 0, value),
        Voice2::ProgramChange { program, bank } => {
            let (valid, msb, lsb) = match bank {
                Some(b) => (1, u7(b.msb), u7(b.lsb)),
                None => (0, 0, 0),
            };
            (0xC, 0, valid, (u7(program) << 24) | (msb << 8) | lsb)
        }
        Voice2::ChannelPressure(value) => (0xD, 0, 0, value),
        Voice2::PitchBend(value) => (0xE, 0, 0, value),
        Voice2::PerNoteManagement {
            note,
            detach,
            reset,
        } => (
            0xF,
            u32::from(note.get()),
            (u32::from(detach) << 1) | u32::from(reset),
            0,
        ),
    };
    [address(0x4, a, opcode) | (b3 << 8) | b4, data, 0, 0]
}

// ---- Flex Data -------------------------------------------------------------

fn flex_head(g: MidiGroup, f: Form, address: FlexAddress, bank: u32, status: u32) -> u32 {
    let (addr, channel) = match address {
        FlexAddress::Channel(c) => (0, u32::from(c.get())),
        FlexAddress::Group => (1, 0),
    };
    head(0xD, g) | (form(f) << 22) | (addr << 20) | (channel << 16) | (bank << 8) | status
}

fn letter(l: Option<Letter>) -> u32 {
    l.map_or(0, |l| l as u32)
}

fn accidental(a: Accidental) -> u32 {
    match a {
        Accidental::Natural => 0x0,
        Accidental::Sharp => 0x1,
        Accidental::DoubleSharp => 0x2,
        Accidental::Flat => 0xF,
        Accidental::DoubleFlat => 0xE,
    }
}

fn alteration(a: Alteration) -> u32 {
    let kind = match a.kind {
        AlterationKind::None => 0,
        AlterationKind::Add => 1,
        AlterationKind::Subtract => 2,
        AlterationKind::Raise => 3,
        AlterationKind::Lower => 4,
    };
    (kind << 4) | u32::from(a.degree.get())
}

fn key_signature(k: KeySignature) -> u32 {
    let sharps_flats = k.sharps_flats.map_or(0x8, |s| (s.get() as u32) & 0xf);
    (sharps_flats << 28) | (letter(k.tonic) << 24)
}

fn chord_name(c: ChordName) -> [u32; 4] {
    let (bass_acc, bass_note) = match c.bass.pitch {
        None => (0x8, 0),
        Some(ChordPitch {
            accidental: a,
            letter: l,
        }) => (accidental(a), l as u32),
    };
    [
        0,
        (accidental(c.tonic_accidental) << 28)
            | (letter(c.tonic) << 24)
            | (u32::from(c.chord_type.get()) << 16)
            | (alteration(c.alterations[0]) << 8)
            | alteration(c.alterations[1]),
        (alteration(c.alterations[2]) << 24) | (alteration(c.alterations[3]) << 16),
        (bass_acc << 28)
            | (bass_note << 24)
            | (u32::from(c.bass.chord_type.get()) << 16)
            | (alteration(c.bass.alterations[0]) << 8)
            | alteration(c.bass.alterations[1]),
    ]
}

fn flex_text_kind(k: FlexTextKind) -> (u32, u32) {
    use FlexTextKind as K;
    match k {
        K::UnknownMetadata => (1, 0x00),
        K::ProjectName => (1, 0x01),
        K::CompositionName => (1, 0x02),
        K::ClipName => (1, 0x03),
        K::Copyright => (1, 0x04),
        K::Composer => (1, 0x05),
        K::Lyricist => (1, 0x06),
        K::Arranger => (1, 0x07),
        K::Publisher => (1, 0x08),
        K::PrimaryPerformer => (1, 0x09),
        K::AccompanyingPerformer => (1, 0x0A),
        K::RecordingDate => (1, 0x0B),
        K::RecordingLocation => (1, 0x0C),
        K::UnknownPerformance => (2, 0x00),
        K::Lyrics => (2, 0x01),
        K::LyricsLanguage => (2, 0x02),
        K::Ruby => (2, 0x03),
        K::RubyLanguage => (2, 0x04),
    }
}

fn flex(g: MidiGroup, f: Flex) -> [u32; 4] {
    let complete = |address, bank, status| flex_head(g, Form::Complete, address, bank, status);
    match f {
        Flex::SetTempo(t) => [complete(FlexAddress::Group, 0, 0x00), t, 0, 0],
        Flex::SetTimeSignature(t) => [
            complete(FlexAddress::Group, 0, 0x01),
            word([t.numerator, t.denominator, t.thirty_seconds, 0]),
            0,
            0,
        ],
        Flex::SetMetronome(m) => [
            complete(FlexAddress::Group, 0, 0x02),
            word([m.clocks_per_click, m.accent_1, m.accent_2, m.accent_3]),
            word([m.subdivision_1, m.subdivision_2, 0, 0]),
            0,
        ],
        Flex::SetKeySignature(a, k) => [complete(a, 0, 0x05), key_signature(k), 0, 0],
        Flex::SetChordName(a, c) => {
            let mut w = chord_name(c);
            w[0] = complete(a, 0, 0x06);
            w
        }
        Flex::Text {
            address,
            kind,
            form,
            bytes: b,
        } => {
            let (bank, status) = flex_text_kind(kind);
            [
                flex_head(g, form, address, bank, status),
                word([b[0], b[1], b[2], b[3]]),
                word([b[4], b[5], b[6], b[7]]),
                word([b[8], b[9], b[10], b[11]]),
            ]
        }
    }
}

// ---- UMP Stream ------------------------------------------------------------

fn stream_head(f: Form, status: u32) -> u32 {
    (0xF << 28) | (form(f) << 26) | (status << 16)
}

fn stream_config(c: StreamConfig) -> u32 {
    let protocol = match c.protocol {
        Protocol::Midi1 => 0x01,
        Protocol::Midi2 => 0x02,
    };
    (protocol << 8) | (u32::from(c.rx_jr) << 1) | u32::from(c.tx_jr)
}

fn text14(h: u32, b: [u8; 14]) -> [u32; 4] {
    [
        h | (u32::from(b[0]) << 8) | u32::from(b[1]),
        word([b[2], b[3], b[4], b[5]]),
        word([b[6], b[7], b[8], b[9]]),
        word([b[10], b[11], b[12], b[13]]),
    ]
}

fn stream(s: Stream) -> [u32; 4] {
    let done = Form::Complete;
    match s {
        Stream::EndpointDiscovery { version, filter } => [
            stream_head(done, 0x00) | (u32::from(version.major) << 8) | u32::from(version.minor),
            u32::from(filter.endpoint_info)
                | (u32::from(filter.device_identity) << 1)
                | (u32::from(filter.endpoint_name) << 2)
                | (u32::from(filter.product_instance_id) << 3)
                | (u32::from(filter.stream_configuration) << 4),
            0,
            0,
        ],
        Stream::EndpointInfo(e) => [
            stream_head(done, 0x01)
                | (u32::from(e.version.major) << 8)
                | u32::from(e.version.minor),
            (u32::from(e.static_function_blocks) << 31)
                | (u32::from(e.function_blocks.get()) << 24)
                | (u32::from(e.midi2) << 9)
                | (u32::from(e.midi1) << 8)
                | (u32::from(e.rx_jr) << 1)
                | u32::from(e.tx_jr),
            0,
            0,
        ],
        Stream::DeviceIdentity(d) => [
            stream_head(done, 0x02),
            (u7(d.manufacturer[0]) << 16) | (u7(d.manufacturer[1]) << 8) | u7(d.manufacturer[2]),
            (u7(d.family[0]) << 24)
                | (u7(d.family[1]) << 16)
                | (u7(d.model[0]) << 8)
                | u7(d.model[1]),
            (u7(d.revision[0]) << 24)
                | (u7(d.revision[1]) << 16)
                | (u7(d.revision[2]) << 8)
                | u7(d.revision[3]),
        ],
        Stream::EndpointName(f, b) => text14(stream_head(f, 0x03), b),
        Stream::ProductInstanceId(f, b) => text14(stream_head(f, 0x04), b),
        Stream::StreamConfigurationRequest(c) => {
            [stream_head(done, 0x05) | stream_config(c), 0, 0, 0]
        }
        Stream::StreamConfigurationNotification(c) => {
            [stream_head(done, 0x06) | stream_config(c), 0, 0, 0]
        }
        Stream::FunctionBlockDiscovery { block, filter } => {
            let block = match block {
                BlockSelect::One(id) => u32::from(id.get()),
                BlockSelect::All => 0xFF,
            };
            [
                stream_head(done, 0x10)
                    | (block << 8)
                    | (u32::from(filter.name) << 1)
                    | u32::from(filter.info),
                0,
                0,
                0,
            ]
        }
        Stream::FunctionBlockInfo(i) => {
            let direction = match i.direction {
                Direction::Input => 0b01,
                Direction::Output => 0b10,
                Direction::Bidirectional => 0b11,
            };
            let ui_hint = match i.ui_hint {
                UiHint::Unknown => 0,
                UiHint::Receiver => 1,
                UiHint::Sender => 2,
                UiHint::Both => 3,
            };
            let midi1 = match i.midi1 {
                Midi1Port::No => 0,
                Midi1Port::Unrestricted => 1,
                Midi1Port::Restricted => 2,
            };
            [
                stream_head(done, 0x11)
                    | (u32::from(i.active) << 15)
                    | (u32::from(i.id.get()) << 8)
                    | (ui_hint << 4)
                    | (midi1 << 2)
                    | direction,
                (u32::from(i.first_group.get()) << 24)
                    | (u32::from(i.groups_spanned.get()) << 16)
                    | (u32::from(i.midi_ci_version) << 8)
                    | u32::from(i.max_sysex8_streams),
                0,
                0,
            ]
        }
        Stream::FunctionBlockName {
            block,
            form,
            bytes: b,
        } => [
            stream_head(form, 0x12) | (u32::from(block.get()) << 8) | u32::from(b[0]),
            word([b[1], b[2], b[3], b[4]]),
            word([b[5], b[6], b[7], b[8]]),
            word([b[9], b[10], b[11], b[12]]),
        ],
        Stream::StartOfClip => [stream_head(done, 0x20), 0, 0, 0],
        Stream::EndOfClip => [stream_head(done, 0x21), 0, 0, 0],
    }
}
