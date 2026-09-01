//! Streaming reassembly of multi-packet SysEx7 UMP messages.
//!
//! Hardware and network transports deliver a SysEx message as a run of UMP
//! SysEx7 packets — a lone `SINGLE`, or a `START` → `CONTINUE`* → `END`
//! sequence (MIDI 2.0 spec §4.4). A consumer that decodes whole messages (a
//! MIDI-CI negotiator, a bulk-dump reader) sees these one event at a time and
//! must buffer until the run completes.
//!
//! [`Sysex7PacketReassembler`] is that buffer: feed each inbound event to
//! [`push`](Sysex7PacketReassembler::push); it returns the complete packet run the
//! moment one finishes (ready to hand to
//! [`tutti_midi_types::ci::sysex7_to_ci`] or any other
//! [`sysex7_payload`](tutti_midi_types::ump::MidiEvent::sysex7_payload)-based
//! decoder), and `None` while a message is still in flight or the event isn't
//! SysEx7. It keeps one in-flight buffer per UMP group, so interleaved streams
//! on different groups don't corrupt each other.

use tutti_midi_types::MidiGroup;
use tutti_midi_types::ump::{
    MidiEvent, UmpMessageType, SYSEX7_STATUS_CONTINUE, SYSEX7_STATUS_END, SYSEX7_STATUS_SINGLE,
    SYSEX7_STATUS_START,
};

/// Whether `event` may appear between a SysEx7 Start and its End without
/// terminating the message (M2-104 §7.7.1).
///
/// Three carve-outs, and only three: System Real Time messages (status
/// 0xF8..=0xFF, "in order to maintain timing synchronization"), and the two
/// Groupless message types — Utility (MT 0x0) and UMP Stream (MT 0xF). Note
/// System *Common* (0xF1..=0xF6) shares MT 0x1 with Real Time but is **not**
/// exempt, so the status byte has to be read rather than the type nibble alone.
///
/// Different-group traffic is also permitted, but the caller handles that by
/// only ever terminating the run belonging to the event's own group.
fn is_sysex_transparent(event: &MidiEvent) -> bool {
    match event.message_type() {
        // Groupless: cannot belong to the SysEx's group at all.
        UmpMessageType::Other(0x0) | UmpMessageType::UmpStream => true,
        UmpMessageType::System => {
            // System Real Time only; System Common terminates.
            let status = ((event.data[0] >> 16) & 0xFF) as u8;
            status >= 0xF8
        }
        _ => false,
    }
}

/// One in-flight SysEx7 message being reassembled on a single UMP group.
#[derive(Clone, Debug)]
struct InFlight {
    group: MidiGroup,
    packets: Vec<MidiEvent>,
}

/// Default cap on a reassembled payload, in bytes.
///
/// M2-101 §5.5.3 sets the floor a device must accept: "All MIDI-CI Devices shall
/// support System Exclusive message lengths of at least 128 bytes", rising to
/// 512 "if either Profile Configuration or Property Exchange is supported" —
/// which tutti does. 4 KiB leaves generous headroom for bulk dumps above that
/// floor while keeping a hostile or faulty stream bounded.
pub const DEFAULT_MAX_SYSEX_BYTES: usize = 4096;

/// SysEx7 carries 6 payload bytes per packet (M2-104 §7.7).
const SYSEX7_BYTES_PER_PACKET: usize = 6;

/// Reassembles multi-packet SysEx7 runs, one buffer per UMP group.
#[derive(Clone, Debug)]
pub struct Sysex7PacketReassembler {
    /// In-flight messages keyed by group. A `Vec` (not a map) because a handful
    /// of groups are ever active at once; linear scan is cheaper than hashing.
    in_flight: Vec<InFlight>,
    /// Largest payload to accumulate before abandoning a run.
    max_bytes: usize,
}

impl Default for Sysex7PacketReassembler {
    fn default() -> Self {
        Self::new()
    }
}

impl Sysex7PacketReassembler {
    /// A reassembler with no in-flight messages, bounded at
    /// [`DEFAULT_MAX_SYSEX_BYTES`].
    pub fn new() -> Self {
        Self::with_max_bytes(DEFAULT_MAX_SYSEX_BYTES)
    }

