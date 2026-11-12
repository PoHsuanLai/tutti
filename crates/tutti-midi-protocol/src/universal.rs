//! Universal System Exclusive messages (M1 pp.34 to 57): the ones an instrument
//! or a device inquiry reads, as typed values.
//!
//! [`read()`](fn@read) turns a whole SysEx message ([`SysexMessage`], which
//! `tutti_ump::SysexAssembler` hands out) into a [`Read`]. It is a pure parse of
//! one message: no state, no allocation. [`UniversalReader`] is the instrument's
//! edge: it takes every UMP of one Group, reassembles the SysEx7 packets, reads
//! the message and answers only the ones addressed to the instrument
//! ([`DeviceId`]).
//!
//! # What is read
//!
//! - Identity Request and Identity Reply (the Device Inquiry, M1 p.40; the
//!   reply is a [`DeviceIdentity`]).
//! - General MIDI System On and Off (M1 p.52, `09 01` and `09 02`).
//! - Master Volume and Master Balance (M1 p.57, `04 01` and `04 02`).
//! - Master Fine Tuning and Master Coarse Tuning (CA-025, `04 03` and `04 04`).
//! - The MIDI Tuning Standard (`08`): [`TuningMessage`], with its
//!   [frequency data](TuningFrequency). A dump whose checksum does not hold is
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
//! `7F` is the all-call: "if `<device ID>` = 7FH then the device should respond
//! regardless of what `<device ID>` it is set to" (M1 p.40). Any other id is for
//! the device that has it. A device with no id of its own to say is
//! [`DeviceId::DEFAULT`]; an instrument takes the messages addressed to it or to
//! all ([`Addressee`]). Master Volume and Balance keep the two apart, as M1
//! p.57 requires: "received on its own ID", "received on the 'All Call' ID".

mod read;

use crate::tuning::TuningMessage;

use tutti_ump::{DeviceIdentity, MidiGroup, SysexAssembler, SysexMessage, SysexStep, U7, U14, Ump};

/// A Universal System Exclusive device id: the 7-bit id of the device a
/// message is for, or the all-call (`7F`).
///
/// Its range is the whole of [`U7`], so it has no refusing constructor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DeviceId(U7);

impl DeviceId {
    /// The all-call, `7F`: every device answers it (M1 p.40).
    pub const ALL_CALL: Self = Self(U7::MAX);
    /// The id a device that has none set takes: `00`.
    pub const DEFAULT: Self = Self(U7::of(0));

    /// The id `id`.
    #[must_use]
    pub const fn new(id: U7) -> Self {
        Self(id)
    }

    /// The id as its 7-bit byte.
    #[must_use]
    pub const fn get(self) -> U7 {
        self.0
    }

    /// Whether this is the all-call.
    #[must_use]
    pub const fn is_all_call(self) -> bool {
        self.0.get() == U7::MAX.get()
    }
}

/// General MIDI System On or Off (M1 p.52).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GeneralMidi {
    /// `09 01`: the device goes to General MIDI System Level 1's initial status.
    On,
    /// `09 02`: a device with another initialized mode returns to it.
    Off,
}

/// A Universal message this crate reads. Fields are the wire's values, in the
/// type that holds them; what a value means (a curve, a displacement) is the
/// consumer's.
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
}

/// A Universal message and the device id it names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Addressed {
    /// The device id in the header.
    pub device: DeviceId,
    /// The message.
    pub message: Universal,
}

/// A dump that failed its checksum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BadChecksum {
    /// The device id in the header.
    pub device: DeviceId,
    /// The checksum the bytes make.
    pub expected: U7,
    /// The checksum the message said.
    pub found: U7,
}

/// What [`read()`](fn@read) made of one SysEx message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Read {
    /// A Universal message this crate reads.
    Universal(Addressed),
    /// A tuning dump whose checksum field is not the XOR of the bytes before it
    /// (MTS, "Checksum Calculation"). The spec says nothing of what a receiver
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
/// let Read::Universal(heard) = read(SysexMessage::new(&bytes).unwrap()) else { panic!() };
/// assert!(heard.device.is_all_call());
/// assert!(matches!(heard.message, Universal::MasterVolume(v) if v.get() == 0x3FFF));
/// ```
#[must_use]
pub fn read(message: SysexMessage<'_>) -> Read {
    let payload = message.payload();
    match payload.first() {
        Some(&(read::NON_REAL_TIME | read::REAL_TIME)) => match read::parse(payload) {
            read::Parsed::Message(device, message) => {
                Read::Universal(Addressed { device, message })
            }
            read::Parsed::BadChecksum {
                device,
                expected,
                found,
            } => Read::BadChecksum(BadChecksum {
                device,
                expected,
                found,
            }),
            read::Parsed::Unread => Read::Unread,
        },
        _ => Read::Foreign,
    }
}

/// Who a message was addressed to, as the receiver sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    device: DeviceId,
    assembler: SysexAssembler<Box<[u8]>>,
    corrupt: u64,
}

impl UniversalReader {
    /// A reader of `group`'s System Exclusive for the device `device`.
    #[must_use]
    pub fn new(group: MidiGroup, device: DeviceId) -> Self {
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
    pub fn device(&self) -> DeviceId {
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
        let Read::Universal(Addressed { device, message }) =
            (match read(self.assembler.message(done)) {
                Read::BadChecksum(bad) => {
                    if bad.device.is_all_call() || bad.device == self.device {
                        self.corrupt += 1;
                    }
                    return None;
                }
                other => other,
            })
        else {
            return None;
        };
        let to = if device.is_all_call() {
            Addressee::All
        } else if device == self.device {
            Addressee::Own
        } else {
            return None;
        };
        Some(Heard { to, message })
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
