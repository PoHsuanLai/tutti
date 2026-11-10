//! Reassembly of multi-packet SysEx7 for delivery to a plugin.
//!
//! The engine carries SysEx as UMP SysEx7 packets of at most six bytes: a
//! lone `SINGLE`, or `START`, `CONTINUE`*, `END`. Every plugin format wants
//! one complete message, `0xF0 … 0xF7`, behind a pointer the plugin may read
//! for the whole `process` call. [`SysExAssembler`] turns the packets into
//! those messages inside one fixed buffer, reserved off the audio thread and
//! never grown, so reassembly does not allocate and a pointer handed to the
//! plugin is never moved during the block it was handed out in.

use tutti_midi_types::ump::SYSEX7_MAX_PAYLOAD;
use tutti_ump::{Message, MidiGroup, Sysex7Packets, SysexStatus, Ump, decode, frame};

use crate::{MIDI_STACK_CAPACITY, MidiEventVec, RtMidiEvents, WireMidi};

/// The longest complete message a plugin is handed, in bytes: the engine's
/// [`SYSEX7_MAX_PAYLOAD`] and its two delimiters. No input produces a longer
/// one (the hardware input abandons it), so nothing is dropped here for its
/// length alone.
///
/// This is the reassembler's limit, not the delivery path's. A plugin node
/// takes at most [`MIDI_STACK_CAPACITY`] MIDI events a chunk (a device
/// callback; a block for the in-process VST2 node), so a message longer
/// than that many packets (1536 payload bytes, less the chunk's other
/// events) reaches a plugin only from a source that spreads its packets
/// over chunks. The live hardware input does not: it emits a dump's packets
/// together, into one block. See [`SysExAssembler`]'s policy.
pub const SYSEX_MESSAGE_CAPACITY: usize = SYSEX7_MAX_PAYLOAD + 2;

/// The most bytes one SysEx7 packet adds to the buffer: six payload bytes and
/// both delimiters (a `SINGLE`).
const BYTES_PER_PACKET: usize = 8;

/// A SysEx message without its `0xF0` and `0xF7` delimiters, if it has them:
/// the payload UMP SysEx7 carries.
pub fn unframed(message: &[u8]) -> &[u8] {
    let payload = message.strip_prefix(&[0xF0]).unwrap_or(message);
    payload.strip_suffix(&[0xF7]).unwrap_or(payload)
}

/// The SysEx7 packets (on the first group) of a plugin's `message`, with its
/// `0xF0` and `0xF7` delimiters or without, or `None` if a payload byte is 128
/// or more: a plugin emitted a malformed message, which the caller drops
/// (nothing is masked into a different message).
#[must_use]
pub fn sysex_packets(message: &[u8]) -> Option<Sysex7Packets<'_>> {
    Sysex7Packets::checked(MidiGroup::FIRST, unframed(message))
}

/// Whether `message` is a complete SysEx message as `MusicDeviceSysEx` wants it
/// (`MusicDevice.h`: "including the F0 and F7 start and termination bytes").
/// [`SysExAssembler`] hands over messages in this form; the AU host checks
/// what it sends with this.
#[must_use]
pub fn is_framed(message: &[u8]) -> bool {
    message.len() >= 2 && message[0] == 0xF0 && message[message.len() - 1] == 0xF7
}

