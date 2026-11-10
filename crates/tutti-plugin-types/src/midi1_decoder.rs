//! Decoding a plugin's MIDI 1.0 byte stream into engine events, SysEx whole.
//!
//! A host that gets a plugin's MIDI as MIDI 1.0 bytes (the AU callback's
//! `MIDIPacketList`) has a stream, not messages: a packet may carry several
//! messages, running status, real-time bytes inside another message, and a
//! SysEx that starts in one packet and ends in a later one. [`Midi1Decoder`]
//! turns each packet's bytes into [`WireMidi`]s, with a SysEx message as UMP
//! SysEx7 packets (the inverse of what [`SysExAssembler`](crate::SysExAssembler)
//! does for input), and hands them to the caller in batches. The platform
//! code only walks its own packet structure and calls in.
//!
//! Nothing here allocates after [`Midi1Decoder::new`]: the SysEx capture
//! buffer is reserved there, and the batch is inline.

use tutti_midi_types::{from_midi1_bytes, sysex7_packet_count};

use crate::{MIDI_STACK_CAPACITY, MidiEventVec, WireMidi, push_sysex_stack};

/// The longest SysEx message the decoder delivers, in bytes, delimiters
/// included: what [`MIDI_STACK_CAPACITY`] SysEx7 packets (six payload bytes
/// each) carry. A message is handed over in one batch, so it cannot be longer
/// than a batch holds. A longer one is dropped whole and counted, never cut.
pub const SYSEX_OUT_CAPACITY: usize = MIDI_STACK_CAPACITY * 6 + 2;

/// The SysEx message being captured, if any.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Capture {
    Idle,
    /// `buf` holds `0xF0` and the data bytes so far.
    Collecting {
        frame_offset: u32,
    },
    /// Over capacity and already counted: its remaining bytes are skipped up
    /// to its `0xF7`.
    Discarding,
}

