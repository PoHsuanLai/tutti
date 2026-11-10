//! System Exclusive: a checked complete message, and the one reassembler of
//! SysEx7 packets into such messages.

use core::iter::FusedIterator;

use crate::{Message, MidiGroup, Sysex7Packet, SysexStatus, U7, Ump, decode};

/// The most bytes one SysEx7 packet adds to a buffer: six payload bytes and
/// both delimiters (a `Complete`).
const BYTES_PER_PACKET: usize = 8;

/// A complete MIDI 1.0 System Exclusive message, checked once: it starts with
/// `0xF0`, ends with `0xF7`, and every byte between is 7-bit.
///
/// It borrows its bytes. A host that sends one builds it at the edge, and the
/// check is the type's, so no function below it accepts "a message with or
/// without its delimiters".
///
/// ```
/// use tutti_ump::SysexMessage;
///
/// let message = SysexMessage::new(&[0xF0, 0x7E, 0x7F, 0xF7]).unwrap();
/// assert_eq!(message.payload(), [0x7E, 0x7F]);
/// assert!(SysexMessage::new(&[0x7E, 0x7F]).is_none());
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SysexMessage<'a>(&'a [u8]);

impl<'a> SysexMessage<'a> {
    /// The message, or `None` unless `bytes` is `0xF0`, any number of 7-bit
    /// bytes, then `0xF7` (at least the two delimiters).
    #[must_use]
    pub fn new(bytes: &'a [u8]) -> Option<Self> {
        let [0xF0, interior @ .., 0xF7] = bytes else {
            return None;
        };
        interior.iter().all(|&b| b < 0x80).then_some(Self(bytes))
    }

    /// The whole message, delimiters included.
    #[must_use]
    pub const fn bytes(self) -> &'a [u8] {
        self.0
    }

    /// The message without its `0xF0` and `0xF7`: the payload SysEx7 carries.
    #[must_use]
    pub fn payload(self) -> &'a [u8] {
        self.0.split_at(1).1.split_at(self.0.len() - 2).0
    }

    /// The message as SysEx7 packets on `group` (M2-104 section 7.7): see
    /// [`sysex7_packets`].
    #[must_use]
    pub fn packets(self, group: MidiGroup) -> Sysex7Packets<'a> {
        // Checked in `new`: every byte between the delimiters is below 128.
        Sysex7Packets::over(group, self.payload())
    }
}

/// The SysEx7 packets (M2-104 section 7.7) of one payload: the bytes between
/// `0xF0` and `0xF7`, without them.
///
/// A payload of up to six bytes is one `Complete` packet (an empty one is a
/// `Complete` with no bytes). A longer one is a `Start`, any number of
/// `Continue`, then an `End`, each carrying six bytes except the last, which
/// carries one to six. Nothing is allocated, and the packets carry no timing:
/// the caller pairs each with an offset. [`SysexAssembler`] is the inverse.
///
/// ```
/// use tutti_ump::{MidiGroup, SysexStatus, U7, decode, Message, sysex7_packets};
///
/// let payload = [U7::of(1); 7];
/// let mut packets = sysex7_packets(MidiGroup::of(0), &payload);
/// assert_eq!(packets.len(), 2);
/// let Ok(Message::Sysex7(_, first)) = decode(packets.next().unwrap()) else { unreachable!() };
/// assert_eq!(first.status(), SysexStatus::Start);
/// ```
#[must_use]
pub fn sysex7_packets(group: MidiGroup, payload: &[U7]) -> Sysex7Packets<'_> {
    Sysex7Packets::over(group, bytemuck::cast_slice(payload))
}

/// The packets of one SysEx payload; see [`sysex7_packets`].
#[derive(Clone, Debug)]
pub struct Sysex7Packets<'a> {
    group: MidiGroup,
    /// The payload not yet packed: every byte below 128, checked by the caller's
    /// type (`U7`, `SysexMessage`).
    rest: &'a [u8],
    /// Whether a packet has been yielded, so an empty payload is one packet.
    started: bool,
}

