//! Universal System Exclusive messages (M1 pp.34 to 57): the ones an instrument
//! or a device inquiry reads, as typed values.
//!
//! [`read()`](fn@read) turns a whole SysEx message ([`SysexMessage`], which
//! `tutti_ump::SysexAssembler` hands out) into a [`Read`]. It is a pure parse of
//! one message: no state, no allocation. [`UniversalReader`] is the instrument's
//! edge: it takes every UMP of one Group, reassembles the SysEx7 packets, reads
//! the message and answers only the ones addressed to the instrument
//! ([`SysexDeviceId`]).
//!
//! # What is read
//!
//! - Identity Request and Identity Reply (the Device Inquiry, M1 p.40; the
//!   reply is a [`DeviceIdentity`]).
//! - General MIDI System On and Off (M1 p.52, `09 01` and `09 02`).
//! - Master Volume and Master Balance (M1 p.57, `04 01` and `04 02`).
//! - Master Fine Tuning and Master Coarse Tuning (CA-025, `04 03` and `04 04`).
//! - The MTC Full Message (Real Time `01 01`, M1 p.53 and T-9): a whole SMPTE
//!   position in one message, as [`MtcFullMessage`].
//! - MIDI Machine Control (RP-013): Real Time `06` commands as [`MmcCommands`]
//!   and `07` responses as [`MmcFields`], with every command and information
//!   field of the specification typed ([`MmcCommand`], [`MmcInfoField`]).
//! - The MIDI Tuning Standard (`08`): [`TuningMessage`], with its
//!   [frequency data](crate::TuningFrequency). A dump whose checksum does not hold is
//!   [`Read::BadChecksum`], not `Unread`.
//!
//! Anything else is [`Read::Unread`] when it is Universal (another sub-id, or one
//! of these cut short or with data bytes over), and [`Read::Foreign`] when the
//! SysEx id is a manufacturer's. Neither is an error: a stream is never dropped
//! for a message nobody here reads. MIDI-CI (`7E dev 0D`) is Universal too and
//! is `tutti-midi-device`'s codec; here it is `Unread`.
//!
//! # The device id
//!
//! `7F` is the all-call: every device responds whatever id it is set to
//! (M1 p.40). Any other id is for
//! the device that has it. A device with no id of its own to say is
//! [`SysexDeviceId::DEFAULT`]; an instrument takes the messages addressed to it or to
//! all ([`Addressee`]). Master Volume and Balance keep the two apart, as M1
//! p.57 requires (a message on its own id and one on the All Call id are distinct).

mod machine_control;
mod read;

use crate::smpte::{MtcRate, SmpteTimecode};
use crate::tuning::TuningMessage;

pub use machine_control::*;

use tutti_ump::{DeviceIdentity, MidiGroup, SysexAssembler, SysexMessage, SysexStep, U7, U14, Ump};

/// A device's own Universal System Exclusive id: `00` to `7E`. `7F` is the
/// all-call (M1 p.40), which every device answers and none has as its own, so
/// [`new`](Self::new) refuses it; a message's header is a [`Recipient`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SysexDeviceId(U7);

/// A `7F`, which is the all-call and no device's own id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{:#04x} is the all-call, not a device's own id", .0.get())]
pub struct NotSysexDeviceId(pub U7);

impl SysexDeviceId {
    /// The id a device that has none set takes: `00`.
    pub const DEFAULT: Self = Self(U7::of(0));

    /// The id `id`, or `None` for `7F`, the all-call.
    #[must_use]
    pub const fn new(id: U7) -> Option<Self> {
        if id.get() == U7::MAX.get() {
            None
        } else {
            Some(Self(id))
        }
    }

    /// The id for a literal or a constant.
    ///
    /// # Panics
    ///
    /// When `id` is above `7E`; in a `const` that is a compile error.
    #[must_use]
    #[track_caller]
    pub const fn of(id: u8) -> Self {
        match Self::new(U7::of(id)) {
            Some(id) => id,
            None => panic!("7F is the all-call, not a device's own id"),
        }
    }

    /// The id as its 7-bit byte.
    #[must_use]
    pub const fn get(self) -> U7 {
        self.0
    }
}

impl From<SysexDeviceId> for U7 {
    fn from(id: SysexDeviceId) -> Self {
        id.0
    }
}

