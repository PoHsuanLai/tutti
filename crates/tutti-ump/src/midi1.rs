//! MIDI 1.0 on the wire: the bytes of one message, SysEx framing, and the
//! reader that turns a byte stream into [`Message`]s.
//!
//! Every status byte and bit position of the MIDI 1.0 wire lives here.
//! [`Voice1::bytes`] and [`System::bytes`] write it; [`Midi1Parser`] reads it
//! one byte at a time, with bounded state and no allocation.

use core::iter::FusedIterator;
use core::slice;

use crate::field::{U7, U14};
use crate::message::{Address, Bank, Message, Sysex7Packet, SysexStatus, System, Voice1};
use crate::{CCNumber, MidiChannel, MidiGroup, NoteNumber};

// ---- bytes out ---------------------------------------------------------------

/// The wire bytes of one MIDI 1.0 message: a status byte and up to two data
/// bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Midi1Bytes {
    bytes: [u8; 3],
    len: u8,
}

impl Midi1Bytes {
    const fn one(status: u8) -> Self {
        Self {
            bytes: [status, 0, 0],
            len: 1,
        }
    }

    const fn two(status: u8, d1: u8) -> Self {
        Self {
            bytes: [status, d1, 0],
            len: 2,
        }
    }

    const fn three(status: u8, d1: u8, d2: u8) -> Self {
        Self {
            bytes: [status, d1, d2],
            len: 3,
        }
    }

    /// The message's bytes, 1 to 3 of them.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        self.bytes.split_at(usize::from(self.len)).0
    }

    /// The bytes with zeros after the last data byte: the form of a host event
    /// with a fixed three-byte field.
    #[must_use]
    pub const fn padded(self) -> [u8; 3] {
        self.bytes
    }

    /// How many bytes the message has, 1 to 3.
    #[must_use]
    #[allow(
        clippy::len_without_is_empty,
        reason = "a message has at least its status byte"
    )]
    pub const fn len(self) -> usize {
        self.len as usize
    }
}

impl U14 {
    /// The upper seven bits: the second data byte of a pitch bend or a song
    /// position.
    #[must_use]
    pub const fn msb(self) -> U7 {
        U7::wire((self.get() >> 7) as u8)
    }

    /// The lower seven bits: the first data byte.
    #[must_use]
    pub const fn lsb(self) -> U7 {
        U7::wire((self.get() & 0x7F) as u8)
    }
}

impl Voice1 {
    /// The message on `channel` as MIDI 1.0 bytes.
    #[must_use]
    pub fn bytes(self, channel: MidiChannel) -> Midi1Bytes {
        let c = channel.get();
        match self {
            Self::NoteOff { note, velocity } => {
                Midi1Bytes::three(0x80 | c, note.get(), velocity.get())
            }
            Self::NoteOn { note, velocity } => {
                Midi1Bytes::three(0x90 | c, note.get(), velocity.get())
            }
            Self::PolyPressure { note, pressure } => {
                Midi1Bytes::three(0xA0 | c, note.get(), pressure.get())
            }
            Self::ControlChange { controller, value } => {
                Midi1Bytes::three(0xB0 | c, controller.get(), value.get())
            }
            Self::ProgramChange(program) => Midi1Bytes::two(0xC0 | c, program.get()),
            Self::ChannelPressure(pressure) => Midi1Bytes::two(0xD0 | c, pressure.get()),
            Self::PitchBend(bend) => {
                Midi1Bytes::three(0xE0 | c, bend.lsb().get(), bend.msb().get())
            }
        }
    }
}