/// One message found in a MIDI 1.0 byte stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Midi1Message<'a> {
    /// A channel, system-common or real-time message: its status byte and
    /// data bytes, running status expanded.
    Short(&'a [u8]),
    /// A complete SysEx message, `0xF0 … 0xF7`.
    SysEx(&'a [u8]),
}

/// Splits MIDI 1.0 bytes into messages, a SysEx message whole. The stateful
/// half of [`Midi1Decoder`]; see its policy.
#[derive(Debug)]
pub struct Midi1Splitter {
    /// The open SysEx message, delimiters included. Reserved once, never
    /// grown past [`SYSEX_OUT_CAPACITY`].
    buf: Vec<u8>,
    capture: Capture,
    sysex_dropped: u64,
}

impl Default for Midi1Splitter {
    fn default() -> Self {
        Self::new()
    }
}

impl Midi1Splitter {
    /// A splitter. Off the audio thread: it reserves the SysEx buffer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            buf: Vec::with_capacity(SYSEX_OUT_CAPACITY),
            capture: Capture::Idle,
            sysex_dropped: 0,
        }
    }

    /// SysEx messages dropped since this splitter was made: longer than
    /// [`SYSEX_OUT_CAPACITY`], or ended by another status byte before their
    /// `0xF7`.
    pub fn sysex_dropped(&self) -> u64 {
        self.sysex_dropped
    }

    /// Reads one packet's `bytes`, calling `f(frame_offset, message)` for
    /// each message they complete. A SysEx message is stamped with the
    /// offset of the packet that held its `0xF0`. Never allocates.
    pub fn split(
        &mut self,
        bytes: &[u8],
        frame_offset: u32,
        f: &mut impl FnMut(u32, Midi1Message<'_>),
    ) {
        // The status in effect for running status. It does not outlive the
        // packet.
        let mut running: Option<u8> = None;
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            if self.capture != Capture::Idle {
                match b {
                    0x00..=0x7F => self.take_data(b),
                    // Real-time: delivered at once, the message goes on.
                    0xF8..=0xFF => f(frame_offset, Midi1Message::Short(&[b])),
                    0xF7 => self.end_sysex(f),
                    // Any other status ends the message unfinished.
                    _ => {
                        if matches!(self.capture, Capture::Collecting { .. }) {
                            self.sysex_dropped += 1;
                        }
                        self.buf.clear();
                        self.capture = Capture::Idle;
                        continue;
                    }
                }
                i += 1;
                continue;
            }
            if b >= 0x80 {
                if b == 0xF0 {
                    running = None;
                    self.buf.clear();
                    self.buf.push(0xF0);
                    self.capture = Capture::Collecting { frame_offset };
                    i += 1;
                    continue;
                }
                let Some(n) = data_len(b) else {
                    // A stray `0xF7`, or a status this host does not deliver.
                    running = None;
                    i += 1;
                    continue;
                };
                // Real-time bytes leave running status alone: they may sit
                // between another message's data bytes.
                if !(0xF8..=0xFF).contains(&b) {
                    running = if b < 0xF0 { Some(b) } else { None };
                }
                let mut msg = [0u8; 3];
                msg[0] = b;
                match gather(bytes, i + 1, &mut msg, n, frame_offset, f) {
                    Gathered::Done(next) | Gathered::Aborted(next) => i = next,
                    // Its data bytes run past the packet: dropped, not read.
                    Gathered::Cut => return,
                }
            } else {
                // A data byte: running status, if one is in effect.
                let Some(status) = running else {
                    i += 1;
                    continue;
                };
                let n = data_len(status).unwrap_or(0);
                if n == 0 {
                    return;
                }
                // `[u8; 3]` covers every message running status can carry.
                let mut msg = [0u8; 3];
                msg[0] = status;
                match gather(bytes, i, &mut msg, n, frame_offset, f) {
                    Gathered::Done(next) | Gathered::Aborted(next) => i = next,
                    Gathered::Cut => return,
                }
            }
        }
    }

    /// A new list of packets begins (a new callback): an open SysEx message
    /// now continues in a later block than the one it started in, so it is
    /// stamped at the start of the block that completes it.
    pub fn begin_list(&mut self) {
        if let Capture::Collecting { frame_offset } = &mut self.capture {
            *frame_offset = 0;
        }
    }

    fn take_data(&mut self, b: u8) {
        if self.capture == Capture::Discarding {
            return;
        }
        // Room for this byte and the closing `0xF7`.
        if self.buf.len() + 2 > SYSEX_OUT_CAPACITY {
            self.buf.clear();
            self.sysex_dropped += 1;
            self.capture = Capture::Discarding;
            return;
        }
        self.buf.push(b);
    }

    fn end_sysex(&mut self, f: &mut impl FnMut(u32, Midi1Message<'_>)) {
        let Capture::Collecting { frame_offset } = self.capture else {
            self.capture = Capture::Idle;
            return;
        };
        self.capture = Capture::Idle;
        self.buf.push(0xF7);
        f(frame_offset, Midi1Message::SysEx(&self.buf));
        self.buf.clear();
    }
}

/// Decodes MIDI 1.0 bytes into [`WireMidi`]s. See the module docs.
///
/// # Policy
///
/// - **A SysEx message is delivered whole or not at all**, as UMP SysEx7
///   packets contiguous in one batch, at the frame offset of the packet that
///   held its `0xF0`.
/// - **It may span packets, and calls.** The open message is kept until its
///   `0xF7`. One that goes on into a later call (after
///   [`begin_list`](Self::begin_list)) is stamped at offset 0, the start of the
///   block that completes it; its first packet's offset belongs to a block
///   that is gone.
/// - **Over [`SYSEX_OUT_CAPACITY`] bytes, it is dropped whole and counted**
///   once in [`sysex_dropped`](Self::sysex_dropped); the rest of its bytes
///   are skipped up to its `0xF7`.
/// - **Any non-real-time status byte ends a SysEx** (MIDI 1.0). One that is
///   not `0xF7` means the message never finished: it is dropped and counted,
///   and the byte is read as usual (an `0xF0` starts the next message).
/// - **Real-time bytes inside a SysEx** are delivered at once and the
///   message continues.
/// - Other messages are as `from_midi1_bytes` reads them; a
///   data byte with no status to attribute it to, or a status whose data
///   bytes are cut short, is dropped. Real-time bytes among a message's data
///   are delivered and the message goes on; any other status byte there ends
///   the message unfinished (dropped) and is read as the start of the next.
#[derive(Debug)]
pub struct Midi1Decoder {
    splitter: Midi1Splitter,
    batch: MidiEventVec,
}

