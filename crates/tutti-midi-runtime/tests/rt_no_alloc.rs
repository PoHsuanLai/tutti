//! Regression gate for RT-safety: MIDI runtime hot paths must not
//! allocate. Covers `MidiBus::queue` (per-unit) and
//! `MidiBus::queue_system` (broadcast via the ArcSwap snapshot added in
//! the midi-runtime step of the RT-safety audit).

use assert_no_alloc::AllocDisabler;
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::MidiUnitId;
use tutti_midi_runtime::{MidiBus, MidiEventSlot};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

fn note_on(ch: u8, note: u8) -> MidiEvent {
    MidiEvent::note_on(0, ch, note, 0x8000)
}

#[test]
fn midi_bus_queue_is_allocation_free() {
    let bus = MidiBus::new();
    // Register a handful of units — registration happens off-RT and is
    // allowed to allocate.
    let mut ids = Vec::with_capacity(8);
    let mut receivers = Vec::with_capacity(8);
    for i in 0..8u64 {
        let id = MidiUnitId::new(i);
        let (sender, receiver) = MidiEventSlot::pair(id);
        bus.insert(sender);
        ids.push(id);
        receivers.push(receiver);
    }

    // Warm up the first routed event outside the no-alloc scope.
    let event = note_on(0, 60);
    bus.queue(ids[0], &[event]);
    let mut drain = [note_on(0, 0); 8];
    let _ = receivers[0].poll_into(&mut drain);

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..10_000 {
            for id in &ids {
                bus.queue(*id, &[event]);
            }
        }
    });
}

#[test]
fn midi_bus_queue_system_broadcast_is_allocation_free() {
    let bus = MidiBus::new();
    let mut receivers = Vec::with_capacity(16);
    for i in 0..16u64 {
        let id = MidiUnitId::new(i);
        let (sender, receiver) = MidiEventSlot::pair(id);
        bus.insert(sender);
        receivers.push(receiver);
    }

    let clock = MidiEvent::timing_clock(0);

    // Warm up: confirm the snapshot is reachable before the no-alloc scope.
    bus.queue_system(&clock);

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..10_000 {
            bus.queue_system(&clock);
        }
    });
}
