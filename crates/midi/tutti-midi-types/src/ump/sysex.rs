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
use tutti_types::MidiGroup;

use midi2::prelude::*;
use midi2::sysex7::Sysex7;

use super::MidiEvent;

pub const SYSEX7_STATUS_SINGLE: u8 = 0x0;
/// SysEx7 status nibble: first packet of a multi-packet message.
pub const SYSEX7_STATUS_START: u8 = 0x1;
/// SysEx7 status nibble: a middle packet of a multi-packet message.
pub const SYSEX7_STATUS_CONTINUE: u8 = 0x2;
/// SysEx7 status nibble: last packet of a multi-packet message.
pub const SYSEX7_STATUS_END: u8 = 0x3;

impl MidiEvent {
    /// Build SysEx 7-bit packets (UMP type 0x3, 64-bit each) for `data` and
    /// push them onto `out`. `data` is the payload *between* 0xF0 and 0xF7
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

    /// Build a single self-contained SysEx 7-bit packet (UMP type 0x3) from a
    /// payload of up to 6 bytes (the data *between* 0xF0 and 0xF7, no
    /// delimiters). Returns `None` if the payload exceeds one packet — use
    /// [`Self::sysex7_fragments`] for longer messages.
    pub fn sysex7_single(group: MidiGroup, payload: &[u8]) -> Option<Self> {
        if payload.len() > 6 {
            return None;
        }
        let mut m = Sysex7::<Vec<u32>>::new();
        m.set_group(u4::new(group.get()));
        m.set_payload(payload.iter().map(|&b| u7::new(b & 0x7F)));
        // A ≤6-byte payload is a single 2-word packet.
        Some(MidiEvent::from_ump(0, &m.data()[..2]))
    }

    /// Decode a single SysEx 7-bit packet (UMP type 0x3) into its
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
mod tests {
    use super::*;

    #[test]
    fn sysex7_single_packet() {
        let mut out = Vec::new();
        MidiEvent::sysex7_fragments(MidiGroup::FIRST, &[0x7E, 0x7F, 0x06, 0x01], &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].data_words().len(), 2);
    }

    #[test]
    fn sysex7_multi_packet() {
        let mut out = Vec::new();
        let data: [u8; 15] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
        MidiEvent::sysex7_fragments(MidiGroup::FIRST, &data, &mut out);
        // 15 bytes / 6 per packet = 3 packets
        assert_eq!(out.len(), 3);
        for ev in &out {
            assert_eq!(ev.data_words().len(), 2);
        }
    }

    #[test]
    fn sysex7_single_payload_round_trips() {
        for payload in [
            &[][..],
            &[0x42][..],
            &[0x7E, 0x7F, 0x06, 0x01][..],
            &[1, 2, 3, 4, 5, 6][..],
        ] {
            let ev = MidiEvent::sysex7_single(MidiGroup::FIRST, payload).expect("fits one packet");
            let (status, bytes, n) = ev.sysex7_payload().expect("decodes");
            assert_eq!(status, SYSEX7_STATUS_SINGLE);
            assert_eq!(n, payload.len());
            assert_eq!(&bytes[..n], payload);
        }
        // 7 bytes is more than one packet.
        assert!(MidiEvent::sysex7_single(MidiGroup::FIRST, &[0; 7]).is_none());
    }
}
