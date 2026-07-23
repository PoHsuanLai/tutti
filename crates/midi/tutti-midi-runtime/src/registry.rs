//! Lock-free MIDI event delivery primitives.
//!
//! [`MidiMailbox`] is the underlying lock-free ring buffer. Each
//! MIDI-receiving audio unit owns a [`MidiReceiver`] half and hands out
//! cheap-to-clone [`MidiSender`] handles to anyone that wants to push events.
//!
//! [`MidiBus`] is a fan-out `DashMap` keyed by [`MidiUnitId`] that holds
//! sender clones and lets the caller queue by id. Standalone consumers of this
//! crate don't have to use it — wiring individual [`MidiSender`]s into
//! your own [`tutti_midi_types::MidiOut`] impl works just as well — but the
//! `tutti` engine installs a [`MidiBus`] as its audio-thread dispatch
//! target so apps can register node senders without writing any glue.
//!
//! Delivery here is *inbound only*: a mailbox feeds a unit that polls it.
//! Protocol messages aimed at peer devices (MIDI-CI, UMP-Stream discovery,
//! Flex metadata) are not delivered through this bus — they go to the
//! hardware-out mailbox (`tutti_midi_io::MidiOutRes`), the only path drained to
//! the wire.

use std::sync::Arc;

use arc_swap::ArcSwap;
use crossbeam_queue::ArrayQueue;
use dashmap::DashMap;
use parking_lot::Mutex;

use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::MidiUnitId;

use crate::mpe::{MpeProcessor, PerNoteExpression};
use crate::snapshot::MidiSnapshot;

const EVENTS_PER_UNIT: usize = 256;

/// Lock-free per-unit MIDI inbox.
///
/// One bounded ring buffer. Push and pop are wait-free, so they're safe to call
/// from the audio thread.
pub struct MidiMailbox {
    events: ArrayQueue<MidiEvent>,
}

impl std::fmt::Debug for MidiMailbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Report occupancy, not contents — the queue is drained on the audio
        // thread and dumping it would be both racy and noisy.
        f.debug_struct("MidiMailbox")
            .field("events", &self.events.len())
            .finish()
    }
}

impl MidiMailbox {
    fn new() -> Self {
        Self {
            events: ArrayQueue::new(EVENTS_PER_UNIT),
        }
    }

    /// Allocate a new slot and return a paired sender + receiver.
    ///
    /// The `unit_id` is baked into both halves so the sender doesn't need
    /// the caller to repeat it on every push. Use [`MidiUnitId::next`] for
    /// a fresh process-unique id.
    pub fn pair(unit_id: MidiUnitId) -> (MidiSender, MidiReceiver) {
        let slot = Arc::new(Self::new());
        (
            MidiSender {
                slot: slot.clone(),
                unit_id,
            },
            MidiReceiver { slot, unit_id },
        )
    }
}

/// Producer handle for a [`MidiMailbox`]. Cheap to clone.
///
/// Implements [`tutti_midi_types::MidiOut`], so MIDI input drivers, sequencers,
/// or arbitrary user code can all push events through the same trait.
#[derive(Clone, Debug)]
pub struct MidiSender {
    slot: Arc<MidiMailbox>,
    unit_id: MidiUnitId,
}

impl MidiSender {
    pub fn unit_id(&self) -> MidiUnitId {
        self.unit_id
    }

    /// Push events into the slot. Audio-thread-safe.
    ///
    /// Returns how many were accepted — `< events.len()` means the 256-slot ring
    /// was full and the rest were dropped. Dropping a note-off while its note-on
    /// landed is what produces a stuck note, so a caller that cares (e.g. a dense
    /// live-input burst) can check the count and back off or warn. `queue` never
    /// blocks, so a full ring drops rather than stalls the audio thread.
    pub fn queue(&self, events: &[MidiEvent]) -> usize {
        let mut accepted = 0;
        for &event in events {
            if self.slot.events.push(event).is_err() {
                break; // ring full; the ArrayQueue stays FIFO, so stop here
            }
            accepted += 1;
        }
        accepted
    }

    /// Convenience: send a MIDI 1.0 note-on (velocity is 7-bit). Returns `false`
    /// if the ring was full and the note was dropped.
    pub fn note_on(&self, channel: u8, note: u8, velocity: u8) -> bool {
        self.slot
            .events
            .push(MidiEvent::note_on_7bit(0, channel, note, velocity))
            .is_ok()
    }

