//! The message catalogue: one value per defined M2-104 message, `Copy`, with
//! no lifetime.

use crate::field::{
    DeltaTicks, FirstGroup, FunctionBlockId, GroupSpan, NoteNumber, Pitch, SharpsFlats, U4, U7, U14,
};
use crate::{CCNumber, MidiChannel, MidiGroup, Ump};

/// One UMP, decoded.
///
/// `Midi1` and `Midi2` are different message types on the wire and stay
/// different here. Utility and Stream messages have no group (M2-104 §2.1.2),
/// so they carry none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Message {
    /// Message type `0x0`.
    Utility(Utility),
    /// Message type `0x1`.
    System(MidiGroup, System),
    /// Message type `0x2`.
    Midi1(Address, Voice1),
    /// Message type `0x3`: one System Exclusive packet.
    Sysex7(MidiGroup, Sysex7Packet),
    /// Message type `0x4`.
    Midi2(Address, Voice2),
    /// Message type `0x5`: System Exclusive 8 and Mixed Data Set.
    Data128(MidiGroup, Data128),
    /// Message type `0xD`.
    Flex(MidiGroup, Flex),
    /// Message type `0xF`.
    Stream(Stream),
}

/// A packet no definition covers: a reserved message type, a defined type
/// with an undefined status, or a defined message with a reserved value in a
/// field. The words are kept, so a later revision's message can be forwarded
/// unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unknown(pub Ump);

/// A Group and a Channel within it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Address {
    /// The Group.
    pub group: MidiGroup,
    /// The Channel in that Group.
    pub channel: MidiChannel,
}

// ---- Utility ---------------------------------------------------------------

/// Utility messages (M2-104 §7.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Utility {
    /// No operation.
    Noop,
    /// The sender's clock, in ticks of 1/31250 s (wraps).
    JrClock(u16),
    /// The time of the following messages, in ticks of 1/31250 s.
    JrTimestamp(u16),
    /// Delta Clockstamp ticks per quarter note; `0` is reserved and kept.
    TicksPerQuarterNote(u16),
    /// Ticks since the last event.
    DeltaClockstamp(DeltaTicks),
}

// ---- System ----------------------------------------------------------------

/// System Common and System Real Time messages (M2-104 §7.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum System {
    /// MIDI Time Code quarter frame: the byte `0nnndddd`.
    TimeCode(U7),
    /// Song Position Pointer, in MIDI beats.
    SongPosition(U14),
    /// Song Select.
    SongSelect(U7),
    /// Tune Request.
    TuneRequest,
    /// Timing Clock.
    TimingClock,
    /// Start.
    Start,
    /// Continue.
    Continue,
    /// Stop.
    Stop,
    /// Active Sensing.
    ActiveSensing,
    /// Reset.
    Reset,
}

// ---- MIDI 1.0 channel voice ------------------------------------------------

/// MIDI 1.0 Channel Voice messages (M2-104 §7.3). A Note On at velocity 0 is
/// a Note On here; reading it as a Note Off is translation's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Voice1 {
    /// Note Off.
    NoteOff {
        /// The note.
        note: NoteNumber,
        /// The release velocity.
        velocity: U7,
    },
    /// Note On.
    NoteOn {
        /// The note.
        note: NoteNumber,
        /// The velocity.
        velocity: U7,
    },
    /// Polyphonic key pressure.
    PolyPressure {
        /// The note.
        note: NoteNumber,
        /// The pressure.
        pressure: U7,
    },
    /// Control Change.
    ControlChange {
        /// The controller.
        controller: CCNumber,
        /// Its value.
        value: U7,
    },
    /// Program Change.
    ProgramChange(U7),
    /// Channel pressure.
    ChannelPressure(U7),
    /// Pitch Bend, `0x2000` centred.
    PitchBend(U14),
}

// ---- System Exclusive ------------------------------------------------------

/// The role of a packet in a System Exclusive message (Tables 18 and 19).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SysexStatus {
    /// The whole message.
    Complete,
    /// The first of several.
    Start,
    /// A middle one.
    Continue,
    /// The last of several.
    End,
}

/// One System Exclusive (7-bit) packet: up to 6 data bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sysex7Packet {
    status: SysexStatus,
    len: u8,
    bytes: [U7; 6],
}

