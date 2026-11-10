//! The plugin's MIDI intake: what keeps a SysEx from reaching the plugin's
//! reassembler with a gap in it.
//!
//! The host's reassembler (in the plugin-server, or the in-process VST2
//! host) sees only the packets that reach it; it cannot tell a packet that
//! never came from one that was lost on the way. Packets are lost on the way
//! in two ways: one event past a chunk's [`MIDI_STACK_CAPACITY`] (the list is
//! inline and cannot grow), or a whole chunk's MIDI (a chunk that never
//! reached the bridge, a reset or re-prepare, a block the in-process node
//! could not run, a chunk the bridge thread skipped as stale). Either way the
//! message's later packets would complete it with bytes missing. So the
//! intake drops the rest of every message that lost a packet (up to its
//! `END`, or its group's next `START` or `SINGLE`): the reassembler never
//! sees that `END`, and drops the open message whole, counted, at the next
//! `START`.
//!
//! [`MIDI_STACK_CAPACITY`]: crate::protocol::MIDI_STACK_CAPACITY

use std::sync::atomic::Ordering;

use tutti_midi_types::ump::{
    SYSEX7_STATUS_CONTINUE, SYSEX7_STATUS_END, SYSEX7_STATUS_SINGLE, SYSEX7_STATUS_START,
    UmpMessageType, message_type, sysex7_payload,
};
use tutti_ump::Ump;

use super::param_delivery::DeliveryCounters;
use crate::protocol::{MidiEventVec, WireMidi};

/// The intake's state, one bit per UMP group, carried across chunks and
/// blocks, and across a reset (the reassembler past it keeps its open run
/// across the node's reset: neither the plugin-server nor the in-process
/// VST2 node resets it).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct MidiIntake {
    /// Groups whose SysEx message lost a packet and is being skipped to its
    /// end.
    skipping: u16,
    /// Groups with a multi-packet SysEx passed on and not ended: a `START`
    /// or `CONTINUE` taken, its `END` (or a `SINGLE`) not yet.
    open: u16,
    /// The list is full of events a note-off cannot displace (see
    /// [`push_displacing`](Self::push_displacing)); until the next chunk.
    exhausted: bool,
    /// Passes over the list `push_displacing` has made (tests count them).
    #[cfg(test)]
    scans: u32,
}

impl MidiIntake {
    /// Whether `event` passes on: `false` for the rest of a SysEx being
    /// skipped. Notes which messages are open. Never allocates.
    pub(crate) fn admit(&mut self, event: &WireMidi) -> bool {
        let Some(status) = sysex7_status(&event.ump) else {
            return true;
        };
        let bit = group_bit(&event.ump);
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
        match status {
            SYSEX7_STATUS_START | SYSEX7_STATUS_CONTINUE => self.open |= bit,
            _ => self.open &= !bit,
        }
        true
    }

    /// Push `event` onto `list`, or drop it: past the list's cap, or as the
    /// rest of a SysEx that already lost a packet. Returns whether it was
    /// taken; the caller counts what was not
    /// ([`DeliveryCounters::count_midi_dropped`]). Never allocates.
    #[must_use = "a dropped event is counted by the caller"]
    pub(crate) fn push(&mut self, list: &mut MidiEventVec, event: WireMidi) -> bool {
        if !self.admit(&event) {
            return false;
        }
        if list.try_push(event).is_ok() {
            return true;
        }
        // Refused: a `START` or `CONTINUE` leaves its message with a gap.
        // (A refused `END` or `SINGLE` leaves none: its message's packets
        // all went or none did.)
        if matches!(
            sysex7_status(&event.ump),
            Some(SYSEX7_STATUS_START | SYSEX7_STATUS_CONTINUE)
        ) {
            let bit = group_bit(&event.ump);
            self.skipping |= bit;
            self.open &= !bit;
        }
        false
    }

