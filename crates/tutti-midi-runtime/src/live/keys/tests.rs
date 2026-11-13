use super::*;
use tutti_fixtures::midi::{self, wide_velocity};
use tutti_graph::EventInletNode;
use tutti_graph::EventKind;
use tutti_graph::contract::Direct;
use tutti_types::{Frame, SampleRate};
use tutti_ump::{MidiGroup, NoteVelocity, Ump};

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
        NoteVelocity::new(wide_velocity(100)),
    )
    .unwrap();
    keys.note_off(Stamp::NextBlock, MidiChannel::FIRST, NoteNumber::of(60))
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
    keys.note_on(
        Stamp::NextBlock,
        MidiChannel::FIRST,
        NoteNumber::of(60),
        NoteVelocity::MIN,
    )
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
            midi::note_on(group, MidiChannel::FIRST, 60, 5),
            midi::note_off(group, MidiChannel::FIRST, 60, 0),
        ]
    );
}

/// A MIDI 1.0 velocity is widened by M2-115 (7 to 16 bits: 100 is `51492`,
/// 64 is `0x8000`, 127 is `0xFFFF`), not sent as the raw 16-bit field.
#[test]
fn a_midi1_velocity_is_widened_not_copied() {
    let (mut d, mut keys) = rig();
    for v in [100u8, 64, 127] {
        keys.note_on_midi1(
            Stamp::NextBlock,
            MidiChannel::FIRST,
            NoteNumber::of(60),
            tutti_ump::U7::of(v),
        )
        .unwrap();
    }
    d.block();
    let velocities: Vec<u16> = sent(&d)
        .iter()
        .map(|u| match u.decode() {
            Ok(Message::Midi2(_, Voice2::NoteOn { velocity, .. })) => velocity.get(),
            other => panic!("a MIDI 2.0 Note On, got {other:?}"),
        })
        .collect();
    assert_eq!(velocities, [51492, 0x8000, 0xFFFF]);
}
