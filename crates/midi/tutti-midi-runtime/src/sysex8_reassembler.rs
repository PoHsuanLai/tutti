//! Streaming reassembly of multi-packet SysEx8 UMP messages.
//!
//! The SysEx8 counterpart to [`Sysex7Reassembler`](crate::Sysex7Reassembler),
//! and deliberately *not* the same shape, because M2-104 gives the two opposite
//! interleaving rules:
//!
//! - SysEx7 (§7.7.1) **forbids** other same-group traffic between Start and End,
//!   so one in-flight buffer per group is enough.
//! - SysEx8 (§7.8.1) explicitly **permits** it — "there are no prohibitions
//!   against interspersing other message UMPs, as there are with the 7-bit
//!   System Exclusive Messages" — and §7.8 says "Interleaving of multiple
//!   simultaneous System Exclusive 8 messages is enabled by use of an 8-bit
//!   Stream ID field."
//!
//! So a SysEx8 buffer is keyed on `(group, stream_id)`: two messages can be in
//! flight on the same group at once, and only the Stream ID tells them apart.
//! Buffering per group alone would concatenate them into one corrupt payload.

use tutti_midi_types::ump::{
    sysex8_message, MidiEvent, UmpMessageType, SYSEX8_STATUS_CONTINUE, SYSEX8_STATUS_END,
    SYSEX8_STATUS_SINGLE, SYSEX8_STATUS_START,
};

/// Default cap on a reassembled SysEx8 payload, in bytes. See
/// [`Sysex8Reassembler::with_max_bytes`].
pub const DEFAULT_MAX_SYSEX8_BYTES: usize = 4096;

/// SysEx8 carries up to 13 payload bytes per packet (M2-104 §7.8: 14 bytes from
/// the Stream ID onward, of which the Stream ID itself is one).
const SYSEX8_BYTES_PER_PACKET: usize = 13;

/// Why a SysEx8 run ended without a usable payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sysex8Abort {
    /// The sender aborted it: an End UMP with `# of bytes` = 0xF (§7.8.1), i.e.
    /// "the previous data is an incomplete message, or the resulting quality of
    /// previous data is unknown".
    SenderAborted,
    /// The run exceeded the reassembler's byte cap and was dropped.
    TooLong,
}

/// One in-flight SysEx8 message, identified by its group *and* stream id.
#[derive(Clone, Debug)]
struct InFlight {
    group: u8,
    stream_id: u8,
    packets: Vec<MidiEvent>,
}

/// What [`Sysex8Reassembler::push`] produced for one event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Sysex8Event {
    /// A message completed: its stream id and reassembled 8-bit payload.
    Message { stream_id: u8, payload: Vec<u8> },
    /// A message ended without a payload. The buffer is already discarded.
    Aborted { stream_id: u8, reason: Sysex8Abort },
}

/// Reassembles multi-packet SysEx8 runs, one buffer per `(group, stream_id)`.
#[derive(Clone, Debug)]
pub struct Sysex8Reassembler {
    in_flight: Vec<InFlight>,
    max_bytes: usize,
}

impl Default for Sysex8Reassembler {
    fn default() -> Self {
        Self::new()
    }
}

impl Sysex8Reassembler {
    /// A reassembler with no in-flight messages, bounded at
    /// [`DEFAULT_MAX_SYSEX8_BYTES`].
    pub fn new() -> Self {
        Self::with_max_bytes(DEFAULT_MAX_SYSEX8_BYTES)
    }

    /// A reassembler bounded at `max_bytes` of payload per message.
    ///
    /// As with SysEx7, an unterminated `START` + endless `CONTINUE` would grow
    /// the buffer until the host is out of memory. A run past the cap is dropped
    /// and reported as [`Sysex8Abort::TooLong`].
    pub fn with_max_bytes(max_bytes: usize) -> Self {
        Self {
            in_flight: Vec::new(),
            max_bytes,
        }
    }