impl<'a> Sysex7Packets<'a> {
    /// The packets of `payload` (the bytes between `0xF0` and `0xF7`, without
    /// them) from plain bytes, or `None` if a byte is 128 or more: the entry
    /// for a payload that arrives unchecked, such as a plugin's. Nothing is
    /// masked and nothing is allocated; a message with its delimiters is a
    /// [`SysexMessage`].
    #[must_use]
    pub fn checked(group: MidiGroup, payload: &'a [u8]) -> Option<Self> {
        payload
            .iter()
            .all(|&b| b < 0x80)
            .then(|| Self::over(group, payload))
    }

    fn over(group: MidiGroup, payload: &'a [u8]) -> Self {
        Self {
            group,
            rest: payload,
            started: false,
        }
    }
}

impl Iterator for Sysex7Packets<'_> {
    type Item = Ump;

    fn next(&mut self) -> Option<Ump> {
        if self.started && self.rest.is_empty() {
            return None;
        }
        let status = match (self.started, self.rest.len() > 6) {
            (false, false) => SysexStatus::Complete,
            (false, true) => SysexStatus::Start,
            (true, true) => SysexStatus::Continue,
            (true, false) => SysexStatus::End,
        };
        let (chunk, rest) = self.rest.split_at(self.rest.len().min(6));
        self.rest = rest;
        self.started = true;
        let mut data = [0u8; 6];
        data[..chunk.len()].copy_from_slice(chunk);
        let packet = Sysex7Packet::new(status, &data[..chunk.len()])
            .expect("at most six bytes, each below 128");
        Some(Ump::from(Message::Sysex7(self.group, packet)))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = match (self.started, self.rest.len()) {
            (true, 0) => 0,
            (_, len) => len.div_ceil(6).max(1),
        };
        (n, Some(n))
    }
}

impl ExactSizeIterator for Sysex7Packets<'_> {}
impl FusedIterator for Sysex7Packets<'_> {}

/// The slice [`SysexMessage::new`] refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotSysex<'a>(pub &'a [u8]);

impl core::fmt::Display for NotSysex<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{} bytes are not one SysEx message (0xF0, 7-bit bytes, 0xF7)",
            self.0.len()
        )
    }
}

impl core::error::Error for NotSysex<'_> {}

impl<'a> TryFrom<&'a [u8]> for SysexMessage<'a> {
    type Error = NotSysex<'a>;

    fn try_from(bytes: &'a [u8]) -> Result<Self, NotSysex<'a>> {
        Self::new(bytes).ok_or(NotSysex(bytes))
    }
}

impl<'a> From<SysexMessage<'a>> for &'a [u8] {
    fn from(message: SysexMessage<'a>) -> Self {
        message.0
    }
}

/// Where one complete message lies in its [`SysexAssembler`]'s buffer.
///
/// Valid until the assembler's next [`begin_block`](SysexAssembler::begin_block),
/// and only for the assembler that returned it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SysexRef {
    start: usize,
    len: usize,
}

impl SysexRef {
    /// The message at `start..start + len` of a buffer the caller owns: a
    /// host that copies a SysEx into its own byte arena names it with the
    /// type [`SysexStep::Complete`] hands out.
    ///
    /// The range is the caller's to keep valid. [`SysexAssembler::message`]
    /// reads only refs that assembler made itself, so a ref built here is
    /// read back from the caller's own buffer through [`range`](Self::range).
    #[must_use]
    pub const fn new(start: usize, len: usize) -> Self {
        Self { start, len }
    }

    /// The message's byte range in [`SysexAssembler::bytes`].
    #[must_use]
    pub const fn range(self) -> core::ops::Range<usize> {
        self.start..self.start + self.len
    }