    /// Convenience: send a MIDI 1.0 note-off. Returns `false` if the ring was
    /// full and the note-off was dropped (which would leave a stuck note).
    pub fn note_off(&self, channel: u8, note: u8) -> bool {
        self.slot
            .events
            .push(MidiEvent::note_off(0, channel, note, 0))
            .is_ok()
    }
}

impl tutti_midi_types::MidiOut for MidiSender {
    fn queue(&self, events: &[MidiEvent]) {
        self.queue(events);
    }
}

/// Consumer handle for a [`MidiMailbox`]. Owned by the audio unit.
///
/// Implements [`tutti_midi_types::MidiIn`] so it can plug into any node that
/// polls events through the trait.
///
/// Cloneable via [`Clone`] to support fundsp graph commits that duplicate
/// nodes — the clone shares the same underlying slot so events queued
/// through any [`MidiSender`] for the unit continue to reach whichever
/// receiver is currently being polled. Polling from multiple receivers in
/// parallel races: keep only one live at a time per slot.
#[derive(Clone, Debug)]
pub struct MidiReceiver {
    slot: Arc<MidiMailbox>,
    unit_id: MidiUnitId,
}

impl MidiReceiver {
    pub fn unit_id(&self) -> MidiUnitId {
        self.unit_id
    }

    /// Drain events into a caller-owned buffer. Returns the number written.
    /// Allocation-free, audio-thread-safe.
    pub fn poll_into(&self, out: &mut [MidiEvent]) -> usize {
        let mut count = 0;
        for slot_ref in out.iter_mut() {
            match self.slot.events.pop() {
                Some(event) => {
                    *slot_ref = event;
                    count += 1;
                }
                None => break,
            }
        }
        count
    }

    pub fn has_events(&self) -> bool {
        !self.slot.events.is_empty()
    }

    pub fn clear(&self) {
        while self.slot.events.pop().is_some() {}
    }

    /// Drain pending events into a snapshot at the given beat position.
    pub fn drain_into_snapshot(&self, snapshot: &mut MidiSnapshot, beat: f64) {
        while let Some(event) = self.slot.events.pop() {
            snapshot.add_event(self.unit_id, beat, event);
        }
    }
}

impl tutti_midi_types::MidiIn for MidiReceiver {
    fn poll_into(
        &self,
        unit_id: MidiUnitId,
        _block_size: usize,
        out: &mut [MidiEvent],
    ) -> usize {
        if unit_id != self.unit_id {
            return 0;
        }
        // Live producers (MIDI input drivers, panel previews) are
        // responsible for stamping `frame_offset` when they push into
        // the slot. We pass through whatever they set.
        self.poll_into(out)
    }
}

/// MIDI fan-out bus: a bag of [`MidiSender`]s keyed by [`MidiUnitId`].
///
/// Use when you have many MIDI-receiving nodes and want to address them
/// through a single object — for example, to feed hardware MIDI input
/// through a routing table that targets nodes by id.
///
/// The `tutti` engine wires one of these as its default audio-thread
/// dispatch target, exposed as `engine.midi`. Standalone consumers of
/// this crate can pick any [`tutti_midi_types::MidiOut`] impl instead —
/// a single [`MidiSender`], a custom routing struct, anything with a
/// `queue(MidiUnitId, &[MidiEvent])` method.
///
/// Delivery is *addressed only*: this bus feeds units that poll their inbox.
/// Messages destined for external peer devices go to the hardware-out mailbox
/// (`tutti_midi_io::MidiOutRes`) instead — a synth inbox is not a wire.
///
/// # Real-time safety
///
/// [`queue`](MidiBus::queue) reads the DashMap (O(1) hash lookup, brief shard
/// read-lock) — keep addressed delivery on the fast path and avoid
/// `insert`/`remove` at playback time.
#[derive(Clone, Default)]
pub struct MidiBus {
    senders: Arc<DashMap<MidiUnitId, MidiSender>>,
    /// Optional MPE processor: every event passing through `queue` is also fed
    /// to this processor before delivery, populating the per-note
    /// expression atomics that voices read on the audio thread.
    /// `None` = MPE disabled (the default).
    ///
    /// The `Mutex` is taken with `try_lock` on the audio-thread feed path
    /// (see [`feed_mpe`](MidiBus::feed_mpe)) so the RT thread never blocks on
    /// it; the only other locker is the off-RT `process`-mutation itself.
    mpe: Arc<ArcSwap<Option<Arc<Mutex<MpeProcessor>>>>>,
    /// The installed processor's `PerNoteExpression`, published separately so
    /// [`mpe_expression`](MidiBus::mpe_expression) reads it with a single
    /// atomic load — without locking the processor (which the audio thread
    /// may be holding). `None` = MPE disabled.
    mpe_expression: Arc<ArcSwap<Option<Arc<PerNoteExpression>>>>,
}