impl TryFrom<U7> for SysexDeviceId {
    type Error = NotSysexDeviceId;

    fn try_from(id: U7) -> Result<Self, Self::Error> {
        Self::new(id).ok_or(NotSysexDeviceId(id))
    }
}

/// A time code at a rate that has an MTC code (24, 25, 29.97 drop-frame or 30
/// frames): the labels `hours:minutes:seconds:frames` checked against their
/// ranges, so a value of this type is a position some clock can read. It is
/// what an MTC Full Message and an MMC time code both state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MtcTimeCode {
    rate: MtcRate,
    timecode: SmpteTimecode,
}

impl MtcTimeCode {
    /// `00:00:00:00` at `rate`.
    #[must_use]
    pub const fn origin(rate: MtcRate) -> Self {
        Self {
            rate,
            timecode: SmpteTimecode::origin(rate.frame_rate()),
        }
    }

    /// The position `hours:minutes:seconds:frames` at `rate`, or `None` when a
    /// field is out of range: hours 0..=23, minutes and seconds 0..=59, frames
    /// below [`MtcRate::labels_per_second`].
    #[must_use]
    pub const fn new(
        rate: MtcRate,
        hours: u8,
        minutes: u8,
        seconds: u8,
        frames: u8,
    ) -> Option<Self> {
        match SmpteTimecode::new(hours, minutes, seconds, frames, rate.frame_rate()) {
            Some(timecode) => Some(Self { rate, timecode }),
            None => None,
        }
    }

    /// The frame rate.
    #[must_use]
    pub const fn rate(self) -> MtcRate {
        self.rate
    }

    /// The time code, for counting frames and reading seconds.
    #[must_use]
    pub const fn timecode(self) -> SmpteTimecode {
        self.timecode
    }
}

/// An MTC Full Message (Real Time `01 01`, M1 p.53 and T-9): one SMPTE position
/// and its rate, sent in place of Quarter Frames when a generator locates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MtcFullMessage(MtcTimeCode);

impl MtcFullMessage {
    /// The message of the position `hours:minutes:seconds:frames` at `rate`, or
    /// `None` when a field is out of range.
    #[must_use]
    pub const fn new(
        rate: MtcRate,
        hours: u8,
        minutes: u8,
        seconds: u8,
        frames: u8,
    ) -> Option<Self> {
        match MtcTimeCode::new(rate, hours, minutes, seconds, frames) {
            Some(time) => Some(Self(time)),
            None => None,
        }
    }

    /// The time code the message states.
    #[must_use]
    pub const fn time_code(self) -> MtcTimeCode {
        self.0
    }

    /// The frame rate.
    #[must_use]
    pub const fn rate(self) -> MtcRate {
        self.0.rate()
    }

    /// Hours, 0..=23.
    #[must_use]
    pub const fn hours(self) -> u8 {
        self.0.timecode().hours()
    }

    /// Minutes, 0..=59.
    #[must_use]
    pub const fn minutes(self) -> u8 {
        self.0.timecode().minutes()
    }

    /// Seconds, 0..=59.
    #[must_use]
    pub const fn seconds(self) -> u8 {
        self.0.timecode().seconds()
    }

    /// Frames within the second, below the rate's label count.
    #[must_use]
    pub const fn frames(self) -> u8 {
        self.0.timecode().frames()
    }

    /// The message of the four data bytes `hr mn sc fr` that follow `01 01`:
    /// `hr` is `0rrhhhhh`, the rate code over the hours. `None` for a byte
    /// that is not a data byte or a field out of range.
    #[must_use]
    pub(crate) const fn from_wire(hr: u8, mn: u8, sc: u8, fr: u8) -> Option<Self> {
        if hr >= 0x80 {
            return None;
        }
        Self::new(MtcRate::masked(hr >> 5), hr & 0x1F, mn, sc, fr)
    }
}

/// Who a Universal message's header names: every device (`7F`) or one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Recipient {
    /// The all-call, `7F` (M1 p.40).
    AllCall,
    /// The device with this id.
    Device(SysexDeviceId),
}

impl Recipient {
    /// The recipient a header byte names.
    #[must_use]
    pub const fn of_byte(id: U7) -> Self {
        match SysexDeviceId::new(id) {
            Some(device) => Self::Device(device),
            None => Self::AllCall,
        }
    }
}