    /// The message's length in bytes, delimiters included.
    #[must_use]
    pub const fn len(self) -> usize {
        self.len
    }

    /// Always false: a complete message holds at least its two delimiters.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.len == 0
    }
}

/// What one packet did to a [`SysexAssembler`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SysexStep {
    /// Not a SysEx7 packet: the caller converts it as usual.
    NotSysex,
    /// A packet taken in with no message to hand on yet: the start or middle
    /// of a message, a packet of one that is being dropped, or one that is not
    /// a well-formed SysEx7 packet.
    Held,
    /// A message completed: its bytes are in the buffer at this place.
    Complete(SysexRef),
}

/// The multi-packet message being assembled, if any.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Run {
    Idle,
    /// Its bytes so far, `0xF0` first, are `buf[start..len]`.
    Assembling {
        start: usize,
        group: MidiGroup,
    },
    /// Over capacity and already counted: its remaining packets are skipped
    /// up to its `END`.
    Discarding {
        group: MidiGroup,
    },
}

/// Reassembles SysEx7 packets (M2-104 §7.7.1) into complete `0xF0 .. 0xF7`
/// messages, inside a buffer the caller provides and that is never grown.
///
/// A consumer that wants whole bytes (a plugin format, which reads one pointer
/// for the whole `process` call; MIDI-CI, which parses a message) feeds each
/// packet to [`push`](Self::push) and reads a completed message back as a
/// [`SysexMessage`]. The assembler allocates nothing: it owns the buffer `B`
/// (a `Vec<u8>` or boxed slice sized off the audio thread, an array) and
/// uses its length as its room, so a message is never moved by a later
/// allocation and a pointer into [`bytes`](Self::bytes) is stable until the
/// next [`begin_block`](Self::begin_block). Size it with `SysexAssembler::room_for`.
///
/// # Policy
///
/// - **A message spans blocks.** A run left open at the end of a block (of
///   packets: a device callback, an audio block) continues in the next, so a
///   dump longer than one block's packets arrives whole.
/// - **One multi-packet message at a time, across all groups.** A `START`
///   while another is still open, on any group, drops the open one, whole,
///   and counts it: it never got its `END`. This deviates on purpose from
///   M2-104's independent streams per group: a consumer of whole messages (a
///   plugin format has no SysEx group to deliver one on; a MIDI-CI endpoint
///   answers on one) could only receive two interleaved dumps as two
///   messages anyway, and one open run keeps the buffer one message long.
///   A `Complete` packet on another group completes beside an open run; on the
///   run's own group it ends the run (dropped and counted), as M2-104 §7.7.1
///   has it.
/// - **A gap upstream drops the open message, counted.** A queue between the
///   source and this assembler that overflows truncates its block's tail; its
///   owner calls [`lost_tail`](Self::lost_tail) after pushing the kept
///   packets, so the packets of the next block are never spliced onto a run
///   that lost some in between.
/// - **Other messages do not end a run.** A merged event list carries a note
///   played during a dump between its packets; ending the dump there would
///   cost the dump for no gain.
/// - **Over capacity, the whole message is dropped and counted**, never
///   truncated: its remaining packets are skipped up to its `END`. The
///   capacity is the `max_message` given to [`new`](Self::new) and the
///   buffer's room.
/// - A `CONTINUE` or `END` with no open run on its group (its `START` was
///   dropped, or arrived before this assembler existed) is skipped.
///
/// Each dropped message is counted once in [`dropped`](Self::dropped).
/// [`reset`](Self::reset) drops the open message too.
///
/// # Memory
///
/// The buffer holds the block's completed messages and, after them, the open
/// run. The open run carried into a block is shorter than one message and each
/// packet adds at most eight bytes, so a buffer of
/// `SysexAssembler::room_for(max_message, packets_per_block)` never runs
/// out of room for a block of up to that many packets.
#[derive(Debug)]
pub struct SysexAssembler<B> {
    /// Completed messages of this block, then the open run: `buf[..len]`.
    buf: B,
    len: usize,
    max_message: usize,
    run: Run,
    dropped: u64,
}

