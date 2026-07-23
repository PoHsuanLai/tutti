//! Streaming reassembly of multi-packet SysEx7 UMP messages.
//!
//! Hardware and network transports deliver a SysEx message as a run of UMP
//! SysEx7 packets — a lone `SINGLE`, or a `START` → `CONTINUE`* → `END`
//! sequence (MIDI 2.0 spec §4.4). A consumer that decodes whole messages (a
//! MIDI-CI negotiator, a bulk-dump reader) sees these one event at a time and
//! must buffer until the run completes.
//!
//! [`Sysex7Reassembler`] is that buffer: feed each inbound event to
//! [`push`](Sysex7Reassembler::push); it returns the complete packet run the
//! moment one finishes (ready to hand to
//! [`tutti_midi_types::ci::sysex7_to_ci`] or any other
//! [`sysex7_payload`](tutti_midi_types::ump::MidiEvent::sysex7_payload)-based
//! decoder), and `None` while a message is still in flight or the event isn't
//! SysEx7. It keeps one in-flight buffer per UMP group, so interleaved streams
//! on different groups don't corrupt each other.

use tutti_midi_types::ump::{
    MidiEvent, UmpMessageType, SYSEX7_STATUS_CONTINUE, SYSEX7_STATUS_END, SYSEX7_STATUS_SINGLE,
    SYSEX7_STATUS_START,
};

/// One in-flight SysEx7 message being reassembled on a single UMP group.
#[derive(Clone, Debug)]
struct InFlight {
    group: u8,
    packets: Vec<MidiEvent>,
}

/// Reassembles multi-packet SysEx7 runs, one buffer per UMP group.
#[derive(Clone, Debug, Default)]
pub struct Sysex7Reassembler {
    /// In-flight messages keyed by group. A `Vec` (not a map) because a handful
    /// of groups are ever active at once; linear scan is cheaper than hashing.
    in_flight: Vec<InFlight>,
}

impl Sysex7Reassembler {
    /// A reassembler with no in-flight messages.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one inbound event. Returns the complete packet run when this event
    /// finishes a message (a `SINGLE`, or the `END` of a started run), else
    /// `None`. Non-SysEx7 events are ignored (return `None`) — the caller can
    /// pass its whole inbound stream through without pre-filtering.
    ///
    /// An out-of-order packet (a `CONTINUE`/`END` with no matching `START`, or a
    /// second `START` mid-run) drops the stale buffer for that group and resyncs,
    /// rather than emitting a corrupt message.
    pub fn push(&mut self, event: &MidiEvent) -> Option<Vec<MidiEvent>> {
        if event.message_type() != UmpMessageType::Sysex7 {
            return None;
        }
        let group = event.group();
        let (status, _bytes, _n) = event.sysex7_payload()?;

        match status {
            SYSEX7_STATUS_SINGLE => {
                // A one-packet message: drop any stale in-flight run on this
                // group and return the single packet immediately.
                self.take_group(group);
                Some(vec![*event])
            }
            SYSEX7_STATUS_START => {
                // Begin (or restart) a run on this group.
                self.take_group(group);
                self.in_flight.push(InFlight {
                    group,
                    packets: vec![*event],
                });
                None
            }
            SYSEX7_STATUS_CONTINUE => {
                if let Some(entry) = self.find_group(group) {
                    entry.packets.push(*event);
                }
                // A CONTINUE with no START is dropped (no matching buffer).
                None
            }
            SYSEX7_STATUS_END => {
                let mut run = self.take_group(group)?;
                run.push(*event);
                Some(run)
            }
            _ => None,
        }
    }

    fn find_group(&mut self, group: u8) -> Option<&mut InFlight> {
        self.in_flight.iter_mut().find(|e| e.group == group)
    }

    /// Remove and return the in-flight packet run for `group`, if any.
    fn take_group(&mut self, group: u8) -> Option<Vec<MidiEvent>> {
        let idx = self.in_flight.iter().position(|e| e.group == group)?;
        Some(self.in_flight.swap_remove(idx).packets)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_midi_types::ci::{ci_to_sysex7, sysex7_to_ci, CiHeader, CiMessage, DiscoveryData};
    use tutti_midi_types::ci::{CiCategories, Muid, CI_DEVICE_ID_FUNCTION_BLOCK, CI_VERSION};

    fn discovery(mfr: [u8; 3]) -> CiMessage {
        CiMessage::Discovery {
            header: CiHeader {
                device_id: CI_DEVICE_ID_FUNCTION_BLOCK,
                ci_version: CI_VERSION,
                source: Muid(0x0123_4567),
                destination: Muid::BROADCAST,
            },
            is_reply: false,
            data: DiscoveryData {
                manufacturer: mfr,
                family: 0x1234,
                family_model: 0x0055,
                software_revision: [1, 0, 0, 0],
                categories: CiCategories::PROFILE_CONFIGURATION,
                max_sysex_size: 512,
            },
        }
    }

    #[test]
    fn single_packet_completes_immediately() {
        // A tiny payload fragments to one SINGLE packet.
        let mut packets = Vec::new();
        MidiEvent::sysex7_fragments(0, &[0x01, 0x02, 0x03], &mut packets);
        assert_eq!(packets.len(), 1, "small payload is a single packet");

        let mut r = Sysex7Reassembler::new();
        let run = r.push(&packets[0]).expect("single completes");
        assert_eq!(run.len(), 1);
    }

    #[test]
    fn multi_packet_ci_message_round_trips() {
        // A full CI Discovery is larger than 6 bytes → START + CONTINUE… + END.
        let msg = discovery([0x00, 0x21, 0x09]);
        let mut packets = Vec::new();
        ci_to_sysex7(0, &msg, &mut packets);
        assert!(packets.len() > 1, "CI discovery spans multiple packets");

        let mut r = Sysex7Reassembler::new();
        let mut completed = None;
        for p in &packets {
            if let Some(run) = r.push(p) {
                completed = Some(run);
            }
        }
        let run = completed.expect("run completes on the END packet");
        assert_eq!(sysex7_to_ci(&run), Some(msg));
    }

    #[test]
    fn interleaved_groups_do_not_corrupt() {
        let a = discovery([1, 2, 3]);
        let b = discovery([4, 5, 6]);
        let mut pa = Vec::new();
        let mut pb = Vec::new();
        ci_to_sysex7(0, &a, &mut pa);
        ci_to_sysex7(1, &b, &mut pb);

        // Interleave the two multi-packet runs on groups 0 and 1.
        let mut r = Sysex7Reassembler::new();
        let mut got_a = None;
        let mut got_b = None;
        let max = pa.len().max(pb.len());
        for i in 0..max {
            if let Some(p) = pa.get(i) {
                if let Some(run) = r.push(p) {
                    got_a = Some(run);
                }
            }
            if let Some(p) = pb.get(i) {
                if let Some(run) = r.push(p) {
                    got_b = Some(run);
                }
            }
        }
        assert_eq!(sysex7_to_ci(&got_a.expect("a completes")), Some(a));
        assert_eq!(sysex7_to_ci(&got_b.expect("b completes")), Some(b));
    }

    #[test]
    fn non_sysex7_event_is_ignored() {
        let mut r = Sysex7Reassembler::new();
        assert!(r.push(&MidiEvent::note_on(0, 0, 60, 0x8000)).is_none());
    }
}
