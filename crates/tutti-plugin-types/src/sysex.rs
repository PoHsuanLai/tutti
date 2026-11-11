//! SysEx for delivery to and from a plugin.
//!
//! The engine carries SysEx as UMP SysEx7 packets of at most six bytes: a
//! lone `Complete`, or `Start`, `Continue`*, `End`. Every plugin format wants
//! one complete message, `0xF0 … 0xF7`, behind a pointer the plugin may read
//! for the whole `process` call. The reassembler is `tutti_ump::SysexAssembler`;
//! [`sysex_assembler`] builds the one every plugin path uses, over a buffer
//! sized off the audio thread and never grown, so reassembly does not
//! allocate and a pointer handed to the plugin is never moved during the block
//! it was handed out in. The other direction, a plugin's message to packets,
//! is [`push_sysex`].

use tutti_ump::{MidiGroup, SYSEX7_MAX_PAYLOAD, Sysex7Packets, SysexAssembler};

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
/// together, into one block. See `tutti_ump::SysexAssembler`'s policy.
pub const SYSEX_MESSAGE_CAPACITY: usize = SYSEX7_MAX_PAYLOAD + 2;

/// The assembler every plugin input path uses: `tutti_ump::SysexAssembler`
/// over a boxed buffer of [`SysexAssembler::room_for`]
/// [`SYSEX_MESSAGE_CAPACITY`] bytes, a [`MIDI_STACK_CAPACITY`] block of
/// packets and one lane (about 132 KiB): a plugin format takes one stream of
/// whole messages, so one message is open at a time.
///
/// Off the audio thread: it allocates the buffer once, and the assembler
/// never grows it, so every call on the audio thread is allocation-free.
#[must_use]
pub fn sysex_assembler() -> SysexAssembler<Box<[u8]>> {
    let room = SysexAssembler::room_for(SYSEX_MESSAGE_CAPACITY, MIDI_STACK_CAPACITY, 1);
    SysexAssembler::new(vec![0; room].into_boxed_slice(), SYSEX_MESSAGE_CAPACITY, 1)
}

/// A SysEx message without its `0xF0` and `0xF7` delimiters, if it has them:
/// the payload UMP SysEx7 carries.
fn unframed(message: &[u8]) -> &[u8] {
    let payload = message.strip_prefix(&[0xF0]).unwrap_or(message);
    payload.strip_suffix(&[0xF7]).unwrap_or(payload)
}

/// The SysEx7 packets (on the first group) of a plugin's `message`, with its
/// `0xF0` and `0xF7` delimiters or without (the one place the formats'
/// unchecked byte slices meet [`Sysex7Packets::checked`]; a message known to be
/// framed is a `tutti_ump::SysexMessage` and has its own `packets`), or `None` if a payload byte is 128
/// or more: a plugin emitted a malformed message, which the caller drops
/// (nothing is masked into a different message).
#[must_use]
pub fn sysex_packets(message: &[u8]) -> Option<Sysex7Packets<'_>> {
    Sysex7Packets::checked(MidiGroup::FIRST, unframed(message))
}