    /// A reassembler bounded at `max_bytes` of payload per message.
    ///
    /// The bound exists because a `START` followed by unlimited `CONTINUE`s
    /// never completes: without a cap the buffer grows until the host runs out
    /// of memory, which a faulty or hostile device can trigger remotely. A run
    /// that exceeds the cap is dropped and the group resyncs on the next
    /// `START`/`SINGLE`, rather than being emitted truncated.
    ///
    /// Pass a peer's advertised `max_sysex_size` (M2-101 §5.5.3) when it is
    /// known; the floor the spec guarantees is 128 bytes, or 512 for a device
    /// supporting Profile Configuration or Property Exchange.
    pub fn with_max_bytes(max_bytes: usize) -> Self {
        Self {
            in_flight: Vec::new(),
            max_bytes,
        }
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
            // M2-104 §7.7.1: "If any Message or UMP on the same Group, other
            // than a System Exclusive Continue UMP or a System Real Time
            // Message, is sent after a System Exclusive Start UMP and before the
            // associated System Exclusive End UMP, then that UMP shall terminate
            // the System Exclusive Message."
            //
            // The two carve-outs are explicit: System Real Time "may be inserted
            // between the UMPs of a System Exclusive message, in order to
            // maintain timing synchronization", and "Messages which are
            // Groupless (MT = 0x0 and 0xF) and those which are sent to a
            // different Group may be interspersed".
            //
            // Terminating drops the partial run rather than emitting it: the
            // message never got its End, so its payload is incomplete.
            if !is_sysex_transparent(event) {
                self.take_group(event.group());
            }
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
                let cap = self.max_bytes;
                if let Some(entry) = self.find_group(group) {
                    entry.packets.push(*event);
                    // Abandon a run that outgrows the cap: a START followed by
                    // unlimited CONTINUEs never completes, so without this the
                    // buffer grows until the host is out of memory.
                    if entry.packets.len() * SYSEX7_BYTES_PER_PACKET > cap {
                        self.take_group(group);
                    }
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

    fn find_group(&mut self, group: MidiGroup) -> Option<&mut InFlight> {
        self.in_flight.iter_mut().find(|e| e.group == group)
    }

    /// The number of in-flight (incomplete) runs. Diagnostics and tests.
    pub fn in_flight_count(&self) -> usize {
        self.in_flight.len()
    }

    /// Remove and return the in-flight packet run for `group`, if any.
    fn take_group(&mut self, group: MidiGroup) -> Option<Vec<MidiEvent>> {
        let idx = self.in_flight.iter().position(|e| e.group == group)?;
        Some(self.in_flight.swap_remove(idx).packets)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_midi_types::ci::{ci_to_sysex7, sysex7_to_ci, CiHeader, CiMessage, DiscoveryData};
    use tutti_midi_types::ci::{CiCategories, Muid, CI_DEVICE_ID_FUNCTION_BLOCK, CI_VERSION};
    use tutti_midi_types::{MidiChannel, MidiGroup};

    fn discovery(mfr: [u8; 3]) -> CiMessage {
        CiMessage::Discovery {
            header: CiHeader {
                device_id: CI_DEVICE_ID_FUNCTION_BLOCK,
                ci_version: CI_VERSION,
                source: Muid(0x0123_4567),
                destination: Muid::BROADCAST,
            },
            is_reply: false,
            data: DiscoveryData::new(
                mfr,
                0x1234,
                0x0055,
                [1, 0, 0, 0],
                CiCategories::PROFILE_CONFIGURATION,
                512,
            ),
        }
    }

    #[test]
    fn single_packet_completes_immediately() {
        // A tiny payload fragments to one SINGLE packet.
        let mut packets = Vec::new();
        MidiEvent::sysex7_fragments(MidiGroup::FIRST, &[0x01, 0x02, 0x03], &mut packets);
        assert_eq!(packets.len(), 1, "small payload is a single packet");

        let mut r = Sysex7PacketReassembler::new();
        let run = r.push(&packets[0]).expect("single completes");
        assert_eq!(run.len(), 1);
    }

    #[test]
    fn multi_packet_ci_message_round_trips() {
        // A full CI Discovery is larger than 6 bytes → START + CONTINUE… + END.
        let msg = discovery([0x00, 0x21, 0x09]);
        let mut packets = Vec::new();
        ci_to_sysex7(MidiGroup::FIRST, &msg, &mut packets);
        assert!(packets.len() > 1, "CI discovery spans multiple packets");

        let mut r = Sysex7PacketReassembler::new();
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
        ci_to_sysex7(MidiGroup::FIRST, &a, &mut pa);
        ci_to_sysex7(MidiGroup::new(1), &b, &mut pb);

        // Interleave the two multi-packet runs on groups 0 and 1.
        let mut r = Sysex7PacketReassembler::new();
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
        let mut r = Sysex7PacketReassembler::new();
        assert!(r
            .push(&MidiEvent::note_on(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                60,
                0x8000
            ))
            .is_none());
    }

    /// A multi-packet run on `group`, for termination tests.
    fn multi_packet_run(group: MidiGroup) -> Vec<MidiEvent> {
        let mut packets = Vec::new();
        ci_to_sysex7(group, &discovery([1, 2, 3]), &mut packets);
        assert!(packets.len() > 2, "need START + CONTINUE + END");
        packets
    }

    #[test]
    fn same_group_traffic_terminates_the_run() {
        // M2-104 §7.7.1: any same-group UMP other than a SysEx Continue or a
        // System Real Time message terminates the message in flight.
        let packets = multi_packet_run(MidiGroup::FIRST);
        let mut r = Sysex7PacketReassembler::new();
        r.push(&packets[0]); // START
        assert_eq!(r.in_flight_count(), 1);

        // A note-on on the same group terminates it.
        assert!(r
            .push(&MidiEvent::note_on(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                60,
                0x8000
            ))
            .is_none());
        assert_eq!(r.in_flight_count(), 0, "run terminated");

        // The END now has no buffer, so nothing corrupt is emitted.
        assert!(r.push(packets.last().unwrap()).is_none());
    }

    #[test]
    fn real_time_and_groupless_and_other_groups_do_not_terminate() {
        let packets = multi_packet_run(MidiGroup::FIRST);
        let mut r = Sysex7PacketReassembler::new();
        for p in &packets[..packets.len() - 1] {
            r.push(p);
        }

        // §7.7.1's explicit carve-outs: System Real Time "may be inserted…in
        // order to maintain timing synchronization", and Groupless messages
        // (MT 0x0 / 0xF) plus other-group traffic "may be interspersed".
        assert!(r.push(&MidiEvent::timing_clock(MidiGroup::FIRST)).is_none());
        assert!(r.push(&MidiEvent::jr_timestamp(0x1234)).is_none()); // MT 0x0
        assert!(r
            .push(&MidiEvent::note_on(
                MidiGroup::new(3),
                MidiChannel::FIRST,
                60,
                0x8000
            ))
            .is_none()); // group 3
        assert_eq!(r.in_flight_count(), 1, "run survives all three");

        // …and the message still completes intact.
        let run = r.push(packets.last().unwrap()).expect("completes");
        assert_eq!(sysex7_to_ci(&run), Some(discovery([1, 2, 3])));
    }

    #[test]
    fn system_common_terminates_but_real_time_does_not() {
        // System Common (0xF1..=0xF6) shares MT 0x1 with Real Time but is not
        // exempt — the status byte decides, not the type nibble.
        let packets = multi_packet_run(MidiGroup::FIRST);
        let mut r = Sysex7PacketReassembler::new();
        r.push(&packets[0]);
        assert!(r
            .push(&MidiEvent::song_select(MidiGroup::FIRST, 3))
            .is_none());
        assert_eq!(r.in_flight_count(), 0, "System Common terminates");
    }

    #[test]
    fn an_unterminated_run_cannot_grow_without_bound() {
        // START + endless CONTINUE is the DoS shape: it never completes, so
        // only a cap stops the buffer growing until the host is out of memory.
        let mut r = Sysex7PacketReassembler::with_max_bytes(64);
        let packets = multi_packet_run(MidiGroup::FIRST);
        r.push(&packets[0]);

        let continue_packet = packets[1];
        for _ in 0..1000 {
            assert!(r.push(&continue_packet).is_none());
        }
        assert_eq!(
            r.in_flight_count(),
            0,
            "the over-long run was abandoned, not buffered"
        );

        // The group resyncs on the next START.
        r.push(&packets[0]);
        assert_eq!(r.in_flight_count(), 1);
    }

    #[test]
    fn a_message_within_the_cap_still_completes() {
        let packets = multi_packet_run(MidiGroup::FIRST);
        // Cap generous enough for this message's payload.
        let mut r = Sysex7PacketReassembler::with_max_bytes(packets.len() * 6);
        let mut completed = None;
        for p in &packets {
            if let Some(run) = r.push(p) {
                completed = Some(run);
            }
        }
        assert_eq!(
            sysex7_to_ci(&completed.expect("completes")),
            Some(discovery([1, 2, 3]))
        );
    }
}
