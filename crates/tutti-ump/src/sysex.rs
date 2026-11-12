//! System Exclusive: a checked complete message, and the one reassembler of
//! SysEx7 packets into such messages.

use core::iter::FusedIterator;

use crate::{Message, MidiGroup, Sysex7Packet, SysexStatus, Ump};

/// The longest SysEx7 payload (the bytes between `0xF0` and `0xF7`) tutti
/// carries: 64 KiB. It is tutti's cap on one message, used to size a
/// [`SysexAssembler`] buffer, and **not a limit the spec sets**: M2-104 section
/// 7.7 lets a message span a Start, any number of Continue UMPs and an End,
/// and states no maximum.
///
/// A cap exists because the hazard is not a large dump but a **lost
/// terminator**: a device unplugged mid-dump, or a dropped buffer, leaves a
/// reassembler mid-message with no end ever arriving, and an uncapped buffer
/// then grows for the lifetime of the connection.
///
/// 64 KiB sits above real traffic and below anything alarming: the largest
/// thing MIDI-CI negotiates is a Property Exchange chunk, and CI's own
/// `max_sysex_size` is a 28-bit field that devices populate with values in the
/// hundreds-to-low-thousands of bytes. Synth bulk dumps (a 32-voice DX7 bank
/// is 4104 bytes) and sample-dump packets are far below it. A message past
/// this is a fault, not a big message. A path's event caps can be lower: a
/// plugin node takes at most 256 MIDI events a chunk, so a dump whose packets
/// arrive in one block reaches a plugin only up to 1536 payload bytes.
pub const SYSEX7_MAX_PAYLOAD: usize = 64 * 1024;

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
    /// [`Sysex7Packets`]. The one way to packetise a checked message; a
    /// payload of unchecked bytes goes through [`Sysex7Packets::checked`].
    #[must_use]
    pub fn packets(self, group: MidiGroup) -> Sysex7Packets<'a> {
        // Checked in `new`: every byte between the delimiters is below 128.
        Sysex7Packets::over(group, self.payload())
    }
}

/// The SysEx7 packets (M2-104 section 7.7) of one payload: the bytes between
/// `0xF0` and `0xF7`, without them. Made by [`SysexMessage::packets`] for a
/// checked message, or by [`Sysex7Packets::checked`] for a payload of plain
/// bytes that arrives unchecked.
///
/// A payload of up to six bytes is one `Complete` packet (an empty one is a
/// `Complete` with no bytes). A longer one is a `Start`, any number of
/// `Continue`, then an `End`, each carrying six bytes except the last, which
/// carries one to six. Nothing is allocated, and the packets carry no timing:
/// the caller pairs each with an offset. [`SysexAssembler`] is the inverse.
///
/// ```
/// use tutti_ump::{MidiGroup, SysexStatus, Sysex7Packets, Message};
///
/// let mut packets = Sysex7Packets::checked(MidiGroup::of(0), &[1; 7]).unwrap();
/// assert_eq!(packets.len(), 2);
/// let Ok(Message::Sysex7(_, first)) = packets.next().unwrap().decode() else { unreachable!() };
/// assert_eq!(first.status(), SysexStatus::Start);
/// ```
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

/// The most Groups a UMP stream has (M2-104 §2.1.2).
const GROUPS: usize = 16;

/// What one Group's SysEx7 stream is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Run {
    Idle,
    /// A multi-packet message in lane `lane`: its bytes so far, `0xF0` first,
    /// are the first `len` bytes of the lane.
    Assembling {
        lane: usize,
        len: usize,
    },
    /// Dropped and already counted: its remaining packets are skipped up to
    /// its `End`.
    Discarding,
}