impl SysexAssembler<()> {
    /// The buffer length that holds a message of `max_message` bytes
    /// (delimiters included) and a block of `packets_per_block` packets after
    /// the open run it carries in: `max_message` plus eight bytes a packet.
    ///
    /// It is on `SysexAssembler<()>` so that a caller sizing a buffer names no
    /// buffer type: `SysexAssembler::room_for(max, packets)`.
    #[must_use]
    pub const fn room_for(max_message: usize, packets_per_block: usize) -> usize {
        max_message + packets_per_block * BYTES_PER_PACKET
    }
}

impl<B: AsRef<[u8]> + AsMut<[u8]>> SysexAssembler<B> {
    /// An assembler over `buf` for messages of up to `max_message` bytes,
    /// delimiters included. Its room is `buf`'s length; an empty buffer drops
    /// (and counts) every message.
    #[must_use]
    pub fn new(buf: B, max_message: usize) -> Self {
        Self {
            buf,
            len: 0,
            max_message,
            run: Run::Idle,
            dropped: 0,
        }
    }

    /// The longest message this assembler hands on, in bytes.
    #[must_use]
    pub fn max_message(&self) -> usize {
        self.max_message
    }

    /// Start a block: forget the last block's completed messages, keeping an
    /// open run. Call it before pushing a block's packets, after the consumer
    /// has stopped reading the last block's. Never allocates.
    pub fn begin_block(&mut self) {
        match self.run {
            Run::Assembling { start, group } => {
                self.buf.as_mut().copy_within(start..self.len, 0);
                self.len -= start;
                self.run = Run::Assembling { start: 0, group };
            }
            Run::Idle | Run::Discarding { .. } => self.len = 0,
        }
    }

    /// Take in one packet. Never allocates.
    ///
    /// A malformed SysEx7 packet (a length past six, a data byte of 128 or
    /// more) is [`SysexStep::Held`] and has no effect.
    pub fn push(&mut self, packet: Ump) -> SysexStep {
        let (group, packet) = match decode(packet) {
            Ok(Message::Sysex7(group, packet)) => (group, packet),
            Ok(_) => return SysexStep::NotSysex,
            Err(_) if packet.message_type() == 0x3 => return SysexStep::Held,
            Err(_) => return SysexStep::NotSysex,
        };
        let payload = packet.data();
        match packet.status() {
            SysexStatus::Complete => {
                if self.run_group() == Some(group) {
                    self.abandon();
                }
                self.single(payload)
            }
            SysexStatus::Start => {
                if matches!(self.run, Run::Assembling { .. }) {
                    self.abandon();
                }
                self.run = Run::Assembling {
                    start: self.len,
                    group,
                };
                self.append(&[0xF0], payload, &[]);
                SysexStep::Held
            }
            SysexStatus::Continue => {
                if self.assembling(group) {
                    self.append(&[], payload, &[]);
                }
                SysexStep::Held
            }
            SysexStatus::End => match self.run {
                Run::Assembling { start, group: g } if g == group => {
                    if !self.append(&[], payload, &[0xF7]) {
                        return SysexStep::Held;
                    }
                    self.run = Run::Idle;
                    SysexStep::Complete(SysexRef {
                        start,
                        len: self.len - start,
                    })
                }
                Run::Discarding { group: g } if g == group => {
                    self.run = Run::Idle;
                    SysexStep::Held
                }
                _ => SysexStep::Held,
            },
        }
    }

    /// An upstream queue dropped packets after the ones just pushed (it
    /// overflowed): drop the open message, counted, so packets of a later
    /// block are not spliced onto it. Call it after pushing a block's kept
    /// packets. Never allocates.
    pub fn lost_tail(&mut self) {
        self.abandon();
    }