impl Sysex7Packet {
    /// The packet, or `None` for more than 6 bytes or a byte of 128 or more.
    #[must_use]
    pub fn new(status: SysexStatus, data: &[u8]) -> Option<Self> {
        if data.len() > 6 {
            return None;
        }
        let mut bytes = [U7::default(); 6];
        for (slot, &b) in bytes.iter_mut().zip(data) {
            *slot = U7::new(b)?;
        }
        Some(Self {
            status,
            len: data.len() as u8,
            bytes,
        })
    }

    /// The packet's role.
    #[must_use]
    pub const fn status(&self) -> SysexStatus {
        self.status
    }

    /// The valid data bytes.
    #[must_use]
    pub fn bytes(&self) -> &[U7] {
        self.bytes.split_at(self.len as usize).0
    }

    /// The valid data bytes as plain bytes: the same bytes as
    /// [`bytes`](Self::bytes), viewed in place without a copy. Each is below
    /// 128.
    #[must_use]
    pub fn data(&self) -> &[u8] {
        bytemuck::cast_slice(self.bytes())
    }
}

/// One System Exclusive 8 packet: a stream id and up to 13 data bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sysex8Packet {
    status: SysexStatus,
    stream_id: u8,
    len: u8,
    bytes: [u8; 13],
}

impl Sysex8Packet {
    /// The packet, or `None` for more than 13 data bytes.
    #[must_use]
    pub fn new(status: SysexStatus, stream_id: u8, data: &[u8]) -> Option<Self> {
        if data.len() > 13 {
            return None;
        }
        let mut bytes = [0; 13];
        bytes[..data.len()].copy_from_slice(data);
        Some(Self {
            status,
            stream_id,
            len: data.len() as u8,
            bytes,
        })
    }

    /// The packet's role.
    #[must_use]
    pub const fn status(&self) -> SysexStatus {
        self.status
    }

    /// The stream id.
    #[must_use]
    pub const fn stream_id(&self) -> u8 {
        self.stream_id
    }

    /// The valid data bytes, after the stream id.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        self.bytes.split_at(self.len as usize).0
    }
}

/// Message type `0x5` (M2-104 §7.8 and §7.9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Data128 {
    /// A System Exclusive 8 packet.
    Sysex8(Sysex8Packet),
    /// An End packet with `# of bytes` `0xF`: the sender ran out of data and
    /// the earlier data of this stream is incomplete (§7.8.1).
    /// The stream id is read from byte 3; §7.8.1 does not say it is kept, so a
    /// sender that zeroes it aborts stream 0.
    Sysex8Abort {
        /// The stream being aborted.
        stream_id: u8,
    },
    /// A Mixed Data Set chunk header.
    MixedDataHeader(MixedDataHeader),
    /// A Mixed Data Set payload packet: 14 bytes, zero-padded.
    MixedDataPayload {
        /// The set this belongs to.
        mds_id: U4,
        /// The payload.
        bytes: [u8; 14],
    },
}

/// A Mixed Data Set chunk header (§7.9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MixedDataHeader {
    /// The set this chunk belongs to.
    pub mds_id: U4,
    /// Valid bytes in this chunk, header included.
    pub chunk_bytes: u16,
    /// Chunks in the set; `0` when unknown.
    pub chunks: u16,
    /// This chunk's number, from 1; `0` marks an unverified end.
    pub chunk: u16,
    /// The 16-bit manufacturer id (§7.10).
    pub manufacturer: u16,
    /// The device id.
    pub device: u16,
    /// Sub id 1.
    pub sub_id_1: u16,
    /// Sub id 2.
    pub sub_id_2: u16,
}

// ---- MIDI 2.0 channel voice ------------------------------------------------

/// A Note On or Note Off attribute (§7.4.14). An attribute type of `0x00`
/// carries no data, and `0x04` to `0xFF` are reserved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attribute {
    /// Type `0x00`.
    None,
    /// Type `0x01`: manufacturer specific.
    Manufacturer(u16),
    /// Type `0x02`: specific to the active MIDI-CI Profile.
    Profile(u16),
    /// Type `0x03`: Q7.9 pitch in half-tone units.
    Pitch(u16),
    /// A reserved type (`0x04` to `0xFF`), kept with its data.
    Reserved {
        /// The attribute type.
        kind: u8,
        /// The attribute data.
        data: u16,
    },
}