    /// [`push`](Self::push), but a note-off that finds the list full takes
    /// the place of a note-on already in it (the one of its own key if the
    /// list has it, else the latest; failing that, the latest channel
    /// message that is not a note-off): the list is a chunk's worth of events
    /// the wire cannot grow, and a note-on lost is a missed note where a
    /// note-off lost is a note that never ends. Returns how many events this
    /// call lost: 0 if `event` went in with room, 1 if it was refused, or
    /// went in over an event it displaced. The caller counts them. A
    /// displaced note-on's own note-off, if already in the list, reaches the
    /// plugin as an off for a note it never heard, which a plugin ignores.
    ///
    /// **Cost, bounded.** One reverse pass over the list decoding each
    /// event's first word (no parse), then a shift: about 0.3 µs for a list
    /// of 256. Each displacement removes a displaceable event, so a list of
    /// 256 takes at most 256 of them, and a scan that finds none marks the
    /// list `exhausted` until the next chunk ([`new_chunk`](Self::new_chunk)),
    /// so the note-offs after it are refused in constant time. A chunk's
    /// worst case is 256 passes and one empty one, whatever the input
    /// carries. Never allocates (`ArrayVec::remove` shifts in place).
    #[must_use = "the lost events are counted by the caller"]
    pub(crate) fn push_displacing(&mut self, list: &mut MidiEventVec, event: WireMidi) -> u64 {
        if self.push(list, event) {
            return 0;
        }
        if self.exhausted || !is_off(&event.ump.0) || !list.is_full() {
            return 1;
        }
        #[cfg(test)]
        {
            self.scans += 1;
        }
        let key = key_of(&event.ump.0);
        let mut latest_on = None;
        let mut latest_other = None;
        let mut victim = None;
        for (i, e) in list.iter().enumerate().rev() {
            let w = e.ump.0[0];
            if is_on(&e.ump.0) {
                if key_of(&e.ump.0) == key {
                    victim = Some(i);
                    break;
                }
                latest_on.get_or_insert(i);
            } else if is_channel_message(w) && !is_off(&e.ump.0) {
                latest_other.get_or_insert(i);
            }
        }
        match victim.or(latest_on).or(latest_other) {
            Some(i) => {
                list.remove(i);
                let _ = list.try_push(event);
            }
            None => self.exhausted = true,
        }
        1
    }

    /// A new chunk begins: the list is empty again.
    pub(crate) fn new_chunk(&mut self) {
        self.exhausted = false;
    }

    /// Everything passed on since the last chunk that arrived was lost
    /// (a whole chunk's MIDI never reached the reassembler, or the node was
    /// reset): skip the rest of every message open in it. Never allocates.
    pub(crate) fn lost(&mut self) {
        self.skipping |= self.open;
        self.open = 0;
    }

    /// `list`, taken through this intake, never reaches the plugin (a chunk
    /// not sent, or the events taken towards the next chunk at a reset):
    /// empty it, skip the rest of every message open in it or before it
    /// ([`lost`](Self::lost)), and return how many events it held, for the
    /// caller to count. Never allocates.
    #[must_use = "the lost events are counted by the caller"]
    pub(crate) fn lose(&mut self, list: &mut MidiEventVec) -> u64 {
        let n = list.len() as u64;
        list.clear();
        self.exhausted = false;
        self.lost();
        n
    }
}

/// Whether the UMP in `data` is a MIDI 1.0 or 2.0 channel voice message.
fn is_channel_message(w: u32) -> bool {
    matches!(w >> 28, 0x2 | 0x4)
}

/// Whether `data` is a note-on that starts a note (a MIDI 1.0 note-on at
/// velocity 0 is a note-off).
fn is_on(data: &[u32; 4]) -> bool {
    let w = data[0];
    (w >> 20) & 0xf == 0x9 && is_channel_message(w) && (w >> 28 == 0x4 || w & 0x7f != 0)
}

/// Whether `data` is a note-off (or a MIDI 1.0 note-on at velocity 0).
fn is_off(data: &[u32; 4]) -> bool {
    let w = data[0];
    is_channel_message(w)
        && match (w >> 20) & 0xf {
            0x8 => true,
            0x9 => w >> 28 == 0x2 && w & 0x7f == 0,
            _ => false,
        }
}

/// A note message's group, channel and note number.
fn key_of(data: &[u32; 4]) -> (u32, u32, u32) {
    let w = data[0];
    ((w >> 24) & 0xf, (w >> 16) & 0xf, (w >> 8) & 0x7f)
}

/// A SysEx7 packet's status, or `None` for any other message.
fn sysex7_status(event: &Ump) -> Option<u8> {
    if message_type(event) != UmpMessageType::Sysex7 {
        return None;
    }
    sysex7_payload(event).map(|(status, _, _)| status)
}

/// The bit of `event`'s UMP group in the intake's per-group masks.
fn group_bit(event: &Ump) -> u16 {
    1u16 << ((event.0[0] >> 24) & 0xF)
}