/// General MIDI System On or Off (M1 p.52).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GeneralMidi {
    /// `09 01`: the device goes to General MIDI System Level 1's initial status.
    On,
    /// `09 02`: a device with another initialized mode returns to it.
    Off,
}

/// A Universal message this crate reads. Fields are the wire's values, in the
/// type that holds them; what a value means (a curve, a displacement) is the
/// consumer's.
// Copy, and read on the audio thread: boxing the table or the changes would allocate there.
#[allow(clippy::large_enum_variant)]
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Universal {
    /// Identity Request (`06 01`, M1 p.40).
    IdentityRequest,
    /// Identity Reply (`06 02`, M1 p.40).
    IdentityReply(DeviceIdentity),
    /// General MIDI System On or Off (`09`, M1 p.52).
    GeneralMidi(GeneralMidi),
    /// Master Volume (`04 01`, M1 p.57): 14 bits, `0` is off.
    MasterVolume(U14),
    /// Master Balance (`04 02`, M1 p.57): 14 bits, `0` is hard left and
    /// `0x3FFF` hard right.
    MasterBalance(U14),
    /// Master Fine Tuning (`04 03`, CA-025): 14 bits, `0x2000` is A440 and a
    /// step is 100/8192 cents.
    MasterFineTuning(U14),
    /// Master Coarse Tuning (`04 04`, CA-025): the MSB, `0x40` is A440 and a
    /// step is a semitone.
    MasterCoarseTuning(U7),
    /// A MIDI Tuning Standard message (`08`, CA-020 and CA-021).
    Tuning(TuningMessage),
    /// An MTC Full Message (Real Time `01 01`, M1 p.53 and T-9).
    MtcFullMessage(MtcFullMessage),
    /// MIDI Machine Control commands (Real Time `06`, RP-013 p.2). The
    /// header's recipient is the destination: a device, a group or all.
    MachineControl(machine_control::MmcCommands),
    /// MIDI Machine Control responses (Real Time `07`, RP-013 p.2). The
    /// header's device ID is the source, and always a device's own.
    MachineControlResponse(machine_control::MmcFields),
}

/// A Universal message and who its header names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Addressed {
    /// The recipient in the header.
    pub to: Recipient,
    /// The message.
    pub message: Universal,
}

/// A dump that failed its checksum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BadChecksum {
    /// The recipient in the header.
    pub to: Recipient,
    /// The checksum the bytes make.
    pub expected: U7,
    /// The checksum the message said.
    pub found: U7,
}

/// What [`read()`](fn@read) made of one SysEx message.
// Copy, and read on the audio thread: boxing the table or the changes would allocate there.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Read {
    /// A Universal message this crate reads.
    Universal(Addressed),
    /// A tuning dump whose checksum field is not the XOR of the bytes before it
    /// (MTS, Checksum Calculation). The spec says nothing of what a receiver
    /// does with one; an instrument does not apply it ([`UniversalReader`]
    /// counts it, [`corrupt`](UniversalReader::corrupt)).
    BadChecksum(BadChecksum),
    /// Universal (`7E` or `7F`), but not one this crate reads, or one of them
    /// that does not parse (cut short, or with bytes after its end).
    Unread,
    /// Not Universal: a manufacturer's System Exclusive.
    Foreign,
}

/// Reads one whole System Exclusive message.
///
/// ```
/// use tutti_midi_protocol::{Read, Universal, read};
/// use tutti_ump::SysexMessage;
///
/// // Master Volume, all-call, 0x3FFF (lsb first).
/// let bytes = [0xF0, 0x7F, 0x7F, 0x04, 0x01, 0x7F, 0x7F, 0xF7];
/// let Read::Universal(heard) = read(SysexMessage::from_framed(&bytes).unwrap()) else { panic!() };
/// assert_eq!(heard.to, tutti_midi_protocol::Recipient::AllCall);
/// assert!(matches!(heard.message, Universal::MasterVolume(v) if v.get() == 0x3FFF));
/// ```
#[must_use]
pub fn read(message: SysexMessage<'_>) -> Read {
    let payload = message.payload();
    match payload.first() {
        Some(&(read::NON_REAL_TIME | read::REAL_TIME)) => match read::parse(payload) {
            read::Parsed::Message(to, message) => Read::Universal(Addressed { to, message }),
            read::Parsed::BadChecksum {
                to,
                expected,
                found,
            } => Read::BadChecksum(BadChecksum {
                to,
                expected,
                found,
            }),
            read::Parsed::Unread => Read::Unread,
        },
        _ => Read::Foreign,
    }
}