/// A Registered Per-Note Controller number (Appendix A, Table 22). A number
/// the table leaves reserved is kept in [`Reserved`](Self::Reserved), so its
/// message re-encodes exactly. Build one from a wire number with [`From<u8>`],
/// which names every number the table defines and only then falls back to
/// `Reserved`. A hand-built `Reserved(7)` encodes as 7 but does not compare
/// equal to, or match, [`Volume`](Self::Volume).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegisteredPerNoteController {
    /// 1: Modulation.
    Modulation,
    /// 2: Breath.
    Breath,
    /// 3: Pitch 7.25 (§7.4.15.2); the value is a [`Pitch`] in Q7.25.
    Pitch,
    /// 7: Volume.
    Volume,
    /// 8: Balance.
    Balance,
    /// 10: Pan.
    Pan,
    /// 11: Expression.
    Expression,
    /// 70: Sound Controller 1, Sound Variation.
    SoundVariation,
    /// 71: Sound Controller 2, Timbre / Harmonic Intensity.
    Timbre,
    /// 72: Sound Controller 3, Release Time.
    ReleaseTime,
    /// 73: Sound Controller 4, Attack Time.
    AttackTime,
    /// 74: Sound Controller 5, Brightness.
    Brightness,
    /// 75: Sound Controller 6, Decay Time.
    DecayTime,
    /// 76: Sound Controller 7, Vibrato Rate.
    VibratoRate,
    /// 77: Sound Controller 8, Vibrato Depth.
    VibratoDepth,
    /// 78: Sound Controller 9, Vibrato Delay.
    VibratoDelay,
    /// 79: Sound Controller 10, undefined.
    SoundController10,
    /// 91: Effects 1 Depth, Reverb Send Level.
    ReverbSend,
    /// 92: Effects 2 Depth (formerly Tremolo Depth).
    Effects2Depth,
    /// 93: Effects 3 Depth, Chorus Send Level.
    ChorusSend,
    /// 94: Effects 4 Depth (formerly Celeste Depth).
    Effects4Depth,
    /// 95: Effects 5 Depth (formerly Phaser Depth).
    Effects5Depth,
    /// A number the table reserves.
    Reserved(u8),
}

impl RegisteredPerNoteController {
    /// The controller number on the wire.
    #[must_use]
    pub const fn number(self) -> u8 {
        match self {
            Self::Modulation => 1,
            Self::Breath => 2,
            Self::Pitch => 3,
            Self::Volume => 7,
            Self::Balance => 8,
            Self::Pan => 10,
            Self::Expression => 11,
            Self::SoundVariation => 70,
            Self::Timbre => 71,
            Self::ReleaseTime => 72,
            Self::AttackTime => 73,
            Self::Brightness => 74,
            Self::DecayTime => 75,
            Self::VibratoRate => 76,
            Self::VibratoDepth => 77,
            Self::VibratoDelay => 78,
            Self::SoundController10 => 79,
            Self::ReverbSend => 91,
            Self::Effects2Depth => 92,
            Self::ChorusSend => 93,
            Self::Effects4Depth => 94,
            Self::Effects5Depth => 95,
            Self::Reserved(n) => n,
        }
    }
}

impl From<u8> for RegisteredPerNoteController {
    fn from(number: u8) -> Self {
        match number {
            1 => Self::Modulation,
            2 => Self::Breath,
            3 => Self::Pitch,
            7 => Self::Volume,
            8 => Self::Balance,
            10 => Self::Pan,
            11 => Self::Expression,
            70 => Self::SoundVariation,
            71 => Self::Timbre,
            72 => Self::ReleaseTime,
            73 => Self::AttackTime,
            74 => Self::Brightness,
            75 => Self::DecayTime,
            76 => Self::VibratoRate,
            77 => Self::VibratoDepth,
            78 => Self::VibratoDelay,
            79 => Self::SoundController10,
            91 => Self::ReverbSend,
            92 => Self::Effects2Depth,
            93 => Self::ChorusSend,
            94 => Self::Effects4Depth,
            95 => Self::Effects5Depth,
            n => Self::Reserved(n),
        }
    }
}

impl From<RegisteredPerNoteController> for u8 {
    fn from(controller: RegisteredPerNoteController) -> u8 {
        controller.number()
    }
}

