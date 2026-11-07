//! SysEx 7-bit: single-packet and multi-packet fragmentation (UMP type 0x3).
//!
//! Like [`super::sysex8`], the emit side leans on midi2's [`midi2::sysex7::Sysex7`]
//! rather than hand-packing words: build one growable message with the full 7-bit
//! payload and split its words into 2-word [`MidiEvent`]s — midi2 owns the
//! Start/Continue/End fragmentation and the status-nibble/count assignment. The
//! decode side keeps a direct first-word reader ([`MidiEvent::sysex7_payload`])
//! because the [`Sysex7PacketReassembler`](crate) + CI/VST3 callers want the raw
//! `(status, [u8; 6], n)` per-packet view, not a whole-message payload iterator.

use std::vec::Vec;
use tutti_ump::MidiGroup;

use midi2::prelude::*;
use midi2::sysex7::Sysex7;

use super::MidiEvent;

/// SysEx7 status nibble: the whole message fits one packet, so there is nothing
/// to reassemble.
pub const SYSEX7_STATUS_SINGLE: u8 = 0x0;
/// SysEx7 status nibble: first packet of a multi-packet message.
pub const SYSEX7_STATUS_START: u8 = 0x1;
/// SysEx7 status nibble: a middle packet of a multi-packet message.
pub const SYSEX7_STATUS_CONTINUE: u8 = 0x2;
/// SysEx7 status nibble: last packet of a multi-packet message.
pub const SYSEX7_STATUS_END: u8 = 0x3;

/// The longest SysEx payload (the bytes between `0xF0` and `0xF7`) the engine
/// carries: the hardware input abandons a longer dump, and the plugin hosts
/// reserve room to reassemble one this long. A path's event caps can be
/// lower: a plugin node takes at most 256 MIDI events a chunk (a device
/// callback; a block for the in-process VST2 node), so a dump whose packets
/// arrive in one block (as the hardware input's do) reaches a plugin only up
/// to 1536 payload bytes.
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
/// this is a fault, not a big message.
pub const SYSEX7_MAX_PAYLOAD: usize = 64 * 1024;

impl MidiEvent {
    /// Builds SysEx 7-bit packets (UMP type 0x3, 64-bit each) for `data` and
    /// pushes them onto `out`. `data` is the payload *between* 0xF0 and 0xF7
    /// (no delimiters). Payloads ≤ 6 bytes produce a single packet; longer
    /// payloads produce `Start` + `Continue*` + `End` — midi2 owns the split.
    pub fn sysex7_fragments(group: MidiGroup, data: &[u8], out: &mut Vec<MidiEvent>) {
        let mut m = Sysex7::<Vec<u32>>::new();
        m.set_group(u4::new(group.get()));
        m.set_payload(data.iter().map(|&b| u7::new(b & 0x7F)));
        // Each SysEx7 packet is 2 words; split the message stream into events.
        for packet in m.data().chunks(2) {
            out.push(MidiEvent::from_ump(0, packet));
        }
    }

    /// How many SysEx 7-bit packets a payload of `payload_len` bytes (the
    /// data between 0xF0 and 0xF7) takes: six bytes to a packet, and an empty
    /// payload is one empty `SINGLE`. The count [`Self::sysex7_packets`]
    /// yields.
    pub const fn sysex7_packet_count(payload_len: usize) -> usize {
        let packets = payload_len.div_ceil(6);
        if packets == 0 { 1 } else { packets }
    }

    /// The SysEx 7-bit packets (UMP type 0x3) for `data`, one at a time and
    /// without allocating: the same packets, in the same order, as
    /// [`Self::sysex7_fragments`] pushes. `data` is the payload *between* 0xF0
    /// and 0xF7 (no delimiters); a byte's high bit is dropped. An empty
    /// payload is one empty `SINGLE` packet. Every packet carries frame
    /// offset 0; stamp them with [`Self::with_frame_offset`].
    ///
    /// For the audio thread, where [`Self::sysex7_fragments`]' `Vec` is not
    /// available.
    pub fn sysex7_packets(group: MidiGroup, data: &[u8]) -> impl Iterator<Item = MidiEvent> + '_ {
        let packets = Self::sysex7_packet_count(data.len());
        (0..packets).map(move |i| {
            let chunk = &data[(i * 6).min(data.len())..((i + 1) * 6).min(data.len())];
            let status = match (packets, i) {
                (1, _) => SYSEX7_STATUS_SINGLE,
                (_, 0) => SYSEX7_STATUS_START,
                (n, i) if i == n - 1 => SYSEX7_STATUS_END,
                _ => SYSEX7_STATUS_CONTINUE,
            };
            let b = |k: usize| u32::from(chunk.get(k).copied().unwrap_or(0) & 0x7F);
            let w0 = (0x3 << 28)
                | (u32::from(group.get() & 0x0F) << 24)
                | (u32::from(status) << 20)
                | ((chunk.len() as u32) << 16)
                | (b(0) << 8)
                | b(1);
            let w1 = (b(2) << 24) | (b(3) << 16) | (b(4) << 8) | b(5);
            MidiEvent::from_ump(0, &[w0, w1])
        })
    }

    /// Builds a single self-contained SysEx 7-bit packet (UMP type 0x3) from a
    /// payload of up to 6 bytes (the data *between* 0xF0 and 0xF7, no
    /// delimiters; a byte's high bit is dropped). Returns `None` if the payload
    /// exceeds one packet — use [`Self::sysex7_fragments`] for longer messages.
    /// Never allocates.
    pub fn sysex7_single(group: MidiGroup, payload: &[u8]) -> Option<Self> {
        if payload.len() > 6 {
            return None;
        }
        // A ≤6-byte payload is one `SINGLE` packet.
        Self::sysex7_packets(group, payload).next()
    }

    /// Decodes a single SysEx 7-bit packet (UMP type 0x3) into its
    /// `(status, payload)` — the inverse of [`Self::sysex7_fragments`]. Returns the
    /// status nibble ([`SYSEX7_STATUS_SINGLE`]/`START`/`CONTINUE`/`END`) and the
    /// up-to-6 payload bytes (no 0xF0/0xF7 delimiters). `None` for any event
    /// that isn't a type-0x3 packet, or one whose declared length exceeds 6.
    ///
    /// Reassembling a multi-packet SysEx stream is the caller's job — this
    /// decodes one packet, mirroring how [`Self::sysex7_fragments`] emits them.
    pub fn sysex7_payload(&self) -> Option<(u8, [u8; 6], usize)> {
        let w0 = self.data[0];
        if (w0 >> 28) & 0x0F != 0x3 {
            return None;
        }
        let status = ((w0 >> 20) & 0x0F) as u8;
        let n = ((w0 >> 16) & 0x0F) as usize;
        if n > 6 {
            return None;
        }
        let w1 = self.data[1];
        let mut out = [0u8; 6];
        for (i, slot) in out.iter_mut().enumerate().take(n) {
            *slot = match i {
                0 => (w0 >> 8) & 0x7F,
                1 => w0 & 0x7F,
                2 => (w1 >> 24) & 0x7F,
                3 => (w1 >> 16) & 0x7F,
                4 => (w1 >> 8) & 0x7F,
                5 => w1 & 0x7F,
                _ => unreachable!(),
            } as u8;
        }
        Some((status, out, n))
    }
}

#[cfg(test)]
mod tests;
