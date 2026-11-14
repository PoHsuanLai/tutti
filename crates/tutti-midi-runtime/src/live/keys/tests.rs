use super::*;
use tutti_fixtures::midi;
use tutti_graph::EventInletNode;
use tutti_graph::EventKind;
use tutti_graph::contract::Direct;
use tutti_midi_protocol::widen_velocity;
use tutti_types::{Frame, SampleRate};
use tutti_ump::{MidiGroup, NoteVelocity, U7, Ump};

fn rig() -> (Direct<EventInletNode>, MidiKeys) {
    let (node, writer) = EventInletNode::pair(LIVE_INLET_CAPACITY);
    (
        Direct::new(node, SampleRate::SR_48K, 64),
        MidiKeys::new(writer, MidiGroup::FIRST),
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
        Stamp::NextBlock,
        MidiChannel::FIRST,
        NoteNumber::of(60),
        NoteVelocity::new(widen_velocity(U7::of(100)).get()),
    )
    .unwrap();
    keys.note_off(Stamp::NextBlock, MidiChannel::FIRST, NoteNumber::of(60))
        .unwrap();
    d.block();
    assert_eq!(
        sent(&d),
        [
            midi::note_on(MidiChannel::FIRST, 60, widen_velocity(U7::of(100)).get()),
            midi::note_off(MidiChannel::FIRST, 60, 0),
        ]
    );
}

/// A Note On at velocity 0 is a MIDI 2.0 Note On (M2-104 §7.4.2), not a Note
/// Off.
#[test]
fn a_note_on_at_velocity_zero_stays_a_note_on() {
    let (mut d, mut keys) = rig();
    keys.note_on(
        Stamp::NextBlock,
        MidiChannel::FIRST,
        NoteNumber::of(60),
        NoteVelocity::MIN,
    )
    .unwrap();
    d.block();
    assert_eq!(sent(&d), [midi::note_on(MidiChannel::FIRST, 60, 0)]);
}

/// A full ring refuses a helper's event with `Full` (the writer's own rule,
/// reached through the helper), so a note stuck for want of room is reported.
#[test]
fn a_full_ring_refuses_through_the_helper() {
    let (_d, mut keys) = rig();
    for _ in 0..LIVE_INLET_CAPACITY.slots() {
        keys.note_on(
            Stamp::NextBlock,
            MidiChannel::FIRST,
            NoteNumber::of(60),
            NoteVelocity::new(1),
        )
        .unwrap();
    }
    assert_eq!(
        keys.note_off(Stamp::NextBlock, MidiChannel::FIRST, NoteNumber::of(60)),
        Err(Refused::Full)
    );
    assert_eq!(keys.counts().losses().lost, 1);
}

/// The helpers send on the Group the keys were made for, not always the first.
#[test]
fn the_helpers_send_on_the_keys_group() {
    let group = MidiGroup::new(2).expect("in range");
    let (node, writer) = EventInletNode::pair(LIVE_INLET_CAPACITY);
    let mut d = Direct::new(node, SampleRate::SR_48K, 64);
    let mut keys = MidiKeys::new(writer, group);
    assert_eq!(keys.group(), group);
    keys.note_on(
        Stamp::NextBlock,
        MidiChannel::FIRST,
        NoteNumber::of(60),
        NoteVelocity::new(5),
    )
    .unwrap();
    keys.note_off(Frame::new(0), MidiChannel::FIRST, NoteNumber::of(60))
        .unwrap();
    d.block();
    assert_eq!(
        sent(&d),
        [
            midi::note_on(tutti_ump::Address::new(group, MidiChannel::FIRST), 60, 5),
            midi::note_off(tutti_ump::Address::new(group, MidiChannel::FIRST), 60, 0),
        ]
    );
}