impl Attribute {
    /// The pitch of an [`Attribute::Pitch`] (§7.4.15.3), or `None` for any other
    /// attribute.
    #[must_use]
    pub const fn pitch(self) -> Option<Pitch> {
        match self {
            Self::Pitch(q7_9) => Some(Pitch::from_q7_9(q7_9)),
            _ => None,
        }
    }
}

/// A Program Change's bank (§7.4.9), present when the Bank Valid bit is set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bank {
    /// Bank MSB.
    pub msb: U7,
    /// Bank LSB.
    pub lsb: U7,
}

/// MIDI 2.0 Channel Voice messages (M2-104 §7.4). Values are the wire's
/// integers; M2-115 scaling is translation's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Voice2 {
    /// Registered Per-Note Controller.
    RegisteredPerNote {
        /// The note.
        note: NoteNumber,
        /// The controller.
        controller: RegisteredPerNoteController,
        /// Its value.
        value: u32,
    },
    /// Assignable Per-Note Controller.
    AssignablePerNote {
        /// The note.
        note: NoteNumber,
        /// The controller, `0..=255`. Not a [`CCNumber`]: a Control Change
        /// index is 7 bits, and this field is a full byte with 256
        /// controllers (§7.4.4), so a `CCNumber` could not hold one above 127.
        controller: u8,
        /// Its value.
        value: u32,
    },
    /// Registered Controller (RPN).
    Registered {
        /// Bank (the RPN MSB).
        bank: U7,
        /// Index (the RPN LSB).
        index: U7,
        /// Its value.
        value: u32,
    },
    /// Assignable Controller (NRPN).
    Assignable {
        /// Bank (the NRPN MSB).
        bank: U7,
        /// Index (the NRPN LSB).
        index: U7,
        /// Its value.
        value: u32,
    },
    /// Relative Registered Controller.
    RelativeRegistered {
        /// Bank.
        bank: U7,
        /// Index.
        index: U7,
        /// The two's-complement change.
        delta: i32,
    },
    /// Relative Assignable Controller.
    RelativeAssignable {
        /// Bank.
        bank: U7,
        /// Index.
        index: U7,
        /// The two's-complement change.
        delta: i32,
    },
    /// Per-Note Pitch Bend, `0x8000_0000` centred.
    PerNotePitchBend {
        /// The note.
        note: NoteNumber,
        /// The bend.
        value: u32,
    },
    /// Note Off.
    NoteOff {
        /// The note.
        note: NoteNumber,
        /// The 16-bit release velocity.
        velocity: u16,
        /// The attribute.
        attribute: Attribute,
    },
    /// Note On. Velocity 0 is a Note On (§7.4.2).
    NoteOn {
        /// The note.
        note: NoteNumber,
        /// The 16-bit velocity.
        velocity: u16,
        /// The attribute.
        attribute: Attribute,
    },
    /// Polyphonic key pressure.
    PolyPressure {
        /// The note.
        note: NoteNumber,
        /// The pressure.
        value: u32,
    },
    /// Control Change.
    ControlChange {
        /// The controller.
        controller: CCNumber,
        /// Its value.
        value: u32,
    },
    /// Program Change, with an optional bank.
    ProgramChange {
        /// The program.
        program: U7,
        /// The bank, when the Bank Valid bit is set.
        bank: Option<Bank>,
    },
    /// Channel pressure.
    ChannelPressure(u32),
    /// Pitch Bend, `0x8000_0000` centred.
    PitchBend(u32),
    /// Per-Note Management.
    PerNoteManagement {
        /// The note.
        note: NoteNumber,
        /// Detach controllers from notes already playing (the D flag).
        detach: bool,
        /// Reset controllers to their defaults (the S flag).
        reset: bool,
    },
}