/// Reassembles SysEx7 packets (M2-104 §7.7) into complete `0xF0 .. 0xF7`
/// messages, one stream per Group, inside a buffer the caller provides and
/// that is never grown.
///
/// A consumer that wants whole bytes (a plugin format, which reads one pointer
/// for the whole `process` call; MIDI-CI, which parses a message) feeds each
/// UMP of its stream to [`push`](Self::push), **not only the SysEx7 packets**,
/// and reads a completed message back as a [`SysexMessage`]. The assembler
/// allocates nothing: it owns the buffer `B` (a `Vec<u8>` or boxed slice sized
/// off the audio thread, an array) and uses its length as its room, so a
/// pointer into [`bytes`](Self::bytes) is stable until the next
/// [`begin_block`](Self::begin_block). Size it with
/// `SysexAssembler::room_for`.
///
/// # The rule (M2-104 §7.7.1)
///
/// A System Exclusive stream belongs to its Group, so two Groups' messages
/// interleave freely. On a Group, "if any Message or UMP on the same Group,
/// other than a System Exclusive Continue UMP or a System Real Time Message,
/// is sent after a System Exclusive Start UMP and before the associated System
/// Exclusive End UMP, then that UMP shall terminate the System Exclusive
/// Message", and "Messages which are Groupless (MT = 0x0 and 0xF) and those
/// which are sent to a different Group may be interspersed". So a message is
/// ended, dropped and counted, by the next UMP of its Group that is not a
/// Continue or a System Real Time message (status `0xF8` to `0xFF` of type
/// `0x1`): a channel voice message, a System Common message, a Flex Data
/// message, a Complete, a Start (which then opens a new message), a SysEx8 or
/// Mixed Data Set UMP. A Utility or UMP Stream message has no Group and ends
/// nothing; neither does a reserved type, which names no Group here.
///
/// # Why there is no block guard
///
/// A design where the assembler lends one block (`block() -> Block<'_>`, a
/// `push` that lends the finished message, the block ending on drop) was
/// weighed and not built: its six callers (the CLAP, VST2, VST3 and AU hosts,
/// the plugin intake and MIDI-CI) hand a plugin a pointer to every message of
/// a block at once, so they keep each [`SysexRef`] until the whole block is
/// pushed. A lending `push` would make each copy its message first. The
/// contract stays in prose: read a [`SysexRef`] only before the next
/// [`begin_block`](Self::begin_block).
///
/// # Policy
///
/// - **At most `lanes` messages are open at once**, one per Group. Each owns a
///   slot of `max_message` bytes in the buffer; a `Start` on a Group while
///   every slot is held by other Groups is dropped whole and counted, and its
///   remaining packets are skipped. A host with one stream (a plugin format
///   with no Group, a MIDI 1.0 byte stream on its one Group) asks for one
///   lane; sixteen holds every Group's message at once, at sixteen messages of
///   room.
/// - **A message spans blocks.** A run left open at the end of a block (of
///   packets: a device callback, an audio block) continues in the next, so a
///   dump longer than one block's packets arrives whole.
/// - **A gap upstream drops every open message, counted.** A queue between the
///   source and this assembler that overflows truncates its block's tail; its
///   owner calls [`lost_tail`](Self::lost_tail) after pushing the kept
///   packets, so the packets of the next block are never spliced onto a run
///   that lost some in between.
/// - **Over capacity, the whole message is dropped and counted**, never
///   truncated: its remaining packets are skipped up to its `End`. The
///   capacity is the `max_message` given to [`new`](Self::new) and the
///   buffer's room.
/// - A `Continue` or `End` with no open message on its Group (its `Start` was
///   dropped, or arrived before this assembler existed) is skipped.
///
/// Each dropped message is counted once in [`dropped`](Self::dropped).
/// [`reset`](Self::reset) drops the open messages too.
///
/// # Memory
///
/// The buffer is the block's completed messages followed by `lanes` slots of
/// `max_message` bytes. A completed message is copied out of its slot, so a
/// pointer handed on is stable for the block while the slot is free for the
/// Group's next message. The messages completed in a block are those open when
/// it began (at most `max_message` bytes each, and each lane's message ends
/// at most once) plus those its packets make (at most eight bytes a packet),
/// so a buffer of `SysexAssembler::room_for(max_message, packets_per_block,
/// lanes)` never runs out for a block of up to that many packets.
#[derive(Debug)]
pub struct SysexAssembler<B> {
    buf: B,
    /// Completed messages of this block: `buf[..len]`, within `buf[..done_room]`.
    len: usize,
    done_room: usize,
    lanes: usize,
    max_message: usize,
    runs: [Run; GROUPS],
    dropped: u64,
}