impl std::fmt::Debug for MidiBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MidiBus")
            .field("subscribers", &self.senders.len())
            .field("mpe_enabled", &self.mpe.load().is_some())
            .finish()
    }
}

impl MidiBus {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install an MPE processor. Every event going through `queue` will be fed
    /// to the processor before being delivered to subscribers.
    ///
    /// Replaces any previously-installed processor. Returns the
    /// processor's `Arc<PerNoteExpression>` so callers (typically
    /// bevy-tutti's `mpe_setup_system`) can hand it to readers without
    /// touching the bus again.
    pub fn install_mpe(&self, processor: MpeProcessor) -> Arc<PerNoteExpression> {
        let expression = processor.expression();
        // Publish the expression separately *before* the processor, so any
        // concurrent `mpe_expression()` reader never has to lock the processor.
        self.mpe_expression
            .store(Arc::new(Some(Arc::clone(&expression))));
        self.mpe
            .store(Arc::new(Some(Arc::new(Mutex::new(processor)))));
        expression
    }

    /// Read-only access to the live `PerNoteExpression`, if MPE is installed.
    /// Lock-free: a single atomic load + `Arc` clone. Does **not** lock the
    /// processor (which the audio-thread feed may hold), so a UI-thread caller
    /// can never stall the RT thread by reading the expression handle.
    pub fn mpe_expression(&self) -> Option<Arc<PerNoteExpression>> {
        self.mpe_expression.load().as_ref().as_ref().map(Arc::clone)
    }

    /// Uninstall the MPE processor. Subsequent events bypass the feed.
    pub fn uninstall_mpe(&self) {
        self.mpe.store(Arc::new(None));
        self.mpe_expression.store(Arc::new(None));
    }

    /// If MPE is installed, feed an event to the processor.
    ///
    /// Uses `try_lock`: this runs on the audio thread (`MidiBus::queue` is the
    /// engine's RT dispatch target), so it must never block. The only
    /// competing locker is `process`-mutation itself; under the rare
    /// contention window this skips the feed for one event rather than
    /// stalling the RT thread — a dropped expression update is far cheaper
    /// than an audio dropout, and the next event re-syncs the state.
    /// No-op when no MPE processor is installed (the default).
    #[inline]
    fn feed_mpe(&self, event: &MidiEvent) {
        if let Some(processor) = self.mpe.load().as_ref().as_ref() {
            if let Some(mut guard) = processor.try_lock() {
                guard.process(event);
            }
        }
    }

    /// Attach (or replace) the sender for a unit id.
    pub fn insert(&self, sender: MidiSender) {
        self.senders.insert(sender.unit_id(), sender);
    }

    /// Drop the sender for a unit id. Subsequent queues for it are no-ops.
    pub fn remove(&self, unit_id: MidiUnitId) {
        self.senders.remove(&unit_id);
    }

    /// Queue events for a subscribed unit. Unknown ids are silently dropped.
    pub fn queue(&self, unit_id: MidiUnitId, events: &[MidiEvent]) {
        // Feed MPE first so the per-note state is current before
        // voices receive the event and start producing audio.
        for event in events {
            self.feed_mpe(event);
        }
        if let Some(sender) = self.senders.get(&unit_id) {
            sender.queue(events);
        }
    }

    /// True if the bus has a subscriber for the unit id.
    pub fn contains(&self, unit_id: MidiUnitId) -> bool {
        self.senders.contains_key(&unit_id)
    }

    pub fn len(&self) -> usize {
        self.senders.len()
    }

    pub fn is_empty(&self) -> bool {
        self.senders.is_empty()
    }

}

