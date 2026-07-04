//! Lock-free MIDI event delivery primitives.
//!
//! [`MidiEventSlot`] is the underlying lock-free ring-buffer pair (one queue
//! for routed channel/voice events, one for broadcast system events). Each
//! MIDI-receiving audio unit owns a [`MidiReceiver`] half and hands out
//! cheap-to-clone [`MidiSender`] handles to anyone that wants to push events.
//!
//! [`MidiBus`] is a fan-out `DashMap` keyed by [`MidiUnitId`] that holds
//! sender clones, lets the caller queue by id, and broadcasts system
//! events to every registered sender. Standalone consumers of this
//! crate don't have to use it — wiring individual [`MidiSender`]s into
//! your own [`tutti_midi_types::MidiQueue`] impl works just as well — but the
//! `tutti` engine installs a [`MidiBus`] as its audio-thread dispatch
//! target so apps can register node senders without writing any glue.

use std::sync::Arc;

use arc_swap::ArcSwap;
use crossbeam_queue::ArrayQueue;
use dashmap::DashMap;
#[cfg(feature = "mpe")]
use parking_lot::Mutex;

use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::MidiUnitId;

#[cfg(feature = "mpe")]
use crate::mpe::{MpeProcessor, PerNoteExpression};
use crate::snapshot::MidiSnapshot;

const EVENTS_PER_UNIT: usize = 256;
const SYSTEM_EVENTS_PER_UNIT: usize = 64;

/// Lock-free per-unit MIDI inbox.
///
/// Holds two bounded ring buffers — one for routed channel/voice events and
/// one for broadcast system messages. Push and pop are wait-free, so they're
/// safe to call from the audio thread.
pub struct MidiEventSlot {
    events: ArrayQueue<MidiEvent>,
    sys_events: ArrayQueue<MidiEvent>,
}