impl SysexAssembler<()> {
    /// The buffer length that holds `lanes` open messages of `max_message`
    /// bytes (delimiters included) and, besides them, the completed messages of
    /// a block of `packets_per_block` packets: `max_message` for each message
    /// open when the block began, and eight bytes a packet.
    ///
    /// It is on `SysexAssembler<()>` so that a caller sizing a buffer names no
    /// buffer type: `SysexAssembler::room_for(max, packets, lanes)`.
    #[must_use]
    pub const fn room_for(max_message: usize, packets_per_block: usize, lanes: usize) -> usize {
        (2 * lanes) * max_message + packets_per_block * BYTES_PER_PACKET
    }
}

impl<B: AsRef<[u8]> + AsMut<[u8]>> SysexAssembler<B> {
    /// An assembler over `buf` for messages of up to `max_message` bytes,
    /// delimiters included, with up to `lanes` of them open at once (at most
    /// sixteen, one per Group). Its room is `buf`'s length: `lanes` slots of
    /// `max_message` bytes come off its end, as many as fit, and the rest holds
    /// completed messages. An empty buffer drops (and counts) every message.
    #[must_use]
    pub fn new(buf: B, max_message: usize, lanes: usize) -> Self {
        let room = buf.as_ref().len();
        let fit = room.checked_div(max_message).unwrap_or(0);
        let lanes = lanes.min(GROUPS).min(fit);
        Self {
            buf,
            len: 0,
            done_room: room - lanes * max_message,
            lanes,
            max_message,
            runs: [Run::Idle; GROUPS],
            dropped: 0,
        }
    }

    /// The longest message this assembler hands on, in bytes.
    #[must_use]
    pub fn max_message(&self) -> usize {
        self.max_message
    }

    /// Start a block: forget the last block's completed messages, keeping the
    /// open ones. Call it before pushing a block's packets, after the consumer
    /// has stopped reading the last block's. Never allocates.
    pub fn begin_block(&mut self) {
        self.len = 0;
    }

    /// Take in one UMP of the stream, whatever it is. Never allocates.
    ///
    /// A SysEx7 packet extends or completes its Group's message. Any other UMP
    /// is [`SysexStep::NotSysex`], for the caller to convert as usual, and ends
    /// its Group's open message unless it is a System Real Time message (see
    /// the type's rule). A malformed SysEx7 packet (a length past six, a data
    /// byte of 128 or more) is [`SysexStep::Held`] and has no effect.
    pub fn push(&mut self, ump: Ump) -> SysexStep {
        let (group, packet) = match ump.decode() {
            Ok(Message::Sysex7(group, packet)) => (group, packet),
            Ok(_) => return self.other(ump),
            Err(_) if ump.message_type() == 0x3 => return SysexStep::Held,
            Err(_) => return self.other(ump),
        };
        let payload = packet.data();
        let g = group.index();
        match packet.status() {
            SysexStatus::Complete => {
                self.end_run(g);
                self.single(payload)
            }
            SysexStatus::Start => {
                self.end_run(g);
                self.start(g, payload);
                SysexStep::Held
            }
            SysexStatus::Continue => {
                if let Run::Assembling { lane, len } = self.runs[g] {
                    self.extend(g, lane, len, payload);
                }
                SysexStep::Held
            }
            SysexStatus::End => match self.runs[g] {
                Run::Assembling { lane, len } => self.finish(g, lane, len, payload),
                Run::Discarding => {
                    self.runs[g] = Run::Idle;
                    SysexStep::Held
                }
                Run::Idle => SysexStep::Held,
            },
        }
    }

    /// An upstream queue dropped packets after the ones just pushed (it
    /// overflowed): drop every open message, counted, so packets of a later
    /// block are not spliced onto one. Call it after pushing a block's kept
    /// packets. Never allocates.
    pub fn lost_tail(&mut self) {
        for g in 0..GROUPS {
            self.end_run(g);
        }
    }