impl Voice2 {
    /// The note a message addresses: the one in byte 3 of a Note On or Off,
    /// Polyphonic Pressure, Per-Note Pitch Bend, a Registered or Assignable
    /// Per-Note Controller or Per-Note Management (M2-104 section 7.4). `None`
    /// for a channel-wide message.
    #[inline]
    #[must_use]
    pub const fn note(&self) -> Option<NoteNumber> {
        match *self {
            Self::RegisteredPerNote { note, .. }
            | Self::AssignablePerNote { note, .. }
            | Self::PerNotePitchBend { note, .. }
            | Self::NoteOff { note, .. }
            | Self::NoteOn { note, .. }
            | Self::PolyPressure { note, .. }
            | Self::PerNoteManagement { note, .. } => Some(note),
            Self::Registered { .. }
            | Self::Assignable { .. }
            | Self::RelativeRegistered { .. }
            | Self::RelativeAssignable { .. }
            | Self::ControlChange { .. }
            | Self::ProgramChange { .. }
            | Self::ChannelPressure(_)
            | Self::PitchBend(_) => None,
        }
    }
}

// ---- Flex Data -------------------------------------------------------------

/// Where a Flex Data message goes (Table 10): one Channel, or the whole Group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlexAddress {
    /// The Channel in the packet's Group.
    Channel(MidiChannel),
    /// The Group.
    Group,
    /// A reserved address (2 or 3), kept.
    Reserved(u8),
}

/// A note letter: `A` is 1 to `G` is 7. The field's `0` is `None` where a
/// message uses it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Letter {
    /// A.
    A,
    /// B.
    B,
    /// C.
    C,
    /// D.
    D,
    /// E.
    E,
    /// F.
    F,
    /// G.
    G,
    /// A reserved value (8 to 15), kept.
    Reserved(u8),
}

/// Flex Data messages (M2-104 §7.5). Set Tempo, Set Time Signature and Set
/// Metronome are Group messages and carry no address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flex {
    /// Set Tempo: units of 10 ns per quarter note.
    SetTempo(u32),
    /// Set Time Signature.
    SetTimeSignature(TimeSignature),
    /// Set Metronome.
    SetMetronome(Metronome),
    /// Set Key Signature.
    SetKeySignature(FlexAddress, KeySignature),
    /// Set Chord Name.
    SetChordName(FlexAddress, ChordName),
    /// A text message: one packet's 12 bytes, zero-padded.
    Text {
        /// Where it goes.
        address: FlexAddress,
        /// Which text.
        kind: FlexTextKind,
        /// The packet's place in the message.
        form: Form,
        /// The bytes.
        bytes: [u8; 12],
    },
}

/// Set Time Signature fields (§7.5.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimeSignature {
    /// The numerator field.
    pub numerator: u8,
    /// The denominator as a negative power of 2; `0` is non-standard.
    pub denominator: u8,
    /// 1/32 notes in 24 MIDI clocks.
    pub thirty_seconds: u8,
}

/// Set Metronome fields (§7.5.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Metronome {
    /// MIDI clocks per primary click.
    pub clocks_per_click: u8,
    /// Bar accent part 1.
    pub accent_1: u8,
    /// Bar accent part 2.
    pub accent_2: u8,
    /// Bar accent part 3.
    pub accent_3: u8,
    /// Subdivision clicks, first set.
    pub subdivision_1: u8,
    /// Subdivision clicks, second set.
    pub subdivision_2: u8,
}

/// Set Key Signature fields (§7.5.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeySignature {
    /// Sharps or flats; `None` for the field's `-8`, unknown or non-standard.
    pub sharps_flats: Option<SharpsFlats>,
    /// The tonic; `None` is unknown or non-standard.
    pub tonic: Option<Letter>,
}

/// An accidental on a chord note (Tables 13 and 15).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Accidental {
    /// Two flats.
    DoubleFlat,
    /// One flat.
    Flat,
    /// Natural.
    Natural,
    /// One sharp.
    Sharp,
    /// Two sharps.
    DoubleSharp,
    /// A value the tables reserve, kept as the raw nibble.
    Reserved(u8),
}

/// What an alteration does to a chord degree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlterationKind {
    /// No alteration.
    None,
    /// Add the degree.
    Add,
    /// Subtract the degree.
    Subtract,
    /// Raise the degree, adding it if needed.
    Raise,
    /// Lower the degree, adding it if needed.
    Lower,
    /// A reserved type (5 to 15), kept.
    Reserved(u8),
}

/// A chord alteration: a kind and the degree it acts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Alteration {
    /// What it does.
    pub kind: AlterationKind,
    /// The degree (1 is the root, 3 the third).
    pub degree: U4,
}

/// A chord type, `0x00..=0x1B` (Table 14). A reserved type read from the wire
/// is kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChordType(u8);