/// Who a message was addressed to, as the receiver sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Addressee {
    /// The all-call (`7F`).
    All,
    /// The receiver's own id.
    Own,
}

/// A Universal message a receiver heard, and who it was addressed to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Heard {
    /// All devices, or this one.
    pub to: Addressee,
    /// The message.
    pub message: Universal,
}

/// The longest message the reader takes in, `F0` and `F7` included: a Single
/// Note Tuning Change of 127 changes with a bank is 517 bytes, a Key-Based
/// Tuning Dump 409.
const MAX_MESSAGE: usize = 528;

/// An instrument's edge for Universal System Exclusive: one Group's UMPs in,
/// the messages addressed to the instrument out.
///
/// It owns its reassembly buffer, sized once, and allocates nothing after
/// [`new`](Self::new). A message longer than it reads is dropped whole and
/// counted ([`dropped`](Self::dropped)), as `SysexAssembler` does.
#[derive(Debug)]
pub struct UniversalReader {
    group: MidiGroup,
    device: SysexDeviceId,
    assembler: SysexAssembler<Box<[u8]>>,
    corrupt: u64,
}

impl UniversalReader {
    /// A reader of `group`'s System Exclusive for the device `device`.
    #[must_use]
    pub fn new(group: MidiGroup, device: SysexDeviceId) -> Self {
        let room = SysexAssembler::room_for(MAX_MESSAGE, 1, 1);
        Self {
            group,
            device,
            assembler: SysexAssembler::new(vec![0; room].into_boxed_slice(), MAX_MESSAGE, 1),
            corrupt: 0,
        }
    }

    /// The device id the reader answers to besides the all-call.
    #[must_use]
    pub fn device(&self) -> SysexDeviceId {
        self.device
    }

    /// Takes in one UMP. A packet of another Group is not this reader's and is
    /// skipped; every UMP of its own Group is taken, because a message of the
    /// Group that is not a Continue ends an open SysEx (M2-104 section 7.7.1).
    /// Returns the Universal message that UMP completed, if it was addressed to
    /// this device or to all.
    pub fn push(&mut self, ump: Ump) -> Option<Heard> {
        if ump.group().is_some_and(|g| g != self.group) {
            return None;
        }
        self.assembler.begin_block();
        let SysexStep::Complete(done) = self.assembler.push(ump) else {
            return None;
        };
        let (recipient, message) = match read(self.assembler.message(done)) {
            Read::Universal(Addressed { to, message }) => (to, message),
            Read::BadChecksum(bad) => {
                if self.is_for_us(bad.to).is_some() {
                    self.corrupt += 1;
                }
                return None;
            }
            _ => return None,
        };
        // A response is a controller's to read, from a device that is not this one.
        if matches!(message, Universal::MachineControlResponse(_)) {
            return None;
        }
        let to = self.is_for_us(recipient)?;
        Some(Heard { to, message })
    }

    /// Who a header names, if it names this reader's device.
    fn is_for_us(&self, recipient: Recipient) -> Option<Addressee> {
        match recipient {
            Recipient::AllCall => Some(Addressee::All),
            Recipient::Device(device) if device == self.device => Some(Addressee::Own),
            Recipient::Device(_) => None,
        }
    }

    /// Messages dropped whole: too long, cut by another message of the Group,
    /// or open at a [`reset`](Self::reset).
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.assembler.dropped()
    }

    /// Dumps addressed to this device that failed their checksum and were not
    /// passed on ([`Read::BadChecksum`]).
    #[must_use]
    pub fn corrupt(&self) -> u64 {
        self.corrupt
    }

    /// Forgets an open message.
    pub fn reset(&mut self) {
        self.assembler.reset();
    }
}

impl Clone for UniversalReader {
    /// A reader of the same Group and device with nothing open: a copy of an
    /// instrument starts between messages.
    fn clone(&self) -> Self {
        Self::new(self.group, self.device)
    }
}