/// Pushes one complete SysEx message a plugin emitted onto `out` as UMP
/// SysEx7 packets (a lone `SINGLE`, or `START`, `CONTINUE`*, `END`), every
/// packet at `frame_offset`, the inverse of what [`SysExAssembler`] does on
/// the input side. `message` may carry its `0xF0` and `0xF7` delimiters or
/// not. Never allocates.
///
/// A message is delivered whole or not at all: one whose packets do not all
/// fit in `out`'s room is dropped and latches `out`'s
/// [`is_overflowed`](tutti_rt::RtVec::is_overflowed), because a `START` left
/// without its `END` would corrupt the stream that follows it. Returns
/// whether it was pushed. A message with a payload byte of 128 or more is
/// malformed ([`sysex_packets`]): it is dropped, not pushed, and `out` does not
/// latch (it did not overflow).
pub fn push_sysex(out: &mut RtMidiEvents, frame_offset: u32, message: &[u8]) -> bool {
    let Some(packets) = sysex_packets(message) else {
        return false;
    };
    if packets.len() > out.remaining() {
        out.mark_overflowed();
        return false;
    }
    for packet in packets {
        out.push(WireMidi {
            offset: frame_offset,
            ump: packet,
        });
    }
    true
}

/// [`push_sysex`] for an inline [`MidiEventVec`] (a stack batch), with the
/// same whole-or-nothing rule: a message whose packets do not all fit in
/// `out`'s remaining room is dropped and `false` is returned. Never allocates.
pub fn push_sysex_stack(out: &mut MidiEventVec, frame_offset: u32, message: &[u8]) -> bool {
    let Some(packets) = sysex_packets(message) else {
        return false;
    };
    if packets.len() > out.remaining_capacity() {
        return false;
    }
    for packet in packets {
        out.push(WireMidi {
            offset: frame_offset,
            ump: packet,
        });
    }
    true
}

/// Where one complete message lies in its [`SysExAssembler`]'s buffer.
///
/// Valid until the assembler's next [`begin_block`](SysExAssembler::begin_block).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SysExRef {
    start: u32,
    len: u32,
}

impl SysExRef {
    /// The message lying at `start..start + len` of some buffer the caller
    /// owns (a host reading a plugin's output keeps its own).
    #[allow(
        clippy::cast_possible_truncation,
        reason = "a buffer reserved for SysEx messages is far below u32::MAX bytes"
    )]
    #[must_use]
    pub fn new(start: usize, len: usize) -> Self {
        Self {
            start: start as u32,
            len: len as u32,
        }
    }

    /// The message's byte range in [`SysExAssembler::bytes`].
    #[must_use]
    pub fn range(self) -> std::ops::Range<usize> {
        let start = self.start as usize;
        start..start + self.len as usize
    }

    /// The message's length in bytes, delimiters included.
    #[must_use]
    pub fn len(self) -> usize {
        self.len as usize
    }

    /// Always false: a complete message holds at least its two delimiters.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.len == 0
    }
}

/// What one event did to a [`SysExAssembler`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SysExStep {
    /// Not a SysEx7 packet: the caller converts it as usual.
    NotSysEx,
    /// A packet taken in with no message to hand on yet: the start or middle
    /// of a message, or a packet of one that is being dropped.
    Held,
    /// A message completed: hand the plugin these bytes.
    Complete(SysExRef),
}

/// The multi-packet message being assembled, if any.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Run {
    Idle,
    /// Its bytes so far, `0xF0` first, are `buf[start..]`.
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