impl ChordType {
    /// The type, or `None` above `0x1B`.
    #[must_use]
    pub const fn new(raw: u8) -> Option<Self> {
        if raw <= 0x1B { Some(Self(raw)) } else { None }
    }

    /// The Table 14 value.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }

    pub(crate) const fn wire(raw: u8) -> Self {
        Self(raw)
    }
}

/// A chord note with its accidental.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChordPitch {
    /// The accidental.
    pub accidental: Accidental,
    /// The note letter.
    pub letter: Letter,
}

/// A chord's bass note and the chord on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChordBass {
    /// The bass note; `None` is the chord's own tonic (sharps/flats `-8`,
    /// note `0`).
    pub pitch: Option<ChordPitch>,
    /// The bass chord type; `0` clears the bass chord.
    pub chord_type: ChordType,
    /// Up to two alterations of the bass chord.
    pub alterations: [Alteration; 2],
}

/// Set Chord Name fields (§7.5.8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChordName {
    /// The tonic's accidental.
    pub tonic_accidental: Accidental,
    /// The tonic; `None` is unknown (no chord).
    pub tonic: Option<Letter>,
    /// The chord type.
    pub chord_type: ChordType,
    /// Up to four alterations.
    pub alterations: [Alteration; 4],
    /// The bass.
    pub bass: ChordBass,
}

/// The messages that use the Flex text format (Table 16).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlexTextKind {
    /// Bank 1, status 0.
    UnknownMetadata,
    /// Bank 1, status 1.
    ProjectName,
    /// Bank 1, status 2.
    CompositionName,
    /// Bank 1, status 3.
    ClipName,
    /// Bank 1, status 4.
    Copyright,
    /// Bank 1, status 5.
    Composer,
    /// Bank 1, status 6.
    Lyricist,
    /// Bank 1, status 7.
    Arranger,
    /// Bank 1, status 8.
    Publisher,
    /// Bank 1, status 9.
    PrimaryPerformer,
    /// Bank 1, status 0xA.
    AccompanyingPerformer,
    /// Bank 1, status 0xB (ISO 8601).
    RecordingDate,
    /// Bank 1, status 0xC.
    RecordingLocation,
    /// Bank 2, status 0.
    UnknownPerformance,
    /// Bank 2, status 1.
    Lyrics,
    /// Bank 2, status 2.
    LyricsLanguage,
    /// Bank 2, status 3.
    Ruby,
    /// Bank 2, status 4.
    RubyLanguage,
}

// ---- UMP Stream ------------------------------------------------------------

/// A packet's place in a multi-packet message (Stream and Flex Format).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Form {
    /// The whole message.
    Complete,
    /// The first packet.
    Start,
    /// A middle packet.
    Continue,
    /// The last packet.
    End,
}

/// A UMP version (Table 5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UmpVersion {
    /// Major.
    pub major: u8,
    /// Minor.
    pub minor: u8,
}

/// Which notifications an Endpoint Discovery asks for (§7.1.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiscoveryFilter {
    /// Endpoint Info.
    pub endpoint_info: bool,
    /// Device Identity.
    pub device_identity: bool,
    /// Endpoint Name.
    pub endpoint_name: bool,
    /// Product Instance Id.
    pub product_instance_id: bool,
    /// Stream Configuration.
    pub stream_configuration: bool,
}

/// An Endpoint Info Notification (§7.1.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EndpointInfo {
    /// The UMP version.
    pub version: UmpVersion,
    /// The Function Blocks never change.
    pub static_function_blocks: bool,
    /// How many Function Blocks.
    /// How many Function Blocks: `0..=32` is defined, `33..=127` reserved and
    /// kept.
    pub function_blocks: U7,
    /// Supports the MIDI 2.0 Protocol.
    pub midi2: bool,
    /// Supports the MIDI 1.0 Protocol.
    pub midi1: bool,
    /// Receives JR Timestamps.
    pub rx_jr: bool,
    /// Transmits JR Timestamps.
    pub tx_jr: bool,
}

/// A device's identity, as in a Device Inquiry reply (§7.1.3). Every byte is 7-bit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceIdentity {
    /// The manufacturer's SysEx id.
    pub manufacturer: [U7; 3],
    /// The family id, LSB first.
    pub family: [U7; 2],
    /// The model id, LSB first.
    pub model: [U7; 2],
    /// The software revision.
    pub revision: [U7; 4],
}