impl Default for Midi1Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Midi1Decoder {
    /// A decoder. Off the audio thread: it reserves the SysEx buffer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            splitter: Midi1Splitter::new(),
            batch: MidiEventVec::new(),
        }
    }

    /// SysEx messages dropped since this decoder was made; see
    /// [`Midi1Splitter::sysex_dropped`].
    pub fn sysex_dropped(&self) -> u64 {
        self.splitter.sysex_dropped()
    }

    /// Starts a new list of packets (a new callback): forgets events staged
    /// but not yet flushed, for a caller whose last list was cut short, and
    /// moves an open SysEx message to offset 0 (see
    /// [`Midi1Splitter::begin_list`]). The open message is kept.
    pub fn begin_list(&mut self) {
        self.batch.clear();
        self.splitter.begin_list();
    }

    /// Reads one packet's `bytes`, staging the events they make. Whenever the
    /// staging area is full, or a SysEx message needs more room than is left,
    /// `flush` gets what is staged first, in order. Call
    /// [`flush`](Self::flush) after the last packet of a list. Never allocates.
    pub fn decode(&mut self, bytes: &[u8], frame_offset: u32, flush: &mut impl FnMut(&[WireMidi])) {
        let Self { splitter, batch } = self;
        splitter.split(bytes, frame_offset, &mut |frame, message| match message {
            Midi1Message::Short(bytes) => {
                let Some(ump) = from_midi1_bytes(bytes) else {
                    return;
                };
                let event = WireMidi { offset: frame, ump };
                if batch.is_full() {
                    flush_batch(batch, flush);
                }
                // Room: the batch is not full, or was just flushed.
                let _ = batch.try_push(event);
            }
            Midi1Message::SysEx(message) => {
                let packets = sysex7_packet_count(message.len() - 2);
                if packets > batch.remaining_capacity() {
                    flush_batch(batch, flush);
                }
                // A message of at most `SYSEX_OUT_CAPACITY` bytes has at most
                // `MIDI_STACK_CAPACITY` packets: it fits an empty batch.
                let pushed = push_sysex_stack(batch, frame, message);
                debug_assert!(pushed, "a capped message fits an empty batch");
            }
        });
    }

    /// Hands on what is staged. Call it after a list's last packet.
    pub fn flush(&mut self, flush: &mut impl FnMut(&[WireMidi])) {
        flush_batch(&mut self.batch, flush);
    }
}

fn flush_batch(batch: &mut MidiEventVec, flush: &mut impl FnMut(&[WireMidi])) {
    if !batch.is_empty() {
        flush(batch);
        batch.clear();
    }
}

/// The MIDI 1.0 velocity byte for a MIDI 2.0 note-on's 16-bit velocity. A
/// velocity of zero stays zero (a note-off in MIDI 1.0, as in MIDI 2.0's own
/// reading of it); any other rounds to at least 1, because the 7-bit scale
/// would otherwise turn a soft note-on into a note-off.
pub fn note_on_velocity_midi1(velocity: u16) -> u8 {
    let v = tutti_midi_types::convert::midi2_velocity_to_midi1(velocity) & 0x7F;
    if velocity == 0 { 0 } else { v.max(1) }
}