impl DeliveryCounters {
    /// `n` MIDI events the node did not deliver. RT-safe: one relaxed add.
    pub(crate) fn count_midi_dropped(&self, n: u64) {
        if n > 0 {
            self.midi_dropped.fetch_add(n, Ordering::Relaxed);
        }
    }

    /// Add the bridge thread's count to
    /// [`dropped_midi_in`](Self::dropped_midi_in). Once, at bind;
    /// a second call is ignored.
    pub(crate) fn attach_bridge_midi(&self, cell: std::sync::Arc<std::sync::atomic::AtomicU64>) {
        let _ = self.bridge_midi_dropped.set(cell);
    }

    /// MIDI events on the node's event input that never reached the
    /// plugin's host, since the plugin was bound: see
    /// [`PluginControls::dropped_midi_in`](super::PluginControls::dropped_midi_in).
    /// RT-safe: two relaxed loads.
    pub(crate) fn dropped_midi_in(&self) -> u64 {
        let bridge = self
            .bridge_midi_dropped
            .get()
            .map_or(0, |c| c.load(Ordering::Relaxed));
        self.midi_dropped.load(Ordering::Relaxed) + bridge
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::MIDI_STACK_CAPACITY;
    use tutti_fixtures::midi;
    use tutti_plugin_types::{SysExAssembler, SysExStep};
    use tutti_ump::{MidiChannel, MidiGroup};

    fn wire(ump: Ump) -> WireMidi {
        WireMidi { offset: 0, ump }
    }

    fn packets(group: MidiGroup, len: usize) -> Vec<WireMidi> {
        let payload: Vec<u8> = (0..len).map(|i| (i % 0x7F) as u8).collect();
        let mut out = Vec::new();
        tutti_midi_types::sysex7_fragments(group, &payload, &mut out);
        out.into_iter().map(wire).collect()
    }

    fn note() -> WireMidi {
        wire(midi::note_on(
            MidiGroup::FIRST,
            MidiChannel::FIRST,
            60,
            0x8000,
        ))
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
        let other = packets(MidiGroup::of(2), 30);
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

    /// The plugin's host at the far end of the chunks: a reassembler, fed
    /// each chunk that arrives as one block. Returns the complete messages'
    /// byte lengths.
    fn deliver(asm: &mut SysExAssembler, chunk: &[WireMidi]) -> Vec<usize> {
        asm.begin_block();
        chunk
            .iter()
            .filter_map(|e| match asm.push(&e.ump) {
                SysExStep::Complete(m) => Some(m.len()),
                _ => None,
            })
            .collect()
    }

    /// **A message cut by the cap stays cut across a reset.** The plugin's
    /// host keeps its open message across the node's reset (the
    /// plugin-server's `Reset` and the in-process VST2 node's reset do not
    /// touch it), so the tail arriving after the reset would complete the
    /// cut message with packets missing. A chunk of `cap - 3` notes and the
    /// first five packets of a 120-byte dump (three taken, two refused);
    /// the node resets; the dump's tail arrives: no message, and the next
    /// message arrives whole with the cut one counted dropped.
    ///
    /// Mutation (run): `lost` clearing `skipping` (what the reset used to
    /// do) → the tail completes a 110-byte message → fails.
    #[test]
    fn a_message_cut_before_a_reset_stays_cut_after_it() {
        let dump = packets(MidiGroup::FIRST, 120);
        let next = packets(MidiGroup::FIRST, 30);
        let mut intake = MidiIntake::default();
        let mut asm = SysExAssembler::reserved();
        let mut dropped = 0u64;

        let mut chunk = MidiEventVec::new();
        for _ in 0..MIDI_STACK_CAPACITY - 3 {
            assert!(intake.push(&mut chunk, note()));
        }
        for &p in &dump[..5] {
            dropped += u64::from(!intake.push(&mut chunk, p));
        }
        assert_eq!(dropped, 2);
        assert!(deliver(&mut asm, &chunk).is_empty());

        // The node's reset: nothing taken towards the next chunk yet.
        let mut pending = MidiEventVec::new();
        dropped += intake.lose(&mut pending);

        let mut chunk = MidiEventVec::new();
        for &p in dump[5..].iter().chain(&next) {
            dropped += u64::from(!intake.push(&mut chunk, p));
        }
        assert_eq!(deliver(&mut asm, &chunk), vec![30 + 2], "only the next one");
        assert_eq!(asm.dropped(), 1, "the cut dump, at the next START");
        assert_eq!(dropped, 2 + (dump.len() - 5) as u64);
    }

    /// **A whole chunk lost cuts every message open in it**: a chunk not
    /// sent (or refused, or skipped as stale) between a message's start and
    /// its tail leaves the tail skipped, not spliced onto the start; a
    /// message that ended before the lost chunk, and the next one, are not
    /// touched.
    ///
    /// Mutation (run): `lost` not marking the open groups skipped → the tail
    /// completes the message with the lost chunk's packets missing → fails.
    #[test]
    fn a_lost_chunk_cuts_the_message_open_in_it() {
        let whole = packets(MidiGroup::FIRST, 20);
        let cut = packets(MidiGroup::FIRST, 60);
        let next = packets(MidiGroup::FIRST, 12);
        let mut intake = MidiIntake::default();
        let mut asm = SysExAssembler::reserved();

        let mut chunk = MidiEventVec::new();
        for &p in whole.iter().chain(&cut[..3]) {
            assert!(intake.push(&mut chunk, p));
        }
        assert_eq!(deliver(&mut asm, &chunk), vec![20 + 2]);

        let mut lost = MidiEventVec::new();
        for &p in &cut[3..6] {
            assert!(intake.push(&mut lost, p));
        }
        assert_eq!(intake.lose(&mut lost), 3);
        assert!(lost.is_empty());

        let mut chunk = MidiEventVec::new();
        let mut skipped = 0;
        for &p in cut[6..].iter().chain(&next) {
            skipped += usize::from(!intake.push(&mut chunk, p));
        }
        assert_eq!(skipped, cut.len() - 6);
        assert_eq!(deliver(&mut asm, &chunk), vec![12 + 2]);
        assert_eq!(asm.dropped(), 1);
    }

    /// **A lost chunk holding only a message's `START` cuts that message
    /// too.** The plugin's host still has an earlier message open (its source
    /// never ended it), so the lost message's `END` would complete the
    /// earlier one with the lost `START`'s bytes missing.
    ///
    /// Mutation (run): `admit` not recording a taken `START` as open → the
    /// lost `START` closes the group, nothing is cut, and the `END` completes
    /// the earlier message → fails.
    #[test]
    fn a_lost_chunk_holding_only_a_start_cuts_its_message() {
        let unended = packets(MidiGroup::FIRST, 30);
        let lost = packets(MidiGroup::FIRST, 12);
        let next = packets(MidiGroup::FIRST, 20);
        assert_eq!(lost.len(), 2, "a START and an END");
        let mut intake = MidiIntake::default();
        let mut asm = SysExAssembler::reserved();

        let mut chunk = MidiEventVec::new();
        for &p in &unended[..2] {
            assert!(intake.push(&mut chunk, p));
        }
        assert!(deliver(&mut asm, &chunk).is_empty());

        let mut gone = MidiEventVec::new();
        assert!(intake.push(&mut gone, lost[0]));
        assert_eq!(intake.lose(&mut gone), 1);

        let mut chunk = MidiEventVec::new();
        assert!(!intake.push(&mut chunk, lost[1]), "the lost START's END");
        for &p in &next {
            assert!(intake.push(&mut chunk, p));
        }
        assert_eq!(deliver(&mut asm, &chunk), vec![20 + 2], "only the next one");
        assert_eq!(asm.dropped(), 1, "the unended one, at the next START");
    }

    /// **A cut is per group.** A message on another group does not end the
    /// skip of one cut on group 1: group 1's tail stays skipped after a
    /// `SINGLE` on group 2 passes.
    ///
    /// Mutation (run): one bit for every group (`let bit = 1u16;`) → the
    /// group-2 `SINGLE` clears group 1's skip and its tail is taken → fails.
    #[test]
    fn a_cut_on_one_group_survives_a_message_on_another() {
        let cut = packets(MidiGroup::FIRST, 30);
        let single = packets(MidiGroup::of(1), 4);
        assert_eq!(single.len(), 1, "a SINGLE");
        assert_ne!(group_bit(&single[0].ump), group_bit(&cut[0].ump));
        let mut intake = MidiIntake::default();

        let mut lost = MidiEventVec::new();
        for &p in &cut[..2] {
            assert!(intake.push(&mut lost, p));
        }
        assert_eq!(intake.lose(&mut lost), 2);

        let mut chunk = MidiEventVec::new();
        assert!(intake.push(&mut chunk, single[0]), "group 2 passes");
        for &p in &cut[2..] {
            assert!(!intake.push(&mut chunk, p), "group 1's tail stays cut");
        }
        assert_eq!(chunk.as_slice(), single.as_slice());
    }

    fn on(key: u8) -> WireMidi {
        wire(midi::note_on(
            MidiGroup::FIRST,
            MidiChannel::FIRST,
            key,
            0xFFFF,
        ))
    }

    fn off(key: u8) -> WireMidi {
        wire(midi::note_off(MidiGroup::FIRST, MidiChannel::FIRST, key, 0))
    }

    /// A list full of note-ons takes a note-off over the note-on of its own
    /// key, and a note-on is refused. 256 note-ons keys 0..=127 twice over:
    /// a note-off of key 5 displaces the later note-on of 5, one is lost; a
    /// further note-on is refused, one lost.
    ///
    /// Mutation: drop the `is_note_off` test in `push_displacing` → the
    /// note-on displaces too, and the list loses a note-on it kept → fails.
    /// Mutation: take the first note-on rather than the key's → the wrong
    /// note-on goes → fails.
    #[test]
    fn a_note_off_displaces_a_note_on_of_its_key_in_a_full_list() {
        let mut intake = MidiIntake::default();
        let mut list = MidiEventVec::new();
        for i in 0..MIDI_STACK_CAPACITY {
            assert_eq!(intake.push_displacing(&mut list, on((i % 128) as u8)), 0);
        }
        assert_eq!(intake.push_displacing(&mut list, on(9)), 1);
        assert_eq!(list.len(), MIDI_STACK_CAPACITY);
        assert_eq!(
            list.last(),
            Some(&on(127)),
            "a refused note-on displaces nothing"
        );
        assert!(list.iter().all(|e| is_on(&e.ump.0)));
        assert_eq!(intake.push_displacing(&mut list, off(5)), 1);
        assert_eq!(list.len(), MIDI_STACK_CAPACITY);
        assert_eq!(list.last(), Some(&off(5)));
        // The later note-on of key 5 (index 133) is the one that went.
        let ons_of_5 = list
            .iter()
            .filter(|e| is_on(&e.ump.0) && key_of(&e.ump.0).2 == 5)
            .count();
        assert_eq!(ons_of_5, 1);
        assert_eq!(list[5], on(5));
    }

    /// The worst case is bounded: a full list of 256 note-ons met by 8,192
    /// note-offs displaces 256 of them (every note-on, key by key) and
    /// refuses the rest, 7,936, in constant time after one empty pass.
    /// Counted: every off beyond the list's 256 is one lost event or one
    /// refused, 8,192 in all.
    ///
    /// Mutation: leave `exhausted` unset → 8,192 passes instead of 257 → fails. Mutation: displace without removing the
    /// victim → the list overflows `try_push` and the count changes → fails.
    #[test]
    fn a_flood_of_note_offs_is_bounded_and_counted() {
        let mut intake = MidiIntake::default();
        let mut list = MidiEventVec::new();
        for i in 0..MIDI_STACK_CAPACITY {
            assert_eq!(intake.push_displacing(&mut list, on((i % 128) as u8)), 0);
        }
        let start = std::time::Instant::now();
        let mut lost = 0;
        for i in 0..8_192usize {
            lost += intake.push_displacing(&mut list, off((i % 128) as u8));
        }
        let took = start.elapsed();
        assert_eq!(lost, 8_192);
        assert_eq!(list.iter().filter(|e| is_on(&e.ump.0)).count(), 0);
        assert_eq!(list.len(), MIDI_STACK_CAPACITY);
        // 256 passes that displace and one that finds nothing: not 8,192.
        assert_eq!(intake.scans, 257);
        // Release builds take about 0.2 ms for all of it; this is a guard
        // against a gross regression on a loaded debug build only.
        assert!(took < std::time::Duration::from_millis(100), "{took:?}");
        // A new chunk may displace again.
        list.clear();
        intake.new_chunk();
        for i in 0..MIDI_STACK_CAPACITY {
            let _ = intake.push_displacing(&mut list, on((i % 128) as u8));
        }
        assert_eq!(intake.push_displacing(&mut list, off(5)), 1);
        assert_eq!(list.last(), Some(&off(5)));
    }
}