/// Reassembles SysEx7 packets into complete `0xF0 … 0xF7` messages for one
/// plugin instance's input, in a buffer reserved once.
///
/// # Policy
///
/// - **A message spans blocks.** One chunk (a device callback; a block for
///   the in-process VST2 node) carries at most [`MIDI_STACK_CAPACITY`]
///   packets (1536 payload bytes), so a dump longer than that cannot arrive
///   in one. A run left open at the end of a block continues in the next.
/// - **A source that delivers a whole dump in one block is limited by the
///   chunk's cap.** The live hardware input does (its input node keeps 512
///   events a block, the plugin node 256 a chunk): the plugin node drops,
///   counted, the events past its cap and the rest of a SysEx that lost a
///   packet there, so the message never gets its `END` here and is dropped
///   whole, counted at the next `START`. On that path a dump reaches a
///   plugin only if its packets and the chunk's other MIDI fit in
///   [`MIDI_STACK_CAPACITY`] events.
/// - **One multi-packet message at a time, across all groups.** A `START`
///   while another is still open, on any group, drops the open one, whole,
///   and counts it: it never got its `END`. This deviates on purpose from
///   M2-104's independent streams per group: no plugin format has a SysEx
///   group to deliver it on, so two interleaved dumps could only reach the
///   plugin as two messages anyway, and the hardware input puts all its
///   traffic on group 1. One open run keeps the buffer one message long.
///   A `SINGLE` on another group completes beside an open run; on the run's
///   own group it ends the run (dropped and counted), as M2-104 §7.7.1 has it.
/// - **A gap upstream drops the open message, counted.** A list between the
///   node and this assembler that overflows truncates its block's tail; its
///   owner calls [`lost_tail`](Self::lost_tail) after staging the kept
///   events, so the packets of the next block are never spliced onto a run
///   that lost some in between.
/// - **Other messages do not end a run.** The plugin's event list merges
///   every source, so a note played during a dump arrives between its
///   packets; ending the dump there would cost the dump for no gain.
/// - **Over capacity, the whole message is dropped and counted**, never
///   truncated: its remaining packets are skipped up to its `END`.
/// - A `CONTINUE` or `END` with no open run on its group (its `START` was
///   dropped, or arrived before this instance existed) is skipped.
///
/// Each dropped message is counted once in [`dropped`](Self::dropped).
/// [`reset`](Self::reset) (the plugin's reset or re-activation) drops the
/// open message too.
///
/// # Memory
///
/// The buffer holds the block's completed messages and, after them, the open
/// run. It is reserved for [`SYSEX_MESSAGE_CAPACITY`] plus
/// [`MIDI_STACK_CAPACITY`] packets' worth: the open run carried into a block
/// is shorter than one message, and each of the block's packets adds at most
/// eight bytes, so a block within the MIDI cap never runs out of room. That
/// is about 66 KiB per plugin instance, reserved once.
#[derive(Debug)]
pub struct SysExAssembler {
    /// Completed messages of this block, then the open run. Never grows past
    /// `room`, which is within its reserved capacity.
    buf: Vec<u8>,
    room: usize,
    max_message: usize,
    run: Run,
    dropped: u64,
}

impl Default for SysExAssembler {
    /// An assembler with no room, which drops (and counts) every message.
    /// Allocates nothing; reserve one with [`reserved`](Self::reserved).
    fn default() -> Self {
        Self::with_capacity(0)
    }
}

impl SysExAssembler {
    /// An assembler for messages of up to [`SYSEX_MESSAGE_CAPACITY`] bytes.
    /// Off the audio thread: it reserves the buffer.
    pub fn reserved() -> Self {
        Self::with_capacity(SYSEX_MESSAGE_CAPACITY)
    }

    /// An assembler for messages of up to `max_message` bytes, delimiters
    /// included. Off the audio thread: it reserves the buffer (none for 0).
    #[must_use]
    pub fn with_capacity(max_message: usize) -> Self {
        let room = if max_message == 0 {
            0
        } else {
            max_message + MIDI_STACK_CAPACITY * BYTES_PER_PACKET
        };
        Self {
            buf: Vec::with_capacity(room),
            room,
            max_message,
            run: Run::Idle,
            dropped: 0,
        }
    }

    /// The longest message this assembler hands on, in bytes.
    pub fn max_message(&self) -> usize {
        self.max_message
    }

    /// Start a block: forget the last block's completed messages, keeping an
    /// open run. Call it before staging a block's events, after the plugin
    /// has stopped reading the last block's. Never allocates.
    pub fn begin_block(&mut self) {
        match self.run {
            Run::Assembling { start, group } => {
                self.buf.copy_within(start.., 0);
                self.buf.truncate(self.buf.len() - start);
                self.run = Run::Assembling { start: 0, group };
            }
            Run::Idle | Run::Discarding { .. } => self.buf.clear(),
        }
    }