/// How reading a short message's data bytes ended.
enum Gathered {
    /// Complete and delivered; reading resumes at this index.
    Done(usize),
    /// A status byte other than real-time stood where data belongs: the
    /// message is dropped (MIDI 1.0) and this byte starts the next one.
    Aborted(usize),
    /// The packet ended first: dropped.
    Cut,
}

/// Reads the `n` data bytes of the short message whose status is `msg[0]`,
/// from `bytes[j..]`, and delivers it. A real-time byte among them is
/// delivered at once and the data goes on, as MIDI 1.0 allows.
fn gather(
    bytes: &[u8],
    mut j: usize,
    msg: &mut [u8; 3],
    n: usize,
    frame_offset: u32,
    f: &mut impl FnMut(u32, Midi1Message<'_>),
) -> Gathered {
    let mut have = 1;
    while have <= n {
        let Some(&b) = bytes.get(j) else {
            return Gathered::Cut;
        };
        match b {
            0x00..=0x7F => {
                msg[have] = b;
                have += 1;
            }
            0xF8..=0xFF => f(frame_offset, Midi1Message::Short(&[b])),
            _ => return Gathered::Aborted(j),
        }
        j += 1;
    }
    f(frame_offset, Midi1Message::Short(&msg[..=n]));
    Gathered::Done(j)
}

/// Data-byte count for a channel-voice, system-common or real-time status,
/// or `None` for one this host does not deliver (the SysEx delimiters).
fn data_len(status: u8) -> Option<usize> {
    match status & 0xF0 {
        // Note off / on, poly pressure, control change, pitch bend.
        0x80 | 0x90 | 0xA0 | 0xB0 | 0xE0 => Some(2),
        // Program change, channel pressure.
        0xC0 | 0xD0 => Some(1),
        0xF0 => match status {
            // MTC quarter frame, song select.
            0xF1 | 0xF3 => Some(1),
            // Song position pointer.
            0xF2 => Some(2),
            // Tune request.
            0xF6 => Some(0),
            // System real-time.
            0xF8..=0xFF => Some(0),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SysExAssembler, SysExStep};
    use tutti_midi_types::{MidiMessage, to_midi1_bytes};
    use tutti_ump::frame;

    /// Decode each packet in turn, then flush; one `Vec` per flush call.
    fn run(decoder: &mut Midi1Decoder, packets: &[(&[u8], u32)]) -> Vec<Vec<WireMidi>> {
        let mut flushes = Vec::new();
        let mut sink = |events: &[WireMidi]| flushes.push(events.to_vec());
        for (bytes, frame) in packets {
            decoder.decode(bytes, *frame, &mut sink);
        }
        decoder.flush(&mut sink);
        flushes
    }

    fn all(flushes: &[Vec<WireMidi>]) -> Vec<WireMidi> {
        flushes.iter().flatten().copied().collect()
    }

    fn msg(e: &WireMidi) -> MidiMessage {
        MidiMessage::from_ump(e.ump)
    }

    fn payload(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 0x7F) as u8).collect()
    }

    fn framed(payload: &[u8]) -> Vec<u8> {
        frame(payload).collect()
    }

    /// The messages a stream of UMP packets reassembles to.
    fn reassemble(events: &[WireMidi]) -> Vec<Vec<u8>> {
        let mut asm = SysExAssembler::reserved();
        let mut out = Vec::new();
        for e in events {
            if let SysExStep::Complete(r) = asm.push(&e.ump) {
                out.push(asm.message(r).to_vec());
            }
        }
        out
    }

    /// **A SysEx of any length up to the capacity arrives whole**: the UMP
    /// packets reassemble to the exact bytes, in one flush.
    ///
    /// Mutation (run): in `decode`'s SysEx arm, push only the first packet (or cap
    /// the payload at six bytes) → the 40-byte message comes back short →
    /// fails.
    #[test]
    fn a_sysex_of_any_length_arrives_whole() {
        for len in [0, 1, 6, 7, 40, 255, SYSEX_OUT_CAPACITY - 2] {
            let message = framed(&payload(len));
            let mut decoder = Midi1Decoder::new();
            let flushes = run(&mut decoder, &[(&message, 5)]);
            assert_eq!(flushes.len(), 1, "{len} bytes in one batch");
            assert!(flushes[0].iter().all(|e| e.offset == 5));
            assert_eq!(reassemble(&flushes[0]), vec![message], "{len} bytes");
            assert_eq!(decoder.sysex_dropped(), 0);
        }
    }

    /// **A SysEx split over packets and calls is the same message**, stamped
    /// with the offset of the packet that held its `0xF0`.
    ///
    /// Mutation (run): reset `capture` to `Idle` at the start of `split` →
    /// the tail packets are skipped as stray data and nothing arrives →
    /// fails.
    #[test]
    fn a_sysex_spanning_packets_is_one_message() {
        let body = payload(30);
        let message = framed(&body);
        let (a, rest) = message.split_at(4);
        let (b, c) = rest.split_at(10);
        let mut decoder = Midi1Decoder::new();
        let flushes = run(&mut decoder, &[(a, 3), (b, 9), (c, 17)]);
        let events = all(&flushes);
        assert_eq!(reassemble(&events), vec![message.clone()]);
        assert!(events.iter().all(|e| e.offset == 3), "start offset");

        // Across a flush too: the list ended, the message did not.
        let mut decoder = Midi1Decoder::new();
        let first = run(&mut decoder, &[(a, 1)]);
        assert!(first.is_empty());
        let second = run(&mut decoder, &[(rest, 2)]);
        assert_eq!(reassemble(&all(&second)), vec![framed(&body)]);
    }

    /// **A message over the capacity is dropped whole and counted once**,
    /// its remaining bytes skipped, and the message after it is delivered.
    ///
    /// Mutation (run): in `take_data`, raise the limit by one byte → the
    /// over-long message is pushed (the debug assert fires) or, built
    /// without it, delivered cut → fails.
    #[test]
    fn a_message_past_capacity_is_dropped_whole() {
        let at_capacity = framed(&payload(SYSEX_OUT_CAPACITY - 2));
        let too_long = framed(&payload(SYSEX_OUT_CAPACITY - 1));
        let after = framed(&payload(9));
        let mut decoder = Midi1Decoder::new();
        let flushes = run(
            &mut decoder,
            &[(&too_long, 0), (&after, 0), (&at_capacity, 0)],
        );
        assert_eq!(
            reassemble(&all(&flushes)),
            vec![after, at_capacity],
            "no cut message, none lost after it"
        );
        assert_eq!(decoder.sysex_dropped(), 1);
    }

    /// **A SysEx cut by another status byte is dropped, counted, and the
    /// byte read as usual**: the next `0xF0` starts a message, a note-on is
    /// a note-on.
    ///
    /// Mutation (run): on a non-real-time status, keep collecting → the
    /// note-on's bytes and the second message fuse into one → fails.
    #[test]
    fn a_status_byte_ends_an_unfinished_sysex() {
        let mut decoder = Midi1Decoder::new();
        let flushes = run(
            &mut decoder,
            &[
                (&[0xF0, 1, 2, 3], 0),
                (&[0x90, 60, 100], 4),
                (&[0xF0, 1, 2], 5),
                (&[0xF0, 7, 0xF7], 6),
            ],
        );
        let events = all(&flushes);
        assert_eq!(decoder.sysex_dropped(), 2);
        assert!(msg(&events[0]).is_note_on());
        assert_eq!(events[0].offset, 4);
        assert_eq!(reassemble(&events[1..]), vec![vec![0xF0, 7, 0xF7]]);
        assert_eq!(events.len(), 2);
    }

    /// **A real-time byte inside a SysEx is delivered and the message
    /// goes on.**
    ///
    /// Mutation (run): treat `0xF8` like another status → the clock is
    /// delivered but the message is dropped and counted → fails.
    #[test]
    fn a_realtime_byte_does_not_end_a_sysex() {
        let mut decoder = Midi1Decoder::new();
        let flushes = run(&mut decoder, &[(&[0xF0, 1, 0xF8, 2, 0xF7], 0)]);
        let events = all(&flushes);
        assert_eq!(events.len(), 2);
        assert_eq!(
            from_midi1_bytes(&[0xF8]).unwrap(),
            events[0].ump,
            "the clock first"
        );
        assert_eq!(reassemble(&events[1..]), vec![vec![0xF0, 1, 2, 0xF7]]);
        assert_eq!(decoder.sysex_dropped(), 0);
    }

    /// **Other messages are read as before**: running status, several
    /// messages in a packet, a cut message dropped, and a stray data byte
    /// skipped.
    ///
    /// Mutation (run): clear `running` on a real-time byte → the second
    /// note after the clock is lost → fails.
    #[test]
    fn short_messages_are_split_as_midi1() {
        let mut decoder = Midi1Decoder::new();
        let flushes = run(
            &mut decoder,
            &[
                (&[0x90, 60, 100, 62, 90, 0xF8, 64, 80], 2),
                (&[0x05, 0xB0, 7], 3),
            ],
        );
        let events = all(&flushes);
        assert_eq!(
            events.len(),
            4,
            "three notes and the clock; the cut CC lost"
        );
        assert_eq!(msg(&events[0]).note(), Some(60));
        assert_eq!(msg(&events[1]).note(), Some(62));
        assert_eq!(msg(&events[3]).note(), Some(64));
        assert!(events.iter().all(|e| e.offset == 2 || e.offset == 3));
    }

    /// **A SysEx that does not fit the room left flushes what is staged
    /// first**, in order, and still arrives in one piece in one batch; a
    /// batch that fills flushes too.
    ///
    /// Mutation (run): drop the `packets > remaining_capacity` flush in
    /// `decode`'s SysEx arm → `push_sysex_stack` refuses the message (it asserts in
    /// debug) → fails.
    #[test]
    fn a_sysex_waits_for_room_in_the_batch() {
        let notes: Vec<[u8; 3]> = (0..MIDI_STACK_CAPACITY - 3)
            .map(|n| [0x90, (n % 100) as u8, 90])
            .collect();
        let big = framed(&payload(SYSEX_OUT_CAPACITY - 2));
        let mut decoder = Midi1Decoder::new();
        let mut packets: Vec<(&[u8], u32)> = notes.iter().map(|n| (&n[..], 0)).collect();
        packets.push((&big, 1));
        let flushes = run(&mut decoder, &packets);
        assert_eq!(flushes.len(), 2);
        assert_eq!(flushes[0].len(), notes.len(), "the notes, before the dump");
        assert!(flushes[0].iter().all(|e| msg(e).is_note_on()));
        assert_eq!(reassemble(&flushes[1]), vec![big]);

        // More short messages than a batch holds flush as it fills.
        let mut decoder = Midi1Decoder::new();
        let many: Vec<(&[u8], u32)> = (0..MIDI_STACK_CAPACITY + 10)
            .map(|_| (&[0xF8u8][..], 0))
            .collect();
        let flushes = run(&mut decoder, &many);
        assert_eq!(
            flushes.iter().map(Vec::len).collect::<Vec<_>>(),
            vec![MIDI_STACK_CAPACITY, 10]
        );
    }

    /// **A soft MIDI 2.0 note-on is not turned into a note-off.**
    ///
    /// Mutation (run): drop `.max(1)` → 0x0001 maps to 0 → fails.
    #[test]
    fn a_soft_note_on_keeps_a_nonzero_velocity() {
        assert_eq!(note_on_velocity_midi1(0), 0);
        for v in [1u16, 0x01FF, 0x0200] {
            assert!(note_on_velocity_midi1(v) >= 1, "{v:#x}");
        }
        assert_eq!(note_on_velocity_midi1(u16::MAX), 127);
    }

    /// **A SysEx that goes on into a later list is stamped at offset 0**, the
    /// start of the block that completes it, not at the earlier block's offset.
    ///
    /// Mutation (run): make `begin_list` a no-op on the splitter → the packets
    /// carry offset 40 → fails.
    #[test]
    fn a_sysex_continued_into_a_later_list_lands_at_the_start() {
        let mut decoder = Midi1Decoder::new();
        let mut out = Vec::new();
        let mut sink = |events: &[WireMidi]| out.extend_from_slice(events);
        decoder.begin_list();
        decoder.decode(&[0xF0, 1, 2], 40, &mut sink);
        decoder.flush(&mut sink);
        decoder.begin_list();
        decoder.decode(&[3, 0xF7], 9, &mut sink);
        decoder.flush(&mut sink);
        assert_eq!(reassemble(&out), vec![vec![0xF0, 1, 2, 3, 0xF7]]);
        assert!(out.iter().all(|e| e.offset == 0));

        // Within one list the start's offset stands.
        let mut out = Vec::new();
        let mut sink = |events: &[WireMidi]| out.extend_from_slice(events);
        decoder.begin_list();
        decoder.decode(&[0xF0, 1], 40, &mut sink);
        decoder.decode(&[2, 0xF7], 41, &mut sink);
        decoder.flush(&mut sink);
        assert!(out.iter().all(|e| e.offset == 40));
    }

    /// **A status byte among a short message's data ends it**, a real-time one
    /// excepted: `90 3C F8 40` is a clock and a note-on, `90 3C B0 07 64` drops
    /// the cut note-on and reads the control change.
    ///
    /// Mutation (run): take any byte as data in `gather` (drop the `_ =>
    /// Aborted` arm's check) → the first message is `90 3C B0`, and the
    /// control change is lost → fails.
    #[test]
    fn a_status_byte_in_a_short_messages_data_ends_it() {
        let mut decoder = Midi1Decoder::new();
        let flushes = run(
            &mut decoder,
            &[
                (&[0x90, 0x3C, 0xF8, 0x40], 0),
                (&[0x90, 0x3C, 0xB0, 0x07, 0x64], 1),
            ],
        );
        let events = all(&flushes);
        let bytes: Vec<_> = events
            .iter()
            .map(|e| to_midi1_bytes(&e.ump).map(|(b, n)| b[..n as usize].to_vec()))
            .collect();
        assert_eq!(
            bytes,
            vec![
                Some(vec![0xF8]),
                Some(vec![0x90, 0x3C, 0x40]),
                Some(vec![0xB0, 0x07, 0x64]),
            ]
        );

        // Running status: the data position holds a status.
        let mut decoder = Midi1Decoder::new();
        let flushes = run(&mut decoder, &[(&[0x90, 60, 100, 62, 0x80, 62, 0], 0)]);
        let notes: Vec<_> = all(&flushes).iter().map(|e| msg(e).is_note_on()).collect();
        assert_eq!(notes, vec![true, false], "the cut note-on 62 is dropped");
    }

    /// **Decoding never allocates**, a maximal SysEx included.
    ///
    /// Mutation (run): build the message in a fresh `Vec` in `end_sysex` →
    /// the gate panics → fails.
    #[test]
    fn decoding_does_not_allocate() {
        let big = framed(&payload(SYSEX_OUT_CAPACITY - 2));
        let too_long = framed(&payload(SYSEX_OUT_CAPACITY - 1));
        let mut decoder = Midi1Decoder::new();
        let mut seen = 0usize;
        assert_no_alloc::assert_no_alloc(|| {
            let mut sink = |events: &[WireMidi]| seen += events.len();
            for bytes in [&big[..], &too_long, &[0x90, 60, 100], &big, &[0xF0, 1]] {
                decoder.decode(bytes, 0, &mut sink);
            }
            decoder.flush(&mut sink);
        });
        assert_eq!(seen, 2 * MIDI_STACK_CAPACITY + 1);
        assert_eq!(decoder.sysex_dropped(), 1);
    }
}