impl System {
    /// The message as MIDI 1.0 bytes.
    #[must_use]
    pub fn bytes(self) -> Midi1Bytes {
        match self {
            Self::TimeCode(data) => Midi1Bytes::two(0xF1, data.get()),
            Self::SongPosition(p) => Midi1Bytes::three(0xF2, p.lsb().get(), p.msb().get()),
            Self::SongSelect(song) => Midi1Bytes::two(0xF3, song.get()),
            Self::TuneRequest => Midi1Bytes::one(0xF6),
            Self::TimingClock => Midi1Bytes::one(0xF8),
            Self::Start => Midi1Bytes::one(0xFA),
            Self::Continue => Midi1Bytes::one(0xFB),
            Self::Stop => Midi1Bytes::one(0xFC),
            Self::ActiveSensing => Midi1Bytes::one(0xFE),
            Self::Reset => Midi1Bytes::one(0xFF),
        }
    }
}

impl Bank {
    /// The two Control Changes that select the bank before a Program Change
    /// (M2-104 Appendix D): CC 0 carries the MSB, CC 32 the LSB.
    #[must_use]
    pub fn bytes(self, channel: MidiChannel) -> [Midi1Bytes; 2] {
        let c = 0xB0 | channel.get();
        [
            Midi1Bytes::three(c, 0, self.msb.get()),
            Midi1Bytes::three(c, 32, self.lsb.get()),
        ]
    }
}

// ---- SysEx framing -------------------------------------------------------------

/// Which byte of a framed message comes next.
#[derive(Clone, Copy, Debug)]
enum Frame {
    Start,
    Body,
    End,
    Done,
}

/// A SysEx payload with its `0xF0` and `0xF7`, as the bytes on the wire. It
/// knows its length.
#[derive(Clone, Debug)]
pub struct Framed<'a, B> {
    payload: slice::Iter<'a, B>,
    next: Frame,
}

/// `payload` between `0xF0` and `0xF7`.
#[must_use]
pub fn frame<B: Copy + Into<u8>>(payload: &[B]) -> Framed<'_, B> {
    Framed {
        payload: payload.iter(),
        next: Frame::Start,
    }
}

impl<B: Copy + Into<u8>> Iterator for Framed<'_, B> {
    type Item = u8;

    fn next(&mut self) -> Option<u8> {
        loop {
            match self.next {
                Frame::Start => {
                    self.next = Frame::Body;
                    return Some(0xF0);
                }
                Frame::Body => match self.payload.next() {
                    Some(&byte) => return Some(byte.into()),
                    None => self.next = Frame::End,
                },
                Frame::End => {
                    self.next = Frame::Done;
                    return Some(0xF7);
                }
                Frame::Done => return None,
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let delimiters = match self.next {
            Frame::Start => 2,
            Frame::Body | Frame::End => 1,
            Frame::Done => 0,
        };
        let n = delimiters + self.payload.len();
        (n, Some(n))
    }
}

impl<B: Copy + Into<u8>> ExactSizeIterator for Framed<'_, B> {}
impl<B: Copy + Into<u8>> FusedIterator for Framed<'_, B> {}

impl Sysex7Packet {
    /// The whole message with its delimiters, for a packet that is one
    /// (`Complete`); `None` for a packet that is a part of a longer one.
    #[must_use]
    pub fn framed(&self) -> Option<Framed<'_, U7>> {
        (self.status() == SysexStatus::Complete).then(|| frame(self.bytes()))
    }
}

// ---- bytes in ------------------------------------------------------------------

/// The SysEx payload not yet sent: up to one packet's worth, held until a
/// seventh byte or the `0xF7` says whether it is the first, a middle or the
/// last packet.
#[derive(Clone, Copy, Debug)]
struct Run {
    bytes: [u8; 6],
    len: u8,
    /// A `Start` packet has gone out.
    started: bool,
}

impl Run {
    const EMPTY: Self = Self {
        bytes: [0; 6],
        len: 0,
        started: false,
    };