    /// Take in one event. Never allocates.
    ///
    /// A malformed SysEx7 packet (a length past six, a data byte of
    /// 128 or more) is [`SysExStep::Held`] and has no effect.
    pub fn push(&mut self, event: &Ump) -> SysExStep {
        let (group, packet) = match decode(*event) {
            Ok(Message::Sysex7(group, packet)) => (group, packet),
            Ok(_) => return SysExStep::NotSysEx,
            // A SysEx7 packet `decode` refuses (a length past six, a data byte
            // of 128 or more) is held and has no effect.
            Err(_) if event.message_type() == 0x3 => return SysExStep::Held,
            Err(_) => return SysExStep::NotSysEx,
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
                let start = self.buf.len();
                self.run = Run::Assembling { start, group };
                self.append(&[0xF0], payload, &[]);
                SysExStep::Held
            }
            SysexStatus::Continue => {
                if self.assembling(group) {
                    self.append(&[], payload, &[]);
                }
                SysExStep::Held
            }
            SysexStatus::End => match self.run {
                Run::Assembling { start, group: g } if g == group => {
                    if !self.append(&[], payload, &[0xF7]) {
                        return SysExStep::Held;
                    }
                    self.run = Run::Idle;
                    SysExStep::Complete(sysex_ref(start, self.buf.len() - start))
                }
                Run::Discarding { group: g } if g == group => {
                    self.run = Run::Idle;
                    SysExStep::Held
                }
                _ => SysExStep::Held,
            },
        }
    }

    /// An upstream list dropped events after the ones just pushed (it
    /// overflowed): drop the open message, counted, so packets of a later
    /// block are not spliced onto it. Call it after pushing a block's kept
    /// events. Never allocates.
    pub fn lost_tail(&mut self) {
        self.abandon();
    }

    /// The plugin was reset or re-activated: drop the open message (counted,
    /// like any message that never reaches the plugin) and the block's
    /// completed ones. Never allocates.
    pub fn reset(&mut self) {
        self.abandon();
        self.buf.clear();
    }

    /// The buffer every [`SysExRef`] of this block indexes. Its address is
    /// stable until the next [`begin_block`](Self::begin_block).
    pub fn bytes(&self) -> &[u8] {
        &self.buf
    }

    /// The bytes of a completed message, `0xF0` to `0xF7`.
    pub fn message(&self, message: SysExRef) -> &[u8] {
        &self.buf[message.range()]
    }

    /// Messages dropped whole since this assembler was made (a
    /// [`reset`](Self::reset) does not zero it): longer than
    /// [`max_message`](Self::max_message), left without their `END` by a
    /// later `START` (or a `SINGLE` on their group), cut by an upstream
    /// overflow ([`lost_tail`](Self::lost_tail)), open at a reset, or one
    /// whose packet found the buffer's room full (more packets in one block
    /// than it reserves for).
    pub fn dropped(&self) -> u64 {
        self.dropped
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
            self.buf.truncate(start);
            self.dropped += 1;
        }
        self.run = Run::Idle;
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
        let closing = if tail.is_empty() { 1 } else { 0 };
        let message = self.buf.len() - start + added + closing;
        if message > self.max_message || self.buf.len() + added > self.room {
            self.buf.truncate(start);
            self.dropped += 1;
            self.run = Run::Discarding { group };
            return false;
        }
        self.buf.extend_from_slice(head);
        self.buf.extend_from_slice(payload);
        self.buf.extend_from_slice(tail);
        true
    }

    /// Complete a one-packet message, placed before the open run (if any) so
    /// the run stays last and can keep growing.
    fn single(&mut self, payload: &[u8]) -> SysExStep {
        let len = payload.len() + 2;
        if len > self.max_message || self.buf.len() + len > self.room {
            self.dropped += 1;
            return SysExStep::Held;
        }
        let at = match self.run {
            Run::Assembling { start, .. } => start,
            Run::Idle | Run::Discarding { .. } => self.buf.len(),
        };
        self.buf.extend(frame(payload));
        // Rotate the message in front of the open run. The run has no
        // pointer handed out yet, so moving it is free of hazard.
        self.buf[at..].rotate_right(len);
        if let Run::Assembling { start, group } = self.run {
            self.run = Run::Assembling {
                start: start + len,
                group,
            };
        }
        SysExStep::Complete(sysex_ref(at, len))
    }
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "the buffer's room is a message's capacity plus a block of packets, far below u32::MAX"
)]
fn sysex_ref(start: usize, len: usize) -> SysExRef {
    SysExRef {
        start: start as u32,
        len: len as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_midi_types::from_midi1_bytes;

    fn packets(group: MidiGroup, payload: &[u8]) -> Vec<Ump> {
        Sysex7Packets::checked(group, payload)
            .expect("a 7-bit payload")
            .collect()
    }

    fn framed(payload: &[u8]) -> Vec<u8> {
        frame(payload).collect()
    }

    /// Feed `events`, collecting each completed message's bytes.
    fn feed(asm: &mut SysExAssembler, events: &[Ump]) -> Vec<Vec<u8>> {
        let mut done = Vec::new();
        for e in events {
            if let SysExStep::Complete(r) = asm.push(e) {
                done.push(asm.message(r).to_vec());
            }
        }
        done
    }

    fn payload(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 0x7F) as u8).collect()
    }

    /// **A multi-packet message completes as one framed message on its
    /// `END`**, with every payload byte in order.
    ///
    /// Mutation (run): `END` not appending its own payload → the last bytes
    /// are missing → fails.
    #[test]
    fn a_multi_packet_message_completes_on_its_end() {
        let body = payload(40);
        let mut asm = SysExAssembler::reserved();
        let pkts = packets(MidiGroup::FIRST, &body);
        assert!(pkts.len() > 2, "START, CONTINUE…, END");
        let done = feed(&mut asm, &pkts);
        assert_eq!(done, vec![framed(&body)]);
        assert_eq!(asm.dropped(), 0);
    }

    /// **A message spans a block boundary**: an open run survives
    /// `begin_block`, and completes in the next block.
    ///
    /// Mutation (run): `begin_block` clearing the buffer and the run whatever
    /// the run's state → the `END` finds no run → nothing completes → fails.
    #[test]
    fn an_open_run_carries_across_blocks() {
        let body = payload(30);
        let pkts = packets(MidiGroup::FIRST, &body);
        let mut asm = SysExAssembler::reserved();
        // A single first, so the run does not start at offset 0 and the
        // carry has to move it.
        let one = packets(MidiGroup::FIRST, &[1, 2]);
        assert_eq!(feed(&mut asm, &one), vec![framed(&[1, 2])]);
        let (first, rest) = pkts.split_at(2);
        assert!(feed(&mut asm, first).is_empty());
        asm.begin_block();
        assert_eq!(feed(&mut asm, rest), vec![framed(&body)]);
        assert_eq!(asm.dropped(), 0);
    }

    /// **A message past capacity is dropped whole and counted once**; the
    /// next message is delivered intact.
    ///
    /// Mutations (run): the capacity check removed → the long message
    /// completes → fails; the dropped run truncated but left `Assembling`
    /// (not `Discarding`) → its tail completes as a headless message →
    /// fails.
    #[test]
    fn a_message_past_capacity_is_dropped_whole_and_counted() {
        let mut asm = SysExAssembler::with_capacity(20);
        let long = payload(19); // 21 framed bytes: one past capacity
        assert!(feed(&mut asm, &packets(MidiGroup::FIRST, &long)).is_empty());
        assert_eq!(asm.dropped(), 1);
        let fits = payload(18); // exactly 20 framed bytes
        assert_eq!(
            feed(&mut asm, &packets(MidiGroup::FIRST, &fits)),
            vec![framed(&fits)]
        );
        assert_eq!(asm.dropped(), 1);
        assert!(asm.bytes().len() <= 20);
    }

    /// **A `START` with no `END` before the next `START` drops the open
    /// message, counted**, and the second message is delivered whole.
    ///
    /// Mutation (run): `START` not abandoning the open run → the second
    /// message is appended to the first → fails.
    #[test]
    fn a_start_before_the_open_runs_end_drops_it() {
        let a = payload(20);
        let b = payload(15);
        let pa = packets(MidiGroup::FIRST, &a);
        let pb = packets(MidiGroup::FIRST, &b);
        let mut asm = SysExAssembler::reserved();
        // A's START and a CONTINUE, then B whole.
        assert!(feed(&mut asm, &pa[..2]).is_empty());
        assert_eq!(feed(&mut asm, &pb), vec![framed(&b)]);
        assert_eq!(asm.dropped(), 1);
        // A's tail is an orphan now: skipped, not counted again.
        assert!(feed(&mut asm, &pa[2..]).is_empty());
        assert_eq!(asm.dropped(), 1);
    }

    /// **A `SINGLE` on another group completes beside an open run without
    /// disturbing it**, and on the run's own group it ends the run.
    ///
    /// Mutation (run): `single` appending after the run instead of rotating
    /// in front of it → the run's bytes are split by the single → fails.
    #[test]
    fn a_single_beside_an_open_run() {
        let body = payload(25);
        let pkts = packets(MidiGroup::FIRST, &body);
        let other = MidiGroup::of(3);
        let mut asm = SysExAssembler::reserved();
        assert!(feed(&mut asm, &pkts[..2]).is_empty());
        assert_eq!(
            feed(&mut asm, &packets(other, &[9, 8, 7])),
            vec![framed(&[9, 8, 7])]
        );
        assert_eq!(feed(&mut asm, &pkts[2..]), vec![framed(&body)]);
        assert_eq!(asm.dropped(), 0);

        // On the run's own group a SINGLE ends the run.
        assert!(feed(&mut asm, &pkts[..2]).is_empty());
        assert_eq!(
            feed(&mut asm, &packets(MidiGroup::FIRST, &[5])),
            vec![framed(&[5])]
        );
        assert_eq!(asm.dropped(), 1);
    }

    /// **Other messages pass through an open run** (`NotSysEx`) and do not
    /// end it.
    ///
    /// Mutation (run): a non-SysEx event abandoning the run → the message
    /// never completes → fails.
    #[test]
    fn other_messages_do_not_end_a_run() {
        use tutti_ump::MidiChannel;
        let body = payload(20);
        let pkts = packets(MidiGroup::FIRST, &body);
        let note = tutti_fixtures::midi::note_on(MidiGroup::FIRST, MidiChannel::of(0), 60, 0x8000);
        let mut asm = SysExAssembler::reserved();
        assert!(feed(&mut asm, &pkts[..2]).is_empty());
        assert_eq!(asm.push(&note), SysExStep::NotSysEx);
        assert_eq!(feed(&mut asm, &pkts[2..]), vec![framed(&body)]);
    }

    /// **An unreserved assembler drops (and counts) every message**, rather
    /// than growing.
    ///
    /// Mutation (run): the room check removed from `single` → the message is
    /// pushed past the zero-capacity buffer, allocating → fails on the count.
    #[test]
    fn an_unreserved_assembler_drops_everything() {
        let mut asm = SysExAssembler::default();
        assert!(feed(&mut asm, &packets(MidiGroup::FIRST, &[1])).is_empty());
        assert!(feed(&mut asm, &packets(MidiGroup::FIRST, &payload(20))).is_empty());
        assert_eq!(asm.dropped(), 2);
        assert_eq!(asm.bytes().len(), 0);
    }

    /// **The longest message fits, carried across blocks of a full MIDI cap
    /// each, without allocating**, and the buffer's address never moves.
    ///
    /// Mutation (run): the room computed as `max_message` alone (no block of
    /// packets on top) → a single beside the carried run does not fit →
    /// counted → fails.
    #[test]
    fn the_longest_message_fits_without_allocating() {
        let body = payload(SYSEX7_MAX_PAYLOAD);
        let pkts = packets(MidiGroup::FIRST, &body);
        let mut asm = SysExAssembler::reserved();
        let base = asm.bytes().as_ptr();
        let singles: Vec<Ump> = (0..MIDI_STACK_CAPACITY)
            .flat_map(|_| packets(MidiGroup::of(1), &[1, 2, 3, 4, 5, 6]))
            .collect();
        let mut done = 0usize;
        assert_no_alloc::assert_no_alloc(|| {
            // Packets one MIDI block at a time, the final block's room filled
            // with singles on another group.
            for chunk in pkts.chunks(MIDI_STACK_CAPACITY - 1) {
                asm.begin_block();
                for p in chunk {
                    if let SysExStep::Complete(r) = asm.push(p) {
                        done += 1;
                        assert_eq!(r.len(), SYSEX_MESSAGE_CAPACITY);
                    }
                }
            }
            for s in &singles[..MIDI_STACK_CAPACITY / 2] {
                if let SysExStep::Complete(_) = asm.push(s) {
                    done += 1;
                }
            }
        });
        assert_eq!(asm.dropped(), 0);
        assert_eq!(done, 1 + MIDI_STACK_CAPACITY / 2);
        assert_eq!(asm.bytes().as_ptr(), base, "the buffer never moved");
    }

    /// **More packets in one block than the buffer has room for drop the
    /// message that would overflow it, counted, without allocating.** A
    /// complete longest message fills the message part of the buffer; the
    /// next message, short of the message cap but past the room, is dropped.
    ///
    /// Mutation (run): `|| self.buf.len() + added > self.room` removed from
    /// `append` → the buffer grows past its reservation → allocates → fails.
    #[test]
    fn more_packets_than_room_drop_without_allocating() {
        let longest = packets(MidiGroup::FIRST, &payload(SYSEX7_MAX_PAYLOAD));
        // Twice a block of packets: more than the room left after `longest`.
        let next = packets(MidiGroup::FIRST, &payload(2 * MIDI_STACK_CAPACITY * 6));
        assert!(next.len() > MIDI_STACK_CAPACITY);
        let mut asm = SysExAssembler::reserved();
        let mut done = 0usize;
        assert_no_alloc::assert_no_alloc(|| {
            asm.begin_block();
            for p in longest.iter().chain(&next) {
                if let SysExStep::Complete(_) = asm.push(p) {
                    done += 1;
                }
            }
        });
        assert_eq!(done, 1, "the longest message, not the one past the room");
        assert_eq!(asm.dropped(), 1);
    }

    /// **A tail lost upstream drops the open message, counted, and the rest
    /// of its packets in a later block are not spliced onto it.**
    ///
    /// Mutation (run): `lost_tail` a no-op → the later packets complete the
    /// run as one message missing its middle → fails.
    #[test]
    fn a_lost_tail_drops_the_open_message() {
        let body = payload(60);
        let pkts = packets(MidiGroup::FIRST, &body);
        let mut asm = SysExAssembler::reserved();
        assert!(feed(&mut asm, &pkts[..3]).is_empty());
        asm.lost_tail(); // pkts[3..5] never arrive
        asm.begin_block();
        assert!(feed(&mut asm, &pkts[5..]).is_empty());
        assert_eq!(asm.dropped(), 1);
        // The next message is whole.
        assert_eq!(feed(&mut asm, &pkts), vec![framed(&body)]);
        assert_eq!(asm.dropped(), 1);
    }

    /// **A reset drops the open message, counted**, so its remaining packets
    /// after the reset are skipped rather than completing it.
    ///
    /// Mutation (run): `reset` only clearing the buffer (the run left
    /// `Assembling`) → not counted → fails.
    #[test]
    fn a_reset_drops_the_open_message() {
        let body = payload(30);
        let pkts = packets(MidiGroup::FIRST, &body);
        let mut asm = SysExAssembler::reserved();
        assert!(feed(&mut asm, &pkts[..2]).is_empty());
        asm.reset();
        assert_eq!(asm.dropped(), 1);
        assert!(asm.bytes().is_empty());
        assert!(feed(&mut asm, &pkts[2..]).is_empty());
        assert_eq!(asm.dropped(), 1);
    }

    /// **A plugin's SysEx of any length reaches the pool intact**, framed or
    /// bare, and reassembles to the message it was.
    ///
    /// Mutation (run): in `push_sysex`, keep only the first packet (or cap
    /// the payload at 6 bytes) → the 40-byte message comes back short →
    /// fails.
    #[test]
    fn a_plugins_sysex_of_any_length_arrives_whole() {
        let mut asm = SysExAssembler::reserved();
        for len in [0, 1, 6, 7, 14, 17, 40, 63 * 6] {
            let body = payload(len);
            for message in [framed(&body), body.clone()] {
                let mut out = RtMidiEvents::new();
                assert!(push_sysex(&mut out, 9, &message), "{len} bytes");
                assert!(out.iter().all(|e| e.offset == 9));
                assert!(!out.is_overflowed());
                assert_eq!(
                    feed(&mut asm, &out.iter().map(|e| e.ump).collect::<Vec<_>>()),
                    vec![framed(&body)]
                );
            }
        }
        assert_eq!(asm.dropped(), 0);
    }

    /// **A message that does not fit the pool is dropped whole and
    /// reported**, never cut: a `START` with no `END` would swallow the
    /// stream after it.
    ///
    /// Mutation (run): drop the `packets > out.remaining()` check so the
    /// packets are pushed until the pool is full → the pool holds a `START`
    /// with no `END` → fails.
    #[test]
    fn a_message_past_the_pool_is_dropped_whole() {
        let mut out = RtMidiEvents::new();
        let room = out.remaining();
        let too_long = payload(room * 6 + 1);
        assert!(!push_sysex(&mut out, 0, &too_long));
        assert!(out.is_empty(), "no partial message");
        assert!(out.is_overflowed());

        // What fits after another event is judged on the room that is left.
        let mut out = RtMidiEvents::new();
        assert!(out.push(WireMidi {
            offset: 0,
            ump: from_midi1_bytes(&[0x90, 60, 100]).unwrap()
        }));
        assert!(!push_sysex(&mut out, 0, &payload((room - 1) * 6 + 1)));
        assert_eq!(out.len(), 1);
        assert!(push_sysex(&mut out, 0, &payload((room - 1) * 6)));
        assert_eq!(out.len(), room);
    }

    /// **A message the assembler completes is framed, `0xF0` to `0xF7`**,
    /// as `MusicDeviceSysEx` takes it; an unframed payload is not.
    ///
    /// Mutation (run): hand `unframed(message)` on instead of the message (as
    /// the AU host did) → `is_framed` is false → fails.
    #[test]
    fn an_assembled_message_is_framed_for_the_au() {
        let mut asm = SysExAssembler::reserved();
        for len in [0, 3, 6, 7, 40] {
            let body = payload(len);
            let done = feed(&mut asm, &packets(MidiGroup::FIRST, &body));
            assert_eq!(done.len(), 1);
            assert!(is_framed(&done[0]), "{len} bytes");
            assert!(!is_framed(unframed(&done[0])));
        }
        assert!(!is_framed(&[]));
        assert!(!is_framed(&[0xF0]));
        assert!(is_framed(&[0xF0, 0xF7]));
    }
}