/// The MIDI Protocol of a stream (§7.1.6.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    /// MIDI 1.0 (`0x01`).
    Midi1,
    /// MIDI 2.0 (`0x02`).
    Midi2,
    /// A reserved value, kept.
    Reserved(u8),
}

/// The payload of Stream Configuration Request and Notification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamConfig {
    /// The protocol.
    pub protocol: Protocol,
    /// Receive JR Timestamps.
    pub rx_jr: bool,
    /// Transmit JR Timestamps.
    pub tx_jr: bool,
}

/// Which Function Block a Discovery asks about (§7.1.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockSelect {
    /// One block. A number above 31 is reserved and kept.
    One(FunctionBlockId),
    /// All of them (`0xFF`).
    All,
}

/// Which notifications a Function Block Discovery asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockFilter {
    /// Function Block Info.
    pub info: bool,
    /// Function Block Name.
    pub name: bool,
}

/// A Function Block's connection direction (§7.1.8); `0b00` is reserved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Receives only.
    Input,
    /// Transmits only.
    Output,
    /// Both.
    Bidirectional,
    /// The reserved `0b00`, kept.
    Reserved(u8),
}

/// The primary role a Function Block declares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiHint {
    /// Unknown or undeclared.
    Unknown,
    /// Mainly a receiver.
    Receiver,
    /// Mainly a sender.
    Sender,
    /// Both.
    Both,
}

/// Whether a Function Block is a MIDI 1.0 port; `0x03` is reserved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Midi1Port {
    /// Not a MIDI 1.0 port.
    No,
    /// A MIDI 1.0 port, bandwidth unrestricted.
    Unrestricted,
    /// A MIDI 1.0 port restricted to 31.25 kbps.
    Restricted,
    /// The reserved `0x03`, kept.
    Reserved(u8),
}

/// A Function Block Info Notification (§7.1.8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FunctionBlockInfo {
    /// Whether the block is active.
    pub active: bool,
    /// Which block.
    pub id: FunctionBlockId,
    /// Its direction.
    pub direction: Direction,
    /// Its UI hint.
    pub ui_hint: UiHint,
    /// MIDI 1.0 port status.
    pub midi1: Midi1Port,
    /// The first Group it spans.
    pub first_group: FirstGroup,
    /// How many Groups it spans.
    pub groups_spanned: GroupSpan,
    /// The MIDI-CI message version, `0` for none.
    pub midi_ci_version: u8,
    /// Simultaneous SysEx 8 streams supported.
    pub max_sysex8_streams: u8,
}

/// UMP Stream messages (M2-104 §7.1). They address the endpoint, so they have
/// no Group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stream {
    /// Endpoint Discovery.
    EndpointDiscovery {
        /// The sender's UMP version.
        version: UmpVersion,
        /// What it asks for.
        filter: DiscoveryFilter,
    },
    /// Endpoint Info Notification.
    EndpointInfo(EndpointInfo),
    /// Device Identity Notification.
    DeviceIdentity(DeviceIdentity),
    /// Endpoint Name Notification: 14 UTF-8 bytes, zero-padded.
    EndpointName(Form, [u8; 14]),
    /// Product Instance Id Notification: 14 ASCII bytes, zero-padded.
    ProductInstanceId(Form, [u8; 14]),
    /// Stream Configuration Request.
    StreamConfigurationRequest(StreamConfig),
    /// Stream Configuration Notification.
    StreamConfigurationNotification(StreamConfig),
    /// Function Block Discovery.
    FunctionBlockDiscovery {
        /// The block asked about.
        block: BlockSelect,
        /// What it asks for.
        filter: BlockFilter,
    },
    /// Function Block Info Notification.
    FunctionBlockInfo(FunctionBlockInfo),
    /// Function Block Name Notification: 13 UTF-8 bytes, zero-padded.
    FunctionBlockName {
        /// Which block.
        block: FunctionBlockId,
        /// The packet's place.
        form: Form,
        /// The bytes.
        bytes: [u8; 13],
    },
    /// Start of Clip.
    StartOfClip,
    /// End of Clip.
    EndOfClip,
}