impl MidiEventSlot {
    fn new() -> Self {
        Self {
            events: ArrayQueue::new(EVENTS_PER_UNIT),
            sys_events: ArrayQueue::new(SYSTEM_EVENTS_PER_UNIT),
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

/// Producer handle for a [`MidiEventSlot`]. Cheap to clone.
///
/// Implements [`tutti_midi_types::MidiQueue`], so MIDI input drivers, sequencers,
/// or arbitrary user code can all push events through the same trait.
#[derive(Clone)]
pub struct MidiSender {
    slot: Arc<MidiEventSlot>,
    unit_id: MidiUnitId,
}

impl MidiSender {
    pub fn unit_id(&self) -> MidiUnitId {
        self.unit_id
    }

    /// Push events into the slot. Audio-thread-safe; full slots silently drop.
    pub fn queue(&self, events: &[MidiEvent]) {
        for &event in events {
            let _ = self.slot.events.push(event);
        }
    }

    /// Push a single system event (clock, start/stop, addressed SysEx).
    pub fn queue_system(&self, event: &MidiEvent) {
        let _ = self.slot.sys_events.push(*event);
    }

    /// Convenience: send a MIDI 1.0 note-on (velocity is 7-bit).
    pub fn note_on(&self, channel: u8, note: u8, velocity: u8) {
        let event = MidiEvent::note_on_7bit(0, channel, note, velocity);
        let _ = self.slot.events.push(event);
    }

    /// Convenience: send a MIDI 1.0 note-off.
    pub fn note_off(&self, channel: u8, note: u8) {
        let event = MidiEvent::note_off(0, channel, note, 0);
        let _ = self.slot.events.push(event);
    }
}

impl tutti_midi_types::MidiQueue for MidiSender {
    fn queue(&self, unit_id: MidiUnitId, events: &[MidiEvent]) {
        if unit_id != self.unit_id {
            return;
        }
        self.queue(events);
    }
}

/// Consumer handle for a [`MidiEventSlot`]. Owned by the audio unit.
///
/// Implements [`tutti_midi_types::MidiSource`] so it can plug into any node that
/// polls events through the trait.
///
/// Cloneable via [`Clone`] to support fundsp graph commits that duplicate
/// nodes — the clone shares the same underlying slot so events queued
/// through any [`MidiSender`] for the unit continue to reach whichever
/// receiver is currently being polled. Polling from multiple receivers in
/// parallel races: keep only one live at a time per slot.
#[derive(Clone)]
pub struct MidiReceiver {
    slot: Arc<MidiEventSlot>,
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

    /// Drain pending system events. Audio-thread-safe.
    pub fn poll_system(&self, out: &mut [MidiEvent]) -> usize {
        let mut written = 0;
        while written < out.len() {
            match self.slot.sys_events.pop() {
                Some(event) => {
                    out[written] = event;
                    written += 1;
                }
                None => break,
            }
        }
        written
    }

    pub fn has_events(&self) -> bool {
        !self.slot.events.is_empty()
    }

    pub fn has_system_events(&self) -> bool {
        !self.slot.sys_events.is_empty()
    }

    pub fn clear(&self) {
        while self.slot.events.pop().is_some() {}
        while self.slot.sys_events.pop().is_some() {}
    }

    /// Drain pending events into a snapshot at the given beat position.
    pub fn drain_into_snapshot(&self, snapshot: &mut MidiSnapshot, beat: f64) {
        while let Some(event) = self.slot.events.pop() {
            snapshot.add_event(self.unit_id, beat, event);
        }
    }
}

impl tutti_midi_types::MidiSource for MidiReceiver {
    fn poll_into(
        &self,
        unit_id: MidiUnitId,
        _block_start_sample: u64,
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
/// through a routing table that targets nodes by id, or to broadcast
/// system real-time messages (clock, start/stop) to everything at once.
///
/// The `tutti` engine wires one of these as its default audio-thread
/// dispatch target, exposed as `engine.midi`. Standalone consumers of
/// this crate can pick any [`tutti_midi_types::MidiQueue`] impl instead —
/// a single [`MidiSender`], a custom routing struct, anything with a
/// `queue(MidiUnitId, &[MidiEvent])` method.
///
/// # Real-time safety
///
/// [`queue_system`](MidiBus::queue_system) reads a flat [`ArcSwap`]
/// snapshot of the current senders — a single atomic load on the RT
/// side, no DashMap shard locks. The snapshot is rebuilt off-RT in
/// [`insert`](MidiBus::insert) / [`remove`](MidiBus::remove).
/// [`queue`](MidiBus::queue) and [`queue_system_to`](MidiBus::queue_system_to)
/// still read the DashMap (O(1) hash lookup, brief shard read-lock) —
/// keep addressed delivery on the fast path and avoid `insert`/`remove`
/// at playback time.
#[derive(Clone, Default)]
pub struct MidiBus {
    senders: Arc<DashMap<MidiUnitId, MidiSender>>,
    broadcast: Arc<ArcSwap<Box<[MidiSender]>>>,
    /// Optional MPE processor: every event passing through `queue`
    /// (and addressed `queue_system_to`) is also fed to this
    /// processor before delivery, populating the per-note
    /// expression atomics that voices read on the audio thread.
    /// `None` = MPE disabled (the default).
    ///
    /// The `Mutex` is taken with `try_lock` on the audio-thread feed path
    /// (see [`feed_mpe`](MidiBus::feed_mpe)) so the RT thread never blocks on
    /// it; the only other locker is the off-RT `process`-mutation itself.
    #[cfg(feature = "mpe")]
    mpe: Arc<ArcSwap<Option<Arc<Mutex<MpeProcessor>>>>>,
    /// The installed processor's `PerNoteExpression`, published separately so
    /// [`mpe_expression`](MidiBus::mpe_expression) reads it with a single
    /// atomic load — without locking the processor (which the audio thread
    /// may be holding). `None` = MPE disabled.
    #[cfg(feature = "mpe")]
    mpe_expression: Arc<ArcSwap<Option<Arc<PerNoteExpression>>>>,
}

impl MidiBus {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install an MPE processor. Every event going through `queue` /
    /// `queue_system_to` will be fed to the processor before being
    /// delivered to subscribers. `queue_system` (true broadcast — clock,
    /// start/stop) bypasses the processor.
    ///
    /// Replaces any previously-installed processor. Returns the
    /// processor's `Arc<PerNoteExpression>` so callers (typically
    /// bevy-tutti's `mpe_setup_system`) can hand it to readers without
    /// touching the bus again.
    #[cfg(feature = "mpe")]
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
    #[cfg(feature = "mpe")]
    pub fn mpe_expression(&self) -> Option<Arc<PerNoteExpression>> {
        self.mpe_expression.load().as_ref().as_ref().map(Arc::clone)
    }

    /// Uninstall the MPE processor. Subsequent events bypass the feed.
    #[cfg(feature = "mpe")]
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
    /// No-op when the `mpe` feature is off.
    #[inline]
    #[cfg(feature = "mpe")]
    fn feed_mpe(&self, event: &MidiEvent) {
        if let Some(processor) = self.mpe.load().as_ref().as_ref() {
            if let Some(mut guard) = processor.try_lock() {
                guard.process(event);
            }
        }
    }
    #[inline]
    #[cfg(not(feature = "mpe"))]
    fn feed_mpe(&self, _event: &MidiEvent) {}

    /// Attach (or replace) the sender for a unit id.
    pub fn insert(&self, sender: MidiSender) {
        self.senders.insert(sender.unit_id(), sender);
        self.rebuild_broadcast();
    }

    /// Drop the sender for a unit id. Subsequent queues for it are no-ops.
    pub fn remove(&self, unit_id: MidiUnitId) {
        self.senders.remove(&unit_id);
        self.rebuild_broadcast();
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

    /// Broadcast a system event to every subscribed unit. RT-safe: reads a
    /// flat snapshot via a single atomic load. **Bypasses MPE** — system
    /// events (clock, start/stop, song-position) aren't channel/voice
    /// data and don't carry per-note expression.
    pub fn queue_system(&self, event: &MidiEvent) {
        for sender in self.broadcast.load().iter() {
            sender.queue_system(event);
        }
    }

    /// Deliver a system event to a single subscribed unit.
    pub fn queue_system_to(&self, unit_id: MidiUnitId, event: &MidiEvent) {
        if let Some(sender) = self.senders.get(&unit_id) {
            sender.queue_system(event);
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

    /// Rebuild the broadcast snapshot from the current DashMap contents.
    /// Called off-RT after every insert/remove. The RT side sees one
    /// atomic swap of the snapshot.
    fn rebuild_broadcast(&self) {
        let snapshot: Box<[MidiSender]> = self
            .senders
            .iter()
            .map(|entry| entry.value().clone())
            .collect();
        self.broadcast.store(Arc::new(snapshot));
    }
}

impl tutti_midi_types::MidiQueue for MidiBus {
    fn queue(&self, unit_id: MidiUnitId, events: &[MidiEvent]) {
        self.queue(unit_id, events);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_midi_types::midi2::{system_common, UmpMessage};

    fn note_on(note: u8, vel_u7: u8) -> MidiEvent {
        MidiEvent::note_on(0, 0, note, (vel_u7 as u16) << 9)
    }

    fn note_off(note: u8) -> MidiEvent {
        MidiEvent::note_off(0, 0, note, 0)
    }

    #[test]
    fn sender_pushes_to_receiver() {
        let id = MidiUnitId::new(12345);
        let (sender, receiver) = MidiEventSlot::pair(id);

        sender.queue(&[note_on(60, 100), note_off(60)]);
        assert!(receiver.has_events());

        let mut buf = [note_on(0, 0); 16];
        let n = receiver.poll_into(&mut buf);
        assert_eq!(n, 2);
        assert!(!receiver.has_events());
    }

    #[test]
    fn cloned_sender_pushes_to_same_receiver() {
        let (sender, receiver) = MidiEventSlot::pair(MidiUnitId::next());
        let s2 = sender.clone();

        sender.queue(&[note_on(60, 100)]);
        s2.queue(&[note_on(64, 100)]);

        let mut buf = [note_on(0, 0); 16];
        assert_eq!(receiver.poll_into(&mut buf), 2);
    }

    #[test]
    fn sender_note_helpers_match_explicit_events() {
        let (sender, receiver) = MidiEventSlot::pair(MidiUnitId::next());
        sender.note_on(0, 60, 100);
        sender.note_off(0, 60);

        let mut buf = [note_on(0, 0); 4];
        assert_eq!(receiver.poll_into(&mut buf), 2);
    }

    #[test]
    fn dropped_receiver_keeps_sender_pushable() {
        // Sender holds an Arc to the slot; dropping the receiver doesn't
        // invalidate pushes. Events accumulate in the slot until the sender
        // is also dropped.
        let (sender, receiver) = MidiEventSlot::pair(MidiUnitId::next());
        drop(receiver);
        sender.queue(&[note_on(60, 100)]);
        // No assertion possible without the receiver — but no crash is the test.
    }

    #[test]
    fn back_pressure_drops_when_full() {
        let (sender, receiver) = MidiEventSlot::pair(MidiUnitId::next());
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
        let (sender, receiver) = MidiEventSlot::pair(id);
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
        let (s1, r1) = MidiEventSlot::pair(id1);
        let (s2, r2) = MidiEventSlot::pair(id2);
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
        let (sender, receiver) = MidiEventSlot::pair(id);
        bus.insert(sender);
        bus.remove(id);

        bus.queue(id, &[note_on(60, 100)]);
        let mut buf = [note_on(0, 0); 4];
        assert_eq!(receiver.poll_into(&mut buf), 0);
    }

    #[test]
    fn bus_broadcasts_system_events() {
        let bus = MidiBus::new();
        let id1 = MidiUnitId::new(1);
        let id2 = MidiUnitId::new(2);
        let (s1, r1) = MidiEventSlot::pair(id1);
        let (s2, r2) = MidiEventSlot::pair(id2);
        bus.insert(s1);
        bus.insert(s2);

        let clock = MidiEvent::timing_clock(0);
        bus.queue_system(&clock);

        assert!(r1.has_system_events());
        assert!(r2.has_system_events());

        let mut buf = [MidiEvent::noop(); 4];
        assert_eq!(r1.poll_system(&mut buf), 1);
        assert!(matches!(
            UmpMessage::try_from(buf[0].data_words()),
            Ok(UmpMessage::SystemCommon(
                system_common::SystemCommon::TimingClock(_)
            ))
        ));
        assert_eq!(r2.poll_system(&mut buf), 1);
    }

    #[test]
    fn bus_queue_system_to_targets_one_unit() {
        let bus = MidiBus::new();
        let id1 = MidiUnitId::new(1);
        let id2 = MidiUnitId::new(2);
        let (s1, r1) = MidiEventSlot::pair(id1);
        let (s2, r2) = MidiEventSlot::pair(id2);
        bus.insert(s1);
        bus.insert(s2);

        let mut sysex = Vec::new();
        MidiEvent::sysex7_fragments(0, &[0x7E, 0x7F, 0x09, 0x01], &mut sysex);
        bus.queue_system_to(id1, &sysex[0]);

        assert!(r1.has_system_events());
        assert!(!r2.has_system_events());
    }

    #[test]
    #[cfg(feature = "mpe")]
    fn bus_mpe_feed_updates_per_note_expression() {
        // Install MPE on the bus, queue a note-on + pitch-bend, verify
        // the per-note expression atomic reflects the bend.
        use crate::mpe::{MpeMode, MpeProcessor, MpeZoneConfig};
        use tutti_midi_types::convert::midi1_pitch_bend_to_midi2;

        let bus = MidiBus::new();
        let processor = MpeProcessor::new(MpeMode::LowerZone(MpeZoneConfig::lower(15)));
        let expression = bus.install_mpe(processor);

        // Subscribe a unit so the bus has somewhere to deliver events.
        let id = MidiUnitId::new(1);
        let (sender, _receiver) = MidiEventSlot::pair(id);
        bus.insert(sender);

        // Note on, channel 2 (a member channel of the lower zone).
        let note_on = MidiEvent::note_on(0, 2, 60, 100u16 << 9);
        // Channel pitch-bend on channel 2 — under MPE classic mapping
        // this routes to note 60.
        let bend = MidiEvent::pitch_bend(0, 2, midi1_pitch_bend_to_midi2(16383));

        bus.queue(id, &[note_on, bend]);

        let actual = expression.get_pitch_bend(60);
        assert!(
            (actual - 1.0).abs() < 0.01,
            "expected pitch_bend ≈ 1.0 after bend, got {actual}"
        );
        assert!(expression.is_active(60), "note 60 should be active");
    }

    #[test]
    #[cfg(feature = "mpe")]
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
        let read_back = bus.mpe_expression().expect("expression published on install");
        assert!(
            Arc::ptr_eq(&installed, &read_back),
            "mpe_expression() must hand back the same handle install_mpe returned"
        );

        bus.uninstall_mpe();
        assert!(bus.mpe_expression().is_none(), "uninstall clears the expression");
    }

    #[test]
    #[cfg(feature = "mpe")]
    fn bus_mpe_uninstall_stops_feed() {
        use crate::mpe::{MpeMode, MpeProcessor, MpeZoneConfig};

        let bus = MidiBus::new();
        let processor = MpeProcessor::new(MpeMode::LowerZone(MpeZoneConfig::lower(15)));
        let expression = bus.install_mpe(processor);
        bus.uninstall_mpe();

        let id = MidiUnitId::new(1);
        let (sender, _receiver) = MidiEventSlot::pair(id);
        bus.insert(sender);

        // Without MPE installed, queueing a note-on shouldn't update
        // the expression atomics.
        let note_on = MidiEvent::note_on(0, 2, 60, 100u16 << 9);
        bus.queue(id, &[note_on]);

        assert!(!expression.is_active(60), "note should not register after uninstall");
    }

    #[test]
    #[cfg(feature = "mpe")]
    fn bus_mpe_disabled_by_default_no_overhead() {
        // No processor installed → `feed_mpe` is a single ArcSwap load
        // and a None-check. Confirm the bus still routes events.
        let bus = MidiBus::new();
        assert!(bus.mpe_expression().is_none());

        let id = MidiUnitId::new(1);
        let (sender, receiver) = MidiEventSlot::pair(id);
        bus.insert(sender);

        let note_on = MidiEvent::note_on(0, 0, 60, 100u16 << 9);
        bus.queue(id, &[note_on]);

        assert!(receiver.has_events());
    }
}
