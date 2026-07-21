//! SysEx 7-bit: single-packet and multi-packet fragmentation (UMP type 0x3).

use std::vec::Vec;

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
    /// payloads produce `Start` + `Continue*` + `End`.
    pub fn sysex7_fragments(group: u8, data: &[u8], out: &mut Vec<MidiEvent>) {
        if data.len() <= 6 {
            out.push(Self::sysex7_packet(group, SYSEX7_STATUS_SINGLE, data));
            return;
        }
        let total = data.len();
        let mut emitted = 0usize;
        let mut chunks = data.chunks(6);
        let first = chunks.next().unwrap_or(&[]);
        emitted += first.len();
        out.push(Self::sysex7_packet(group, SYSEX7_STATUS_START, first));
        for chunk in chunks {
            emitted += chunk.len();
            let status = if emitted >= total {
                SYSEX7_STATUS_END
            } else {
                SYSEX7_STATUS_CONTINUE
            };
            out.push(Self::sysex7_packet(group, status, chunk));
        }
    }

    /// Build a single self-contained SysEx 7-bit packet (UMP type 0x3) from a
    /// payload of up to 6 bytes (the data *between* 0xF0 and 0xF7, no
    /// delimiters). Returns `None` if the payload exceeds one packet — use
    /// [`Self::sysex7_fragments`] for longer messages. The no-`alloc`
    /// single-packet counterpart to `sysex7_fragments`.
    pub fn sysex7_single(group: u8, payload: &[u8]) -> Option<Self> {
        if payload.len() > 6 {
            return None;
        }
        Some(Self::sysex7_packet(group, SYSEX7_STATUS_SINGLE, payload))
    }

    /// Decode a single SysEx 7-bit packet (UMP type 0x3) into its
    /// `(status, payload)` — the inverse of [`Self::sysex7_packet`]. Returns the
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

    fn sysex7_packet(group: u8, status: u8, payload: &[u8]) -> Self {
        debug_assert!(payload.len() <= 6);
        let n = payload.len() as u8;
        let mut w0 = (0x3u32 << 28)
            | (((group & 0x0F) as u32) << 24)
            | (((status & 0x0F) as u32) << 20)
            | (((n & 0x0F) as u32) << 16);
        let mut w1 = 0u32;
        for (i, &b) in payload.iter().enumerate() {
            let byte = (b & 0x7F) as u32;
            match i {
                0 => w0 |= byte << 8,
                1 => w0 |= byte,
                2 => w1 |= byte << 24,
                3 => w1 |= byte << 16,
                4 => w1 |= byte << 8,
                5 => w1 |= byte,
                _ => unreachable!(),
            }
        }
        Self::from_ump(0, &[w0, w1])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sysex7_single_packet() {
        let mut out = Vec::new();
        MidiEvent::sysex7_fragments(0, &[0x7E, 0x7F, 0x06, 0x01], &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].data_words().len(), 2);
    }

    #[test]
    fn sysex7_multi_packet() {
        let mut out = Vec::new();
        let data: [u8; 15] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
        MidiEvent::sysex7_fragments(0, &data, &mut out);
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
            let ev = MidiEvent::sysex7_single(0, payload).expect("fits one packet");
            let (status, bytes, n) = ev.sysex7_payload().expect("decodes");
            assert_eq!(status, SYSEX7_STATUS_SINGLE);
            assert_eq!(n, payload.len());
            assert_eq!(&bytes[..n], payload);
        }
        // 7 bytes is more than one packet.
        assert!(MidiEvent::sysex7_single(0, &[0; 7]).is_none());
    }
}