    /// Feed one inbound event.
    ///
    /// Returns [`Sysex8Event::Message`] when a message completes, or
    /// [`Sysex8Event::Aborted`] when one ends without a usable payload, and
    /// `None` while a message is in flight or the event isn't SysEx8.
    ///
    /// Unlike the SysEx7 reassembler this never terminates a run on unrelated
    /// traffic: §7.8.1 permits interspersing, and the Stream ID is what keeps
    /// concurrent messages apart.
    pub fn push(&mut self, event: &MidiEvent) -> Option<Sysex8Event> {
        if event.message_type() != UmpMessageType::Sysex8 {
            return None;
        }
        let group = event.group();
        let (status, stream_id, _bytes) = event.sysex8_header()?;

        match status {
            SYSEX8_STATUS_SINGLE => {
                self.take(group, stream_id);
                Some(Self::finish(stream_id, &[*event]))
            }
            SYSEX8_STATUS_START => {
                // A second START for the same stream restarts it; the partial
                // run before it never got its End.
                self.take(group, stream_id);
                self.in_flight.push(InFlight {
                    group,
                    stream_id,
                    packets: vec![*event],
                });
                None
            }
            SYSEX8_STATUS_CONTINUE => {
                let cap = self.max_bytes;
                let entry = self.find(group, stream_id)?;
                entry.packets.push(*event);
                if entry.packets.len() * SYSEX8_BYTES_PER_PACKET > cap {
                    self.take(group, stream_id);
                    return Some(Sysex8Event::Aborted {
                        stream_id,
                        reason: Sysex8Abort::TooLong,
                    });
                }
                None
            }
            SYSEX8_STATUS_END => {
                let mut run = self.take(group, stream_id)?;
                // §7.8: "The special value 0xF is used in an End UMP to abort a
                // System Exclusive 8 message." The partial data is explicitly
                // untrustworthy, so report the abort rather than a payload.
                if event.is_sysex8_abort() {
                    return Some(Sysex8Event::Aborted {
                        stream_id,
                        reason: Sysex8Abort::SenderAborted,
                    });
                }
                run.push(*event);
                Some(Self::finish(stream_id, &run))
            }
            _ => None,
        }
    }

    /// Decode a complete packet run, reporting an abort if it doesn't parse.
    fn finish(stream_id: u8, run: &[MidiEvent]) -> Sysex8Event {
        match sysex8_message(run) {
            Some((sid, payload)) => Sysex8Event::Message {
                stream_id: sid,
                payload,
            },
            None => Sysex8Event::Aborted {
                stream_id,
                reason: Sysex8Abort::SenderAborted,
            },
        }
    }

    /// The number of in-flight (incomplete) runs. Diagnostics and tests.
    pub fn in_flight_count(&self) -> usize {
        self.in_flight.len()
    }

    fn find(&mut self, group: u8, stream_id: u8) -> Option<&mut InFlight> {
        self.in_flight
            .iter_mut()
            .find(|e| e.group == group && e.stream_id == stream_id)
    }