/// Pushes one complete SysEx message a plugin emitted onto `out` as UMP
/// SysEx7 packets (a lone `SINGLE`, or `START`, `CONTINUE`*, `END`), every
/// packet at `frame_offset`, the inverse of what [`SysexAssembler`] does on
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

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_ump::{MidiChannel, SysexStep, Ump, frame};

    fn packets(group: MidiGroup, payload: &[u8]) -> Vec<Ump> {
        Sysex7Packets::checked(group, payload)
            .expect("a 7-bit payload")
            .collect()
    }

    fn framed(payload: &[u8]) -> Vec<u8> {
        frame(payload).collect()
    }

    /// Feed `events`, collecting each completed message's bytes.
    fn feed(asm: &mut SysexAssembler<Box<[u8]>>, events: &[Ump]) -> Vec<Vec<u8>> {
        let mut done = Vec::new();
        for &e in events {
            if let SysexStep::Complete(r) = asm.push(e) {
                done.push(asm.message(r).bytes().to_vec());
            }
        }
        done
    }

    /// **The plugin assembler is sized for the longest message carried across
    /// blocks of a full MIDI cap each, without allocating**, and its buffer's
    /// address never moves. (The assembler's own policy is
    /// `tutti-ump`'s `sysex_assembler` suite; this pins the size this crate
    /// gives it.)
    ///
    /// Mutation (run): the room computed as `SYSEX_MESSAGE_CAPACITY` alone (no
    /// block of packets on top) → a single beside the carried run does not
    /// fit → counted → fails.
    #[test]
    fn the_longest_message_fits_without_allocating() {
        let body = tutti_fixtures::midi::sysex_payload(SYSEX7_MAX_PAYLOAD);
        let pkts = packets(MidiGroup::FIRST, &body);
        let mut asm = sysex_assembler();
        assert_eq!(asm.max_message(), SYSEX_MESSAGE_CAPACITY);
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
                for &p in chunk {
                    if let SysexStep::Complete(r) = asm.push(p) {
                        done += 1;
                        assert_eq!(r.len(), SYSEX_MESSAGE_CAPACITY);
                    }
                }
            }
            for &s in &singles[..MIDI_STACK_CAPACITY / 2] {
                if let SysexStep::Complete(_) = asm.push(s) {
                    done += 1;
                }
            }
        });
        assert_eq!(asm.dropped(), 0);
        assert_eq!(done, 1 + MIDI_STACK_CAPACITY / 2);
        assert_eq!(asm.bytes().as_ptr(), base, "the buffer never moved");
    }

    /// **A plugin's SysEx of any length reaches the pool intact**, framed or
    /// bare, and reassembles to the message it was.
    ///
    /// Mutation (run): in `push_sysex`, keep only the first packet (or cap
    /// the payload at 6 bytes) → the 40-byte message comes back short →
    /// fails.
    #[test]
    fn a_plugins_sysex_of_any_length_arrives_whole() {
        let mut asm = sysex_assembler();
        for len in [0, 1, 6, 7, 14, 17, 40, 63 * 6] {
            let body = tutti_fixtures::midi::sysex_payload(len);
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

    /// **`sysex_packets` takes a message with or without its delimiters and
    /// refuses a payload byte of 128 or more**, whichever way it is framed.
    ///
    /// Mutation (run): `Sysex7Packets::checked` replaced by an unchecked
    /// `over` (masking) → the 8-bit payloads are accepted → fails; `unframed`
    /// a no-op → the delimiters are packed as payload → the 0xF7 is refused
    /// as 8-bit → fails.
    #[test]
    fn sysex_packets_takes_either_framing_and_refuses_8_bit_bytes() {
        let body = [1, 2, 3];
        let want: Vec<Ump> = packets(MidiGroup::FIRST, &body);
        for message in [&[0xF0, 1, 2, 3, 0xF7][..], &[1, 2, 3][..]] {
            assert_eq!(sysex_packets(message).unwrap().collect::<Vec<_>>(), want);
        }
        assert!(sysex_packets(&[0xF0, 1, 0x80, 0xF7]).is_none());
        assert!(sysex_packets(&[1, 0x80]).is_none());
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
        let too_long = tutti_fixtures::midi::sysex_payload(room * 6 + 1);
        assert!(!push_sysex(&mut out, 0, &too_long));
        assert!(out.is_empty(), "no partial message");
        assert!(out.is_overflowed());

        // What fits after another event is judged on the room that is left.
        let mut out = RtMidiEvents::new();
        assert!(out.push(WireMidi {
            offset: 0,
            ump: tutti_fixtures::midi::note_on(
                MidiGroup::FIRST,
                MidiChannel::of(0),
                60,
                tutti_fixtures::midi::MID_VELOCITY
            )
        }));
        assert!(!push_sysex(
            &mut out,
            0,
            &tutti_fixtures::midi::sysex_payload((room - 1) * 6 + 1)
        ));
        assert_eq!(out.len(), 1);
        assert!(push_sysex(
            &mut out,
            0,
            &tutti_fixtures::midi::sysex_payload((room - 1) * 6)
        ));
        assert_eq!(out.len(), room);
    }
}