    /// The consumer was reset: drop the open message (counted, like any
    /// message that never reaches it) and the block's completed ones. Never
    /// allocates.
    pub fn reset(&mut self) {
        self.abandon();
        self.len = 0;
    }

    /// The bytes every [`SysexRef`] of this block indexes. Its address is
    /// stable until the next [`begin_block`](Self::begin_block).
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.buf.as_ref()[..self.len]
    }

    /// A completed message.
    ///
    /// # Panics
    ///
    /// If `message` is not one this assembler returned this block.
    #[must_use]
    pub fn message(&self, message: SysexRef) -> SysexMessage<'_> {
        SysexMessage::new(&self.bytes()[message.range()])
            .expect("a completed message is F0, 7-bit bytes, F7")
    }

    /// Messages dropped whole since this assembler was made (a
    /// [`reset`](Self::reset) does not zero it): longer than
    /// [`max_message`](Self::max_message), left without their `END` by a
    /// later `START` (or a `Complete` on their group), cut by an upstream
    /// overflow ([`lost_tail`](Self::lost_tail)), open at a reset, or one
    /// whose packet found the buffer's room full (more packets in one block
    /// than it was sized for).
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    fn room(&self) -> usize {
        self.buf.as_ref().len()
    }

    fn run_group(&self) -> Option<MidiGroup> {
        match self.run {
            Run::Assembling { group, .. } | Run::Discarding { group } => Some(group),
            Run::Idle => None,
        }
    }

    fn assembling(&self, group: MidiGroup) -> bool {
        matches!(self.run, Run::Assembling { group: g, .. } if g == group)
    }

    /// Drop the open run, counted.
    fn abandon(&mut self) {
        if let Run::Assembling { start, .. } = self.run {
            self.len = start;
            self.dropped += 1;
        }
        self.run = Run::Idle;
    }

    /// Copy `bytes` after the bytes in use. The caller has checked the room.
    fn put(&mut self, bytes: &[u8]) {
        let end = self.len + bytes.len();
        self.buf.as_mut()[self.len..end].copy_from_slice(bytes);
        self.len = end;
    }

    /// Append `head`, `payload` and `tail` to the open run, or, past the
    /// message's capacity or the buffer's room, drop the run whole (counted)
    /// and skip the rest of it. Returns whether they were appended.
    fn append(&mut self, head: &[u8], payload: &[u8], tail: &[u8]) -> bool {
        let Run::Assembling { start, group } = self.run else {
            return false;
        };
        let added = head.len() + payload.len() + tail.len();
        // An unterminated run still needs its `0xF7`.
        let closing = usize::from(tail.is_empty());
        let message = self.len - start + added + closing;
        if message > self.max_message || self.len + added > self.room() {
            self.len = start;
            self.dropped += 1;
            self.run = Run::Discarding { group };
            return false;
        }
        self.put(head);
        self.put(payload);
        self.put(tail);
        true
    }

    /// Complete a one-packet message, placed before the open run (if any) so
    /// the run stays last and can keep growing.
    fn single(&mut self, payload: &[u8]) -> SysexStep {
        let len = payload.len() + 2;
        if len > self.max_message || self.len + len > self.room() {
            self.dropped += 1;
            return SysexStep::Held;
        }
        let at = match self.run {
            Run::Assembling { start, .. } => start,
            Run::Idle | Run::Discarding { .. } => self.len,
        };
        self.put(&[0xF0]);
        self.put(payload);
        self.put(&[0xF7]);
        // Rotate the message in front of the open run. The run has no
        // pointer handed out yet, so moving it is free of hazard.
        let len_now = self.len;
        self.buf.as_mut()[at..len_now].rotate_right(len);
        if let Run::Assembling { start, group } = self.run {
            self.run = Run::Assembling {
                start: start + len,
                group,
            };
        }
        SysexStep::Complete(SysexRef { start: at, len })
    }
}