    fn take(&mut self, group: u8, stream_id: u8) -> Option<Vec<MidiEvent>> {
        let idx = self
            .in_flight
            .iter()
            .position(|e| e.group == group && e.stream_id == stream_id)?;
        Some(self.in_flight.swap_remove(idx).packets)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_midi_types::tutti_types::{MidiChannel, MidiGroup};
    use tutti_midi_types::ump::{SYSEX8_BYTES_ABORT, SYSEX8_STATUS_END};

    fn payload_of(ev: Option<Sysex8Event>) -> Vec<u8> {
        match ev {
            Some(Sysex8Event::Message { payload, .. }) => payload,
            other => panic!("expected a completed message, got {other:?}"),
        }
    }

    #[test]
    fn single_packet_completes_immediately() {
        let mut out = Vec::new();
        MidiEvent::sysex8_fragments(MidiGroup::FIRST, 0x11, &[1, 2, 3], &mut out);
        assert_eq!(out.len(), 1);

        let mut r = Sysex8Reassembler::new();
        assert_eq!(payload_of(r.push(&out[0])), vec![1, 2, 3]);
    }

    #[test]
    fn multi_packet_run_reassembles() {
        let data: Vec<u8> = (0u8..30).collect();
        let mut out = Vec::new();
        MidiEvent::sysex8_fragments(MidiGroup::FIRST, 0x55, &data, &mut out);
        assert_eq!(out.len(), 3);

        let mut r = Sysex8Reassembler::new();
        assert!(r.push(&out[0]).is_none());
        assert!(r.push(&out[1]).is_none());
        assert_eq!(payload_of(r.push(&out[2])), data);
    }

    #[test]
    fn interleaved_streams_on_one_group_stay_separate() {
        // The case the Stream ID exists for, and the one a per-group buffer
        // gets wrong: two messages in flight on group 0 at the same time.
        let a: Vec<u8> = (0u8..30).collect();
        let b: Vec<u8> = (100u8..130).collect();
        let (mut pa, mut pb) = (Vec::new(), Vec::new());
        MidiEvent::sysex8_fragments(MidiGroup::FIRST, 0x01, &a, &mut pa);
        MidiEvent::sysex8_fragments(MidiGroup::FIRST, 0x02, &b, &mut pb);

        // Start(1) Start(2) Continue(1) Continue(2) End(2) End(1)
        let mut r = Sysex8Reassembler::new();
        assert!(r.push(&pa[0]).is_none());
        assert!(r.push(&pb[0]).is_none());
        assert_eq!(r.in_flight_count(), 2, "both streams in flight at once");
        assert!(r.push(&pa[1]).is_none());
        assert!(r.push(&pb[1]).is_none());
        assert_eq!(payload_of(r.push(&pb[2])), b, "stream 2 completes first");
        assert_eq!(payload_of(r.push(&pa[2])), a, "stream 1 unaffected");
    }

    #[test]
    fn unrelated_traffic_does_not_terminate_a_run() {
        // §7.8.1 permits interspersing — unlike SysEx7, this must NOT terminate.
        let data: Vec<u8> = (0u8..30).collect();
        let mut out = Vec::new();
        MidiEvent::sysex8_fragments(MidiGroup::FIRST, 0x7F, &data, &mut out);

        let mut r = Sysex8Reassembler::new();
        r.push(&out[0]);
        assert!(r
            .push(&MidiEvent::note_on(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                60,
                0x8000
            ))
            .is_none());
        assert_eq!(r.in_flight_count(), 1, "note-on must not terminate SysEx8");
        r.push(&out[1]);
        assert_eq!(payload_of(r.push(&out[2])), data);
    }

    #[test]
    fn sender_abort_is_reported_not_silently_accepted() {
        // M2-104 §7.8: `# of bytes` = 0xF in an End UMP aborts the message. The
        // buffered data is explicitly untrustworthy.
        let data: Vec<u8> = (0u8..30).collect();
        let mut out = Vec::new();
        MidiEvent::sysex8_fragments(MidiGroup::FIRST, 0x09, &data, &mut out);

        let mut r = Sysex8Reassembler::new();
        r.push(&out[0]);
        r.push(&out[1]);

        // Hand-build the abort: End status with the 0xF byte count.
        let mut abort = out[2];
        abort.data[0] = (abort.data[0] & !0x00FF_0000)
            | ((SYSEX8_STATUS_END as u32) << 20)
            | ((SYSEX8_BYTES_ABORT as u32) << 16);
        assert!(abort.is_sysex8_abort());

        assert_eq!(
            r.push(&abort),
            Some(Sysex8Event::Aborted {
                stream_id: 0x09,
                reason: Sysex8Abort::SenderAborted,
            })
        );
        assert_eq!(r.in_flight_count(), 0, "buffer discarded");
    }

    #[test]
    fn an_unterminated_run_cannot_grow_without_bound() {
        let data: Vec<u8> = (0u8..30).collect();
        let mut out = Vec::new();
        MidiEvent::sysex8_fragments(MidiGroup::FIRST, 0x03, &data, &mut out);

        let mut r = Sysex8Reassembler::with_max_bytes(64);
        r.push(&out[0]);
        let cont = out[1];
        let mut aborted = false;
        for _ in 0..1000 {
            if let Some(Sysex8Event::Aborted {
                reason: Sysex8Abort::TooLong,
                ..
            }) = r.push(&cont)
            {
                aborted = true;
                break;
            }
        }
        assert!(aborted, "the over-long run is reported, not buffered");
        assert_eq!(r.in_flight_count(), 0);
    }

    #[test]
    fn non_sysex8_event_is_ignored() {
        let mut r = Sysex8Reassembler::new();
        assert!(r
            .push(&MidiEvent::note_on(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                60,
                0x8000
            ))
            .is_none());
        assert!(r.push(&MidiEvent::timing_clock(MidiGroup::FIRST)).is_none());
    }
}