    /// Takes a payload byte; the packet it pushes out when the run is full.
    fn push(&mut self, byte: u8) -> Option<Sysex7Packet> {
        let full = (self.len == 6).then(|| {
            let status = if self.started {
                SysexStatus::Continue
            } else {
                SysexStatus::Start
            };
            self.started = true;
            self.len = 0;
            Sysex7Packet::new(status, &self.bytes)
        });
        self.bytes[usize::from(self.len)] = byte;
        self.len += 1;
        full.flatten()
    }

    /// The last packet, at the `0xF7`.
    fn finish(self) -> Option<Sysex7Packet> {
        let status = if self.started {
            SysexStatus::End
        } else {
            SysexStatus::Complete
        };
        Sysex7Packet::new(status, &self.bytes[..usize::from(self.len)])
    }
}

/// What the parser is reading.
#[derive(Clone, Copy, Debug)]
enum State {
    /// Between messages, with no running status.
    Idle,
    /// A channel or system common message: its status and the data bytes read
    /// so far. A channel message stays open once complete, which is running
    /// status.
    Short {
        status: u8,
        data: [u8; 2],
        have: u8,
    },
    Sysex(Run),
}

/// Reads MIDI 1.0 bytes, one at a time, into [`Message`]s.
///
/// - **Running status** survives data and real-time bytes and ends at any other
///   status byte, so a system common message, `0xF0` and `0xF7` cancel it
///   (MIDI 1.0 detailed specification, table of running status rules).
/// - **Real-time bytes** (`0xF8..=0xFF`) come out at once, wherever they sit,
///   and touch nothing else: not a message half read, not a SysEx in flight.
/// - **SysEx** comes out as [`Sysex7Packet`]s as its bytes arrive: `Start`
///   once a seventh byte shows the message is longer than a packet, `Continue`
///   for each six after, and `End` (or `Complete` for a short message) at the
///   `0xF7`. A status byte other than a real-time one or `0xF7` ends the
///   message with no `End`: M2-104 section 7.7.1 says the next message on the
///   group terminates it, and the receiver of the packets drops the open one.
/// - **Stray bytes** (a data byte with no status, an undefined status, a lone
///   `0xF7`) produce nothing.
///
/// A Note On at velocity 0 is a Note On here; reading it as a Note Off is
/// translation's.
#[derive(Clone, Debug)]
pub struct Midi1Parser {
    group: MidiGroup,
    state: State,
}

impl Midi1Parser {
    /// A parser whose messages are on `group`. MIDI 1.0 has no group: a stream
    /// is one cable, so the group is a property of the port it came in on.
    #[must_use]
    pub const fn new(group: MidiGroup) -> Self {
        Self {
            group,
            state: State::Idle,
        }
    }

    /// The one message at the start of `bytes`: the status byte says how many
    /// bytes count, and any after them (the padding of a fixed three-byte
    /// field, as a CLAP, VST2 or VST3 MIDI event has) are ignored. `None` for a
    /// SysEx or undefined status, a message cut short, or a data byte with its
    /// top bit set.
    ///
    /// A padded field fed to [`feed`](Self::feed) would be read as running
    /// status data.
    #[must_use]
    pub fn message(group: MidiGroup, bytes: &[u8]) -> Option<Message> {
        let used = match *bytes.first()? {
            0xC0..=0xDF | 0xF1 | 0xF3 => 2,
            0x80..=0xEF | 0xF2 => 3,
            0xF6 | 0xF8 | 0xFA..=0xFC | 0xFE | 0xFF => 1,
            _ => return None,
        };
        let bytes = bytes.get(..used)?;
        if bytes.iter().skip(1).any(|b| b & 0x80 != 0) {
            return None;
        }
        Self::new(group).feed_all(bytes).last()
    }

    /// Ends the message being read and the running status, but not a SysEx in
    /// flight: for a transport that delimits messages (a plugin's event, an AU
    /// packet), where a short message does not continue past its delimiter.
    pub fn end_of_message(&mut self) {
        if matches!(self.state, State::Short { .. }) {
            self.state = State::Idle;
        }
    }