impl tutti_midi_types::MidiRouter for MidiBus {
    fn queue(&self, unit_id: MidiUnitId, events: &[MidiEvent]) {
        self.queue(unit_id, events);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note_on(note: u8, vel_u7: u8) -> MidiEvent {
        MidiEvent::note_on(
            0,
            0,
            note,
            tutti_midi_types::convert::midi1_velocity_to_midi2(vel_u7),
        )
    }

    fn note_off(note: u8) -> MidiEvent {
        MidiEvent::note_off(0, 0, note, 0)
    }

    #[test]
    fn sender_pushes_to_receiver() {
        let id = MidiUnitId::new(12345);
        let (sender, receiver) = MidiMailbox::pair(id);

        sender.queue(&[note_on(60, 100), note_off(60)]);
        assert!(receiver.has_events());

        let mut buf = [note_on(0, 0); 16];
        let n = receiver.poll_into(&mut buf);
        assert_eq!(n, 2);
        assert!(!receiver.has_events());
    }

    #[test]
    fn cloned_sender_pushes_to_same_receiver() {
        let (sender, receiver) = MidiMailbox::pair(MidiUnitId::next());
        let s2 = sender.clone();

        sender.queue(&[note_on(60, 100)]);
        s2.queue(&[note_on(64, 100)]);

        let mut buf = [note_on(0, 0); 16];
        assert_eq!(receiver.poll_into(&mut buf), 2);
    }

    #[test]
    fn sender_note_helpers_match_explicit_events() {
        let (sender, receiver) = MidiMailbox::pair(MidiUnitId::next());
        sender.note_on(0, 60, 100);
        sender.note_off(0, 60);

        let mut buf = [note_on(0, 0); 4];
        assert_eq!(receiver.poll_into(&mut buf), 2);
    }

    #[test]
    fn bus_queue_reaches_the_addressed_unit() {
        let bus = MidiBus::new();
        let unit = MidiUnitId::new(1);
        let (s, r) = MidiMailbox::pair(unit);
        bus.insert(s);

        bus.queue(unit, &[MidiEvent::note_on_7bit(0, 0, 60, 100)]);
        bus.queue(unit, &[MidiEvent::note_off(0, 0, 60, 0)]);

        let mut buf = [MidiEvent::noop(); 4];
        assert_eq!(r.poll_into(&mut buf), 2);
        assert!(buf[0].is_note_on());
        assert_eq!(buf[0].note(), Some(60));
        assert!(buf[1].is_note_off());
    }

    #[test]
    fn dropped_receiver_keeps_sender_pushable() {
        // Sender holds an Arc to the slot; dropping the receiver doesn't
        // invalidate pushes. Events accumulate in the slot until the sender
        // is also dropped.
        let (sender, receiver) = MidiMailbox::pair(MidiUnitId::next());
        drop(receiver);
        sender.queue(&[note_on(60, 100)]);
        // No assertion possible without the receiver — but no crash is the test.
    }

    #[test]
    fn back_pressure_drops_when_full() {
        let (sender, receiver) = MidiMailbox::pair(MidiUnitId::next());
        let events: Vec<_> = (0..512).map(|i| note_on((i % 128) as u8, 100)).collect();
        sender.queue(&events);

        let mut buf = [note_on(0, 0); 512];
        let n = receiver.poll_into(&mut buf);
        assert!(n <= EVENTS_PER_UNIT);
        assert!(n > 0);
    }

    #[test]
    fn receiver_drains_into_snapshot() {
        let id = MidiUnitId::new(7);
        let (sender, receiver) = MidiMailbox::pair(id);
        sender.queue(&[note_on(60, 100), note_off(60)]);

        let mut snap = MidiSnapshot::new();
        receiver.drain_into_snapshot(&mut snap, 0.0);
        assert!(snap.has_events(id));
    }

    #[test]
    fn bus_routes_by_unit_id() {
        let bus = MidiBus::new();
        let id1 = MidiUnitId::new(1);
        let id2 = MidiUnitId::new(2);
        let (s1, r1) = MidiMailbox::pair(id1);
        let (s2, r2) = MidiMailbox::pair(id2);
        bus.insert(s1);
        bus.insert(s2);

        bus.queue(id1, &[note_on(60, 100)]);
        bus.queue(id2, &[note_on(64, 100)]);

        let mut buf = [note_on(0, 0); 4];
        assert_eq!(r1.poll_into(&mut buf), 1);
        assert_eq!(r2.poll_into(&mut buf), 1);
    }

    #[test]
    fn bus_queue_for_unknown_unit_is_silent() {
        let bus = MidiBus::new();
        bus.queue(MidiUnitId::new(999), &[note_on(60, 100)]);
        // No panic, no allocation, no observable effect.
    }

    #[test]
    fn bus_remove_drops_routing() {
        let bus = MidiBus::new();
        let id = MidiUnitId::new(42);
        let (sender, receiver) = MidiMailbox::pair(id);
        bus.insert(sender);
        bus.remove(id);

        bus.queue(id, &[note_on(60, 100)]);
        let mut buf = [note_on(0, 0); 4];
        assert_eq!(receiver.poll_into(&mut buf), 0);
    }

    #[test]
    fn bus_mpe_feed_updates_per_note_expression() {
        // Install MPE on the bus, queue a note-on + pitch-bend, verify
        // the per-note expression atomic reflects the bend.
        use crate::mpe::{MpeMode, MpeProcessor, MpeZoneConfig};
        use tutti_midi_types::convert::midi1_pitch_bend_to_midi2;
        use tutti_midi_types::NoteId;

        let bus = MidiBus::new();
        let processor = MpeProcessor::new(MpeMode::LowerZone(MpeZoneConfig::lower(15)));
        let expression = bus.install_mpe(processor);

        // Subscribe a unit so the bus has somewhere to deliver events.
        let id = MidiUnitId::new(1);
        let (sender, _receiver) = MidiMailbox::pair(id);
        bus.insert(sender);

        // Note on, channel 2 (a member channel of the lower zone).
        let note_on = MidiEvent::note_on(
            0,
            2,
            60,
            tutti_midi_types::convert::midi1_velocity_to_midi2(100),
        );
        // Channel pitch-bend on channel 2 — under MPE classic mapping
        // this routes to note 60.
        let bend = MidiEvent::pitch_bend(0, 2, midi1_pitch_bend_to_midi2(16383));

        bus.queue(id, &[note_on, bend]);

        let n60 = NoteId::from_channel_note(2, 60);
        let actual = expression.get_pitch_bend(n60);
        assert!(
            (actual - 1.0).abs() < 0.01,
            "expected pitch_bend ≈ 1.0 after bend, got {actual}"
        );
        assert!(expression.is_active(n60), "note 60 should be active");
    }

    #[test]
    fn bus_mpe_expression_readable_without_processor_lock() {
        // mpe_expression() must read the published expression handle via the
        // separate ArcSwap, NOT by locking the processor — so it stays
        // readable even while the processor mutex is held (which on the audio
        // thread it transiently is). We can't easily hold the RT lock from a
        // test, but we can at least assert install publishes it and uninstall
        // clears it, and that the returned handle is the same one install gave.
        use crate::mpe::{MpeMode, MpeProcessor, MpeZoneConfig};

        let bus = MidiBus::new();
        assert!(bus.mpe_expression().is_none());

        let processor = MpeProcessor::new(MpeMode::LowerZone(MpeZoneConfig::lower(15)));
        let installed = bus.install_mpe(processor);
        let read_back = bus
            .mpe_expression()
            .expect("expression published on install");
        assert!(
            Arc::ptr_eq(&installed, &read_back),
            "mpe_expression() must hand back the same handle install_mpe returned"
        );

        bus.uninstall_mpe();
        assert!(
            bus.mpe_expression().is_none(),
            "uninstall clears the expression"
        );
    }

    #[test]
    fn bus_mpe_uninstall_stops_feed() {
        use crate::mpe::{MpeMode, MpeProcessor, MpeZoneConfig};
        use tutti_midi_types::NoteId;

        let bus = MidiBus::new();
        let processor = MpeProcessor::new(MpeMode::LowerZone(MpeZoneConfig::lower(15)));
        let expression = bus.install_mpe(processor);
        bus.uninstall_mpe();

        let id = MidiUnitId::new(1);
        let (sender, _receiver) = MidiMailbox::pair(id);
        bus.insert(sender);

        // Without MPE installed, queueing a note-on shouldn't update
        // the expression atomics.
        let note_on = MidiEvent::note_on(
            0,
            2,
            60,
            tutti_midi_types::convert::midi1_velocity_to_midi2(100),
        );
        bus.queue(id, &[note_on]);

        assert!(
            !expression.is_active(NoteId::from_channel_note(2, 60)),
            "note should not register after uninstall"
        );
    }

    #[test]
    fn bus_mpe_disabled_by_default_no_overhead() {
        // No processor installed → `feed_mpe` is a single ArcSwap load
        // and a None-check. Confirm the bus still routes events.
        let bus = MidiBus::new();
        assert!(bus.mpe_expression().is_none());

        let id = MidiUnitId::new(1);
        let (sender, receiver) = MidiMailbox::pair(id);
        bus.insert(sender);

        let note_on = MidiEvent::note_on(
            0,
            0,
            60,
            tutti_midi_types::convert::midi1_velocity_to_midi2(100),
        );
        bus.queue(id, &[note_on]);

        assert!(receiver.has_events());
    }
}
