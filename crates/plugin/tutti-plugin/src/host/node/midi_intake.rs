//! The plugin node's MIDI intake: a chunk's events into its capped list.
//!
//! A chunk carries at most [`MIDI_STACK_CAPACITY`] MIDI events (the list is
//! inline and cannot grow); the rest are dropped and counted. Dropping one
//! packet of a multi-packet SysEx would leave a gap that the host's
//! reassembler cannot see, and the message's later packets, in the next
//! chunk, would complete it with bytes missing. So once a `START` or
//! `CONTINUE` is dropped, the intake drops the rest of that message too (up
//! to its `END`, or the group's next `START` or `SINGLE`): the plugin's
//! reassembler then never sees its `END`, and drops the open message whole,
//! counted, at the next `START`.

use std::sync::atomic::Ordering;

use tutti_midi_types::ump::{
    UmpMessageType, SYSEX7_STATUS_CONTINUE, SYSEX7_STATUS_END, SYSEX7_STATUS_SINGLE,
    SYSEX7_STATUS_START,
};

use super::param_delivery::DeliveryCounters;
use crate::protocol::{MidiEvent, MidiEventVec};

/// Takes a node's MIDI into its chunk's list. Carries, across chunks and
/// blocks, which groups' SysEx is being skipped after a dropped packet.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct MidiIntake {
    /// One bit per group whose SysEx message lost a packet and is being
    /// skipped to its end.
    skipping: u16,
}

impl MidiIntake {
    /// Push `event` onto `list`, or drop it: past the list's cap, or as the
    /// rest of a SysEx that already lost a packet. Returns whether it was
    /// taken; the caller counts what was not
    /// ([`DeliveryCounters::count_midi_dropped`]). Never allocates.
    #[must_use = "a dropped event is counted by the caller"]
    pub(crate) fn push(&mut self, list: &mut MidiEventVec, event: MidiEvent) -> bool {
        let sysex = sysex7_status(&event).map(|status| (status, 1u16 << event.group().get()));
        if let Some((status, bit)) = sysex {
            match status {
                SYSEX7_STATUS_START | SYSEX7_STATUS_SINGLE => self.skipping &= !bit,
                SYSEX7_STATUS_CONTINUE | SYSEX7_STATUS_END if self.skipping & bit != 0 => {
                    if status == SYSEX7_STATUS_END {
                        self.skipping &= !bit;
                    }
                    return false;
                }
                _ => {}
            }
        }
        if list.try_push(event).is_ok() {
            return true;
        }
        if let Some((SYSEX7_STATUS_START | SYSEX7_STATUS_CONTINUE, bit)) = sysex {
            self.skipping |= bit;
        }
        false
    }

    /// The plugin was reset: nothing is being skipped. (Its reassembler
    /// dropped the open message.)
    pub(crate) fn reset(&mut self) {
        self.skipping = 0;
    }
}

/// A SysEx7 packet's status, or `None` for any other message.
fn sysex7_status(event: &MidiEvent) -> Option<u8> {
    if event.message_type() != UmpMessageType::Sysex7 {
        return None;
    }
    event.sysex7_payload().map(|(status, _, _)| status)
}

impl DeliveryCounters {
    /// `n` MIDI events the node did not deliver. RT-safe: one relaxed add.
    pub(crate) fn count_midi_dropped(&self, n: u64) {
        if n > 0 {
            self.midi_dropped.fetch_add(n, Ordering::Relaxed);
        }
    }

    /// MIDI events the node has not delivered since the plugin was bound:
    /// past a chunk's [`MIDI_STACK_CAPACITY`](crate::protocol::MIDI_STACK_CAPACITY),
    /// or the rest of a SysEx that lost a packet there. RT-safe: one relaxed
    /// load.
    pub(crate) fn midi_events_dropped(&self) -> u64 {
        self.midi_dropped.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::MIDI_STACK_CAPACITY;
    use tutti_midi_types::{MidiChannel, MidiGroup};

    fn packets(group: MidiGroup, len: usize) -> Vec<MidiEvent> {
        let payload: Vec<u8> = (0..len).map(|i| (i % 0x7F) as u8).collect();
        let mut out = Vec::new();
        MidiEvent::sysex7_fragments(group, &payload, &mut out);
        out
    }

    fn note() -> MidiEvent {
        MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000)
    }

    /// **Past the cap, events are dropped and reported, without allocating.**
    ///
    /// Mutation (run): a refused push returning `true` → fails.
    #[test]
    fn past_the_cap_events_are_dropped() {
        let mut intake = MidiIntake::default();
        let mut list = MidiEventVec::new();
        let mut dropped = 0;
        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..MIDI_STACK_CAPACITY + 10 {
                dropped += u32::from(!intake.push(&mut list, note()));
            }
        });
        assert_eq!(list.len(), MIDI_STACK_CAPACITY);
        assert_eq!(dropped, 10);
    }

    /// **A SysEx that lost a packet to the cap loses the rest of it in the
    /// next chunk too**, so its later packets never reach the plugin's
    /// reassembler to complete it with a gap. Another group's message, and
    /// this group's next one, pass.
    ///
    /// Mutation (run): the skip never set (a refused packet only counted) →
    /// the cut message's tail is taken into the next chunk → fails.
    #[test]
    fn a_cut_sysex_is_skipped_to_its_end() {
        let mut intake = MidiIntake::default();
        let mut dropped = 0usize;
        let cut = packets(MidiGroup::FIRST, 6 * 20);
        let other = packets(MidiGroup::new(2), 30);
        let next = packets(MidiGroup::FIRST, 30);

        // The first chunk fills up with notes, then takes the first three
        // packets of `cut` and refuses the rest.
        let mut list = MidiEventVec::new();
        for _ in 0..MIDI_STACK_CAPACITY - 3 {
            assert!(intake.push(&mut list, note()));
        }
        for &p in &cut[..5] {
            dropped += usize::from(!intake.push(&mut list, p));
        }
        assert_eq!(dropped, 2);

        // The next chunk: the rest of `cut` is skipped, `other` and `next`
        // pass whole.
        let mut list = MidiEventVec::new();
        for &p in cut[5..].iter().chain(&other).chain(&next) {
            dropped += usize::from(!intake.push(&mut list, p));
        }
        let taken: Vec<_> = other.iter().chain(&next).copied().collect();
        assert_eq!(list.as_slice(), taken.as_slice());
        assert_eq!(dropped, 2 + (cut.len() - 5));
    }
}
