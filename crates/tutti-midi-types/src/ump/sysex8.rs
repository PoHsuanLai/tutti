//! SysEx 8-bit (UMP Message Type 0x5): 128-bit packets carrying 8-bit-clean
//! System Exclusive / Mixed Data payloads (M2-104 §7.8).
//!
//! Unlike SysEx7 (MT 0x3), which [`super::sysex`] hand-packs, SysEx8 leans
//! on midi2's [`midi2::sysex8::Sysex8`]: each packet holds up to 13 payload
//! bytes plus a `stream_id` that ties a multi-packet message together, and midi2
//! owns the Start/Continue/End fragmentation internally. So the emit side builds
//! one growable message and splits its words into 4-word [`MidiEvent`]s (like the
//! UMP-Stream text helpers), and the decode side hands midi2 the reassembled
//! word slice.
//!
//! "Mixed Data Set" (statuses 0x8 Header / 0x9 Payload) is the *other* MT-0x5
//! sub-form, for bulk data streaming. It is **not** covered here: it has its own
//! statuses and an `mds id` field where SysEx8 has a Stream ID, so `Sysex8` is
//! not "the whole surface" of MT 0x5. Unimplemented rather than unnecessary —
//! see the tracking issue.

use std::vec::Vec;
use tutti_ump::MidiGroup;

use midi2::prelude::*;

use super::MidiEvent;

/// SysEx8 status nibble: a single self-contained packet.
pub const SYSEX8_STATUS_SINGLE: u8 = 0x0;
/// SysEx8 status nibble: first packet of a multi-packet message.
pub const SYSEX8_STATUS_START: u8 = 0x1;
/// SysEx8 status nibble: a middle packet of a multi-packet message.
pub const SYSEX8_STATUS_CONTINUE: u8 = 0x2;
/// SysEx8 status nibble: last packet of a multi-packet message.
pub const SYSEX8_STATUS_END: u8 = 0x3;

/// The `# of bytes` value that marks an **aborted** SysEx8 message.
///
/// M2-104 §7.8: "The special value 0xF is used in an End UMP to abort a System
/// Exclusive 8 message." §7.8.1 defines what that means: "the previous data is
/// an incomplete message, or the resulting quality of previous data is unknown."
/// A reader that ignores this accepts corrupt data as good.
pub const SYSEX8_BYTES_ABORT: u8 = 0xF;

impl MidiEvent {
    /// Builds SysEx 8-bit packets (UMP MT 0x5, 128-bit each) carrying `data`, and
    /// pushes them onto `out`. `data` is the raw 8-bit payload (no 0xF0/0xF7
    /// delimiters — SysEx8 is delimiter-free). `stream_id` groups the packets of
    /// one logical message so an interleaved second SysEx8 stream stays distinct.
    /// Payloads ≤ 13 bytes produce a single packet; longer payloads are
    /// fragmented into Start + Continue* + End by midi2.
    pub fn sysex8_fragments(
        group: MidiGroup,
        stream_id: u8,
        data: &[u8],
        out: &mut Vec<MidiEvent>,
    ) {
        use midi2::sysex8::Sysex8;
        let mut m = Sysex8::<Vec<u32>>::new();
        m.set_group(u4::new(group.get()));
        m.set_payload(data.iter().copied());
        // The stream id lives in octet 2 of each packet's first word (bits 8..15),
        // right after the [type|group][status|count] byte and before the first
        // data byte in octet 3. midi2 v0.11's `set_stream_id` writes octet 3
        // instead (clobbering data0), so patch octet 2 ourselves per packet.
        let sid = ((stream_id as u32) << 8) & 0x0000_FF00;
        for packet in m.data().chunks(4) {
            let mut ev = MidiEvent::from_ump(0, packet);
            ev.data[0] = (ev.data[0] & !0x0000_FF00) | sid;
            out.push(ev);
        }
    }

    /// Reads the `(status, stream_id)` of a single SysEx8 packet (UMP MT 0x5)
    /// directly from its first word, without a full midi2 decode. `None` for any
    /// event that isn't a type-0x5 packet. Use [`sysex8_message`] to recover the
    /// reassembled payload of a whole (possibly multi-packet) message.
    pub fn sysex8_status(&self) -> Option<(u8, u8)> {
        self.sysex8_header()
            .map(|(status, stream_id, _)| (status, stream_id))
    }

    /// Reads `(status, stream_id, byte_count)` from a SysEx8 packet's first word.
    /// `None` for any event that isn't a type-0x5 packet.
    ///
    /// The byte count is what [`sysex8_status`](Self::sysex8_status) omits, and
    /// it is load-bearing: a value of [`SYSEX8_BYTES_ABORT`] in an End packet
    /// means the message was aborted, not completed.
    pub fn sysex8_header(&self) -> Option<(u8, u8, u8)> {
        let w0 = self.data[0];
        if (w0 >> 28) & 0x0F != 0x5 {
            return None;
        }
        // Word 0 layout (M2-104 §7.8): nibble0 = type(0x5), nibble1 = group,
        // nibble2 = status, nibble3 = byte count, octet2 (bits 8..15) = stream_id,
        // octet3 (bits 0..7) = first data byte.
        let status = ((w0 >> 20) & 0x0F) as u8;
        let byte_count = ((w0 >> 16) & 0x0F) as u8;
        let stream_id = ((w0 >> 8) & 0xFF) as u8;
        Some((status, stream_id, byte_count))
    }

    /// Whether this packet aborts its SysEx8 message: an End UMP whose
    /// `# of bytes` field is [`SYSEX8_BYTES_ABORT`] (M2-104 §7.8.1).
    #[must_use]
    pub fn is_sysex8_abort(&self) -> bool {
        matches!(
            self.sysex8_header(),
            Some((SYSEX8_STATUS_END, _, SYSEX8_BYTES_ABORT))
        )
    }
}

/// Reassembles a SysEx8 message from the packets in `events` (in order) into its
/// `(stream_id, payload)`, or `None` if the events don't form one valid SysEx8
/// message. The inverse of [`MidiEvent::sysex8_fragments`]: pass back the same
/// events (a `Single` packet, or a `Start … End` run sharing one `stream_id`).
///
/// midi2 owns the fragmentation rules, so this flattens each event's meaningful
/// words into one contiguous buffer and decodes the whole message at once.
pub fn sysex8_message(events: &[MidiEvent]) -> Option<(u8, Vec<u8>)> {
    use midi2::sysex8::Sysex8;

    if events.is_empty() {
        return None;
    }
    // SysEx8 packets are always 4 words; concatenate them for midi2 to parse.
    let mut words = Vec::with_capacity(events.len() * 4);
    for ev in events {
        if (ev.data[0] >> 28) & 0x0F != 0x5 {
            return None;
        }
        words.extend_from_slice(&ev.data);
    }
    let m = Sysex8::try_from(&words[..]).ok()?;
    let payload: Vec<u8> = m.payload().collect();
    // Stream id is octet 2 (bits 8..15) of the first packet — the octet
    // `sysex8_fragments` writes and midi2's own reader reads (its *writer* is the
    // buggy half, which is why the emit path patches the word rather than
    // calling set_stream_id).
    let stream_id = ((words[0] >> 8) & 0xFF) as u8;
    Some((stream_id, payload))
}

#[cfg(test)]
mod tests;