    /// Reads one byte; the message it completes, if any.
    #[must_use]
    pub fn feed(&mut self, byte: u8) -> Option<Message> {
        match byte {
            0x00..=0x7F => self.data(byte),
            0xF8..=0xFF => self.real_time(byte),
            0xF7 => match core::mem::replace(&mut self.state, State::Idle) {
                State::Sysex(run) => run.finish().map(|p| Message::Sysex7(self.group, p)),
                State::Idle | State::Short { .. } => None,
            },
            _ => self.status(byte),
        }
    }

    /// The messages in `bytes`, in order.
    pub fn feed_all<'a>(&'a mut self, bytes: &'a [u8]) -> impl Iterator<Item = Message> + 'a {
        bytes.iter().filter_map(|&b| self.feed(b))
    }

    fn real_time(&self, byte: u8) -> Option<Message> {
        let system = match byte {
            0xF8 => System::TimingClock,
            0xFA => System::Start,
            0xFB => System::Continue,
            0xFC => System::Stop,
            0xFE => System::ActiveSensing,
            0xFF => System::Reset,
            _ => return None,
        };
        Some(Message::System(self.group, system))
    }

    /// A status byte other than `0xF7` and real-time: what was open is dropped.
    fn status(&mut self, byte: u8) -> Option<Message> {
        self.state = match byte {
            0xF0 => State::Sysex(Run::EMPTY),
            0xF4..=0xF6 => State::Idle,
            _ => State::Short {
                status: byte,
                data: [0; 2],
                have: 0,
            },
        };
        (byte == 0xF6).then(|| Message::System(self.group, System::TuneRequest))
    }

    fn data(&mut self, byte: u8) -> Option<Message> {
        let group = self.group;
        match &mut self.state {
            State::Idle => None,
            State::Sysex(run) => run.push(byte).map(|p| Message::Sysex7(group, p)),
            State::Short { status, data, have } => {
                data[usize::from(*have)] = byte;
                *have += 1;
                let status = *status;
                if *have < data_bytes(status) {
                    return None;
                }
                *have = 0;
                let [d1, d2] = *data;
                if status >= 0xF0 {
                    self.state = State::Idle;
                }
                short(group, status, d1, d2)
            }
        }
    }
}

/// How many data bytes follow a channel or system common status.
const fn data_bytes(status: u8) -> u8 {
    match status {
        0xC0..=0xDF | 0xF1 | 0xF3 => 1,
        _ => 2,
    }
}

/// The message of a status byte and its data bytes; `d2` is unused by a
/// one-byte message.
fn short(group: MidiGroup, status: u8, d1: u8, d2: u8) -> Option<Message> {
    let u7 = U7::new;
    let u14 = || U14::new(u16::from(d1) | u16::from(d2) << 7);
    if status >= 0xF0 {
        let system = match status {
            0xF1 => System::TimeCode(u7(d1)?),
            0xF2 => System::SongPosition(u14()?),
            _ => System::SongSelect(u7(d1)?),
        };
        return Some(Message::System(group, system));
    }
    let address = Address {
        group,
        channel: MidiChannel::masked(status),
    };
    let note = || NoteNumber::new(d1);
    let voice = match status >> 4 {
        0x8 => Voice1::NoteOff {
            note: note()?,
            velocity: u7(d2)?,
        },
        0x9 => Voice1::NoteOn {
            note: note()?,
            velocity: u7(d2)?,
        },
        0xA => Voice1::PolyPressure {
            note: note()?,
            pressure: u7(d2)?,
        },
        0xB => Voice1::ControlChange {
            controller: CCNumber::new(d1)?,
            value: u7(d2)?,
        },
        0xC => Voice1::ProgramChange(u7(d1)?),
        0xD => Voice1::ChannelPressure(u7(d1)?),
        _ => Voice1::PitchBend(u14()?),
    };
    Some(Message::Midi1(address, voice))
}

#[cfg(test)]
mod tests;
