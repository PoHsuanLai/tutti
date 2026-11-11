use super::*;
use tutti_fixtures::midi::{self, wide_velocity};
use tutti_graph::EventInletNode;
use tutti_graph::EventKind;
use tutti_graph::contract::Direct;
use tutti_types::SampleRate;
use tutti_ump::MidiGroup;

fn rig() -> (Direct<EventInletNode>, MidiKeys) {
    let (node, writer) = EventInletNode::pair(LIVE_INLET_CAPACITY);
    (
        Direct::new(node, SampleRate(48_000.0), 64),
        MidiKeys::new(writer),
    )
}

fn sent(d: &Direct<EventInletNode>) -> Vec<Ump> {
    d.events_out(0)
        .iter()
        .map(|e| match e.kind {
            EventKind::Midi(ump) => ump,
            _ => unreachable!(),
        })
        .collect()
}

/// The `note_on` / `note_off` helpers build the same packets the explicit
/// constructors do, and they come out of the inlet's port on the next block.
#[test]
fn the_key_helpers_match_explicit_events() {
    let (mut d, mut keys) = rig();
    keys.note_on(
        At::NextBlock,
        MidiChannel::FIRST,
        NoteNumber::of(60),
        wide_velocity(100),
    )
    .unwrap();
    keys.note_off(At::NextBlock, MidiChannel::FIRST, NoteNumber::of(60))
        .unwrap();
    d.block();
    assert_eq!(
        sent(&d),
        [
            midi::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, wide_velocity(100)),
            midi::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0),
        ]
    );
}

/// A Note On at velocity 0 is a MIDI 2.0 Note On (M2-104 §7.4.2), not a Note
/// Off.
#[test]
fn a_note_on_at_velocity_zero_stays_a_note_on() {
    let (mut d, mut keys) = rig();
    keys.note_on(At::NextBlock, MidiChannel::FIRST, NoteNumber::of(60), 0)
        .unwrap();
    d.block();
    assert_eq!(
        sent(&d),
        [midi::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0)]
    );
}

/// A full ring refuses a helper's event with `Full` (the writer's own rule,
/// reached through the helper), so a note stuck for want of room is reported.
#[test]
fn a_full_ring_refuses_through_the_helper() {
    let (_d, mut keys) = rig();
    for _ in 0..LIVE_INLET_CAPACITY {
        keys.note_on(At::NextBlock, MidiChannel::FIRST, NoteNumber::of(60), 1)
            .unwrap();
    }
    assert_eq!(
        keys.note_off(At::NextBlock, MidiChannel::FIRST, NoteNumber::of(60)),
        Err(Refused::Full)
    );
    assert_eq!(keys.lost(), 1);
}
