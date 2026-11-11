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
//! [`MIDI_STACK_CAPACITY`]: tutti_plugin_types::MIDI_STACK_CAPACITY

use tutti_ump::{Key, KeyEvent, Message, SysexStatus, Ump, decode};

use crate::protocol::{MidiEventVec, WireMidi};

/// The intake's state, one bit per UMP group, carried across chunks and
/// blocks, and across a reset (the reassembler past it keeps its open run
/// across the node's reset: neither the plugin-server nor the in-process
/// VST2 node resets it).
#[derive(Clone, Copy, Debug, Default)]
pub struct MidiIntake {
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
    pub fn admit(&mut self, event: &WireMidi) -> bool {
        let Some(status) = sysex7_status(&event.ump) else {
            return true;
        };
        let bit = group_bit(&event.ump);
        match status {
            SysexStatus::Start | SysexStatus::Complete => self.skipping &= !bit,
            SysexStatus::Continue | SysexStatus::End if self.skipping & bit != 0 => {
                if status == SysexStatus::End {
                    self.skipping &= !bit;
                }
                return false;
            }
            _ => {}
        }
        match status {
            SysexStatus::Start | SysexStatus::Continue => self.open |= bit,
            _ => self.open &= !bit,
        }
        true
    }

    /// Push `event` onto `list`, or drop it: past the list's cap, or as the
    /// rest of a SysEx that already lost a packet. Returns whether it was
    /// taken; the caller counts what was not
    /// (`DeliveryCounters::count_midi_dropped` in `tutti-plugin`). Never allocates.
    #[must_use = "a dropped event is counted by the caller"]
    pub fn push(&mut self, list: &mut MidiEventVec, event: WireMidi) -> bool {
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
            Some(SysexStatus::Start | SysexStatus::Continue)
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
    pub fn push_displacing(&mut self, list: &mut MidiEventVec, event: WireMidi) -> u64 {
        if self.push(list, event) {
            return 0;
        }
        let Some(KeyEvent::Off(off)) = event.ump.key_event() else {
            return 1;
        };
        if self.exhausted || !list.is_full() {
            return 1;
        }
        #[cfg(test)]
        {
            self.scans += 1;
        }
        let mut latest_on = None;
        let mut latest_other = None;
        let mut victim = None;
        for (i, e) in list.iter().enumerate().rev() {
            match e.ump.key_event() {
                Some(KeyEvent::On(on)) => {
                    if same_note(on, off) {
                        victim = Some(i);
                        break;
                    }
                    latest_on.get_or_insert(i);
                }
                Some(KeyEvent::Off(_)) => {}
                _ if e.ump.address().is_some() => {
                    latest_other.get_or_insert(i);
                }
                _ => {}
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
    pub fn new_chunk(&mut self) {
        self.exhausted = false;
    }

    /// Everything passed on since the last chunk that arrived was lost
    /// (a whole chunk's MIDI never reached the reassembler, or the node was
    /// reset): skip the rest of every message open in it. Never allocates.
    pub fn lost(&mut self) {
        self.skipping |= self.open;
        self.open = 0;
    }

    /// `list`, taken through this intake, never reaches the plugin (a chunk
    /// not sent, or the events taken towards the next chunk at a reset):
    /// empty it, skip the rest of every message open in it or before it
    /// ([`lost`](Self::lost)), and return how many events it held, for the
    /// caller to count. Never allocates.
    #[must_use = "the lost events are counted by the caller"]
    pub fn lose(&mut self, list: &mut MidiEventVec) -> u64 {
        let n = list.len() as u64;
        list.clear();
        self.exhausted = false;
        self.lost();
        n
    }
}

/// Whether `a` and `b` are the same note: group, channel and number, whichever
/// protocol spelled each (a MIDI 1.0 off ends a MIDI 2.0 on).
fn same_note(a: Key, b: Key) -> bool {
    (a.lane(), a.note()) == (b.lane(), b.note())
}

/// A SysEx7 packet's status, or `None` for any other message.
fn sysex7_status(event: &Ump) -> Option<SysexStatus> {
    match decode(*event) {
        Ok(Message::Sysex7(_, packet)) => Some(packet.status()),
        _ => None,
    }
}

/// The bit of a SysEx7 packet's group in the intake's per-group masks.
fn group_bit(event: &Ump) -> u16 {
    event.group().map_or(0, |group| 1 << group.get())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_fixtures::midi;
    use tutti_plugin_types::{MIDI_STACK_CAPACITY, sysex_assembler};
    use tutti_ump::{MidiChannel, MidiGroup, SysexAssembler, SysexStep};

    fn wire(ump: Ump) -> WireMidi {
        WireMidi { offset: 0, ump }
    }

    fn packets(group: MidiGroup, len: usize) -> Vec<WireMidi> {
        let payload: Vec<tutti_ump::U7> = (0..len)
            .map(|i| tutti_ump::U7::of((i % 0x7F) as u8))
            .collect();
        tutti_ump::sysex7_packets(group, &payload)
            .map(wire)
            .collect()
    }

    fn note() -> WireMidi {
        wire(midi::note_on(
            MidiGroup::FIRST,
            MidiChannel::FIRST,
            60,
            midi::MID_VELOCITY,
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
    fn deliver(asm: &mut SysexAssembler<Box<[u8]>>, chunk: &[WireMidi]) -> Vec<usize> {
        asm.begin_block();
        chunk
            .iter()
            .filter_map(|e| match asm.push(e.ump) {
                SysexStep::Complete(m) => Some(m.len()),
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
        let mut asm = sysex_assembler();
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
        let mut asm = sysex_assembler();

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
        let mut asm = sysex_assembler();

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
        assert!(
            list.iter()
                .all(|e| matches!(e.ump.key_event(), Some(KeyEvent::On(_))))
        );
        assert_eq!(intake.push_displacing(&mut list, off(5)), 1);
        assert_eq!(list.len(), MIDI_STACK_CAPACITY);
        assert_eq!(list.last(), Some(&off(5)));
        // The later note-on of key 5 (index 133) is the one that went.
        let ons_of_5 = list
            .iter()
            .filter(|e| matches!(e.ump.key_event(), Some(KeyEvent::On(k)) if k.note() == 5))
            .count();
        assert_eq!(ons_of_5, 1);
        assert_eq!(list[5], on(5));
    }

    /// A MIDI 1.0 note-off ends a MIDI 2.0 note-on of the same group, channel
    /// and number: it displaces that note's on, not the latest one.
    ///
    /// Mutation (run): `same_note` comparing whole `Key`s (the protocol bit
    /// too) → the latest note-on goes instead → fails.
    #[test]
    fn a_midi1_note_off_displaces_its_midi2_note_on() {
        let mut intake = MidiIntake::default();
        let mut list = MidiEventVec::new();
        for i in 0..MIDI_STACK_CAPACITY {
            assert_eq!(intake.push_displacing(&mut list, on((i % 128) as u8)), 0);
        }
        // A MIDI 1.0 Note Off (type 2, status 0x80), group 0, channel 0, key 5.
        let m1_off = wire(Ump([0x2080_0000 | (5 << 8), 0, 0, 0]));
        assert_eq!(intake.push_displacing(&mut list, m1_off), 1);
        assert_eq!(list.last(), Some(&m1_off));
        assert_eq!(
            list[MIDI_STACK_CAPACITY - 2],
            on(127),
            "the latest on stays"
        );
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
        assert_eq!(
            list.iter()
                .filter(|e| matches!(e.ump.key_event(), Some(KeyEvent::On(_))))
                .count(),
            0
        );
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