    /// The consumer was reset: drop the open messages (counted, like any
    /// message that never reaches it) and the block's completed ones. Never
    /// allocates.
    pub fn reset(&mut self) {
        self.lost_tail();
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
    /// [`max_message`](Self::max_message), ended before their `End` by a later
    /// UMP of their Group, found no free lane, cut by an upstream overflow
    /// ([`lost_tail`](Self::lost_tail)), open at a reset, or one that found the
    /// buffer's room for completed messages full (more packets in one block
    /// than it was sized for).
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// A UMP that is not a well-formed SysEx7 packet.
    fn other(&mut self, ump: Ump) -> SysexStep {
        if let Some(group) = ump.group() {
            // System Real Time (type 1, status 0xF8..=0xFF) may be inserted
            // between the UMPs of a message; anything else on the Group ends it.
            let real_time = ump.message_type() == 0x1 && (ump.0[0] >> 16) & 0xFF >= 0xF8;
            if !real_time {
                self.end_run(group.index());
            }
        }
        SysexStep::NotSysex
    }

    /// The Group's open message ends without its `End`: counted if it was
    /// being assembled (a discarded one was counted when it was dropped).
    fn end_run(&mut self, g: usize) {
        if matches!(self.runs[g], Run::Assembling { .. }) {
            self.dropped += 1;
        }
        self.runs[g] = Run::Idle;
    }

    /// Where lane `lane` starts in the buffer.
    fn lane_base(&self, lane: usize) -> usize {
        self.done_room + lane * self.max_message
    }

    fn free_lane(&self) -> Option<usize> {
        (0..self.lanes).find(|&lane| {
            !self
                .runs
                .iter()
                .any(|run| matches!(run, Run::Assembling { lane: l, .. } if *l == lane))
        })
    }

    /// Open a message on Group `g`: `0xF0`, then `payload`, in a free lane.
    fn start(&mut self, g: usize, payload: &[u8]) {
        let Some(lane) = self.free_lane() else {
            self.runs[g] = Run::Discarding;
            self.dropped += 1;
            return;
        };
        let base = self.lane_base(lane);
        self.buf.as_mut()[base] = 0xF0;
        self.extend(g, lane, 1, payload);
    }

    /// Append `payload` to Group `g`'s message of `len` bytes in `lane`, or,
    /// past the message's capacity (with its `0xF7`), drop it whole and skip
    /// the rest of it.
    fn extend(&mut self, g: usize, lane: usize, len: usize, payload: &[u8]) {
        let new_len = len + payload.len();
        if new_len + 1 > self.max_message {
            self.runs[g] = Run::Discarding;
            self.dropped += 1;
            return;
        }
        let at = self.lane_base(lane) + len;
        self.buf.as_mut()[at..at + payload.len()].copy_from_slice(payload);
        self.runs[g] = Run::Assembling { lane, len: new_len };
    }

    /// Close Group `g`'s message with `payload` and `0xF7`, and move it into
    /// the completed messages.
    fn finish(&mut self, g: usize, lane: usize, len: usize, payload: &[u8]) -> SysexStep {
        self.extend(g, lane, len, payload);
        let Run::Assembling { len, .. } = self.runs[g] else {
            // Over capacity: `extend` dropped it and counted.
            self.runs[g] = Run::Idle;
            return SysexStep::Held;
        };
        self.runs[g] = Run::Idle;
        let size = len + 1;
        if self.len + size > self.done_room {
            self.dropped += 1;
            return SysexStep::Held;
        }
        let base = self.lane_base(lane);
        let buf = self.buf.as_mut();
        buf[base + len] = 0xF7;
        buf.copy_within(base..base + size, self.len);
        let start = self.len;
        self.len += size;
        SysexStep::Complete(SysexRef { start, len: size })
    }

    /// Complete a one-packet message.
    fn single(&mut self, payload: &[u8]) -> SysexStep {
        let size = payload.len() + 2;
        if size > self.max_message || self.len + size > self.done_room {
            self.dropped += 1;
            return SysexStep::Held;
        }
        let start = self.len;
        let buf = self.buf.as_mut();
        buf[start] = 0xF0;
        buf[start + 1..start + 1 + payload.len()].copy_from_slice(payload);
        buf[start + size - 1] = 0xF7;
        self.len += size;
        SysexStep::Complete(SysexRef { start, len: size })
    }
}
