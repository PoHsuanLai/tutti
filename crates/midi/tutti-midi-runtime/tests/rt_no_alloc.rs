//! Regression gate for RT-safety: MIDI runtime hot paths must not
//! allocate. Covers `MidiBus::queue` (addressed per-unit delivery) and the
//! outbound `MidiSender::queue` push that the hardware-out mailbox and the RT
//! clip tap both use.

use assert_no_alloc::AllocDisabler;
use tutti_midi_runtime::{MidiBus, MidiMailbox};
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::MidiUnitId;

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
        let (sender, receiver) = MidiMailbox::pair(id);
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

/// The outbound mailbox push — the path the clock master, the RT clip tap, and
/// the protocol producers (MIDI-CI, endpoint discovery, Flex metadata) all take
/// to reach hardware out. The clock master runs it once per audio block, so it
/// must not allocate.
#[test]
fn outbound_mailbox_push_is_allocation_free() {
    let (sender, receiver) = MidiMailbox::pair(MidiUnitId::new(1));
    let clock = MidiEvent::timing_clock(0);

    // Warm up outside the no-alloc scope.
    sender.queue(&[clock]);
    let mut drain = [note_on(0, 0); 8];
    let _ = receiver.poll_into(&mut drain);

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..10_000 {
            sender.queue(&[clock]);
            // Drain so the bounded ring never fills and starts rejecting —
            // this exercises both halves of the mailbox.
            let _ = receiver.poll_into(&mut drain);
        }
    });
}

/// The per-note expression store (`AtomicPerNoteMap`) is touched on the audio
/// thread on every note-on/off and every expression update. Claiming, updating,
/// reading, and releasing a slot must all be allocation-free.
#[test]
fn per_note_expression_hot_path_is_allocation_free() {
    use tutti_midi_runtime::PerNoteExpression;
    use tutti_midi_types::NoteId;

    let expr = PerNoteExpression::new();
    // Warm up: claim + release once outside the no-alloc scope so any lazy
    // init (there is none, but be safe) happens first.
    let warm = NoteId::from_channel_note(0, 60);
    expr.note_on(warm);
    expr.note_off(warm);

    assert_no_alloc::assert_no_alloc(|| {
        for round in 0..10_000u32 {
            // Spread across channels + notes so many distinct ids are claimed
            // and released, exercising the probe/claim/free paths.
            let ch = (round % 15) as u8 + 1;
            let note = (round % 60) as u8 + 24;
            let id = NoteId::from_channel_note(ch, note);
            expr.note_on(id);
            expr.set_pitch_bend(id, 0.5);
            expr.set_pressure(id, 0.25);
            expr.set_slide(id, 0.75);
            let _ = expr.get_pitch_bend(id);
            let _ = expr.get_slide(id);
            expr.note_off(id);
        }
    });
}
