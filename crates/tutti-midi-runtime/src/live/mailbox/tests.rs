use super::*;
use tutti_midi_types::convert::midi1_velocity_to_midi2;

use tutti_fixtures::midi;

/// A note-on with a 7-bit velocity, widened (M2-115), at the block's first
/// frame: what `MidiSender::note_on` pushes.
fn note_on(note: u8, vel_u7: u8) -> (Offset, Ump) {
    (
        Offset::ZERO,
        midi::note_on(
            MidiGroup::FIRST,
            MidiChannel::FIRST,
            note,
            midi1_velocity_to_midi2(vel_u7),
        ),
    )
}

/// An event as the ring holds it: its offset as the raw stamp.
fn raw(e: (Offset, Ump)) -> (u32, Ump) {
    (e.0.get(), e.1)
}

fn note_off(note: u8) -> (Offset, Ump) {
    (
        Offset::ZERO,
        midi::note_off(MidiGroup::FIRST, MidiChannel::FIRST, note, 0),
    )
}

#[test]
fn sender_pushes_to_receiver() {
    let (sender, receiver) = MidiMailbox::pair();
    sender.queue(&[note_on(60, 100), note_off(60)]);
    assert!(receiver.has_events());
    let mut buf = [(0, Ump([0; 4])); 16];
    assert_eq!(receiver.poll_into(&mut buf), 2);
    assert_eq!(buf[..2], [raw(note_on(60, 100)), raw(note_off(60))]);
    assert!(!receiver.has_events());
}

#[test]
fn cloned_sender_pushes_to_same_receiver() {
    let (sender, receiver) = MidiMailbox::pair();
    let s2 = sender.clone();
    sender.queue(&[note_on(60, 100)]);
    s2.queue(&[note_on(64, 100)]);
    let mut buf = [(0, Ump([0; 4])); 16];
    assert_eq!(receiver.poll_into(&mut buf), 2);
}

/// The `note_on` / `note_off` conveniences build the same events the
/// explicit constructors do.
///
/// Mutation: a helper emitting another channel or velocity scaling → fails.
#[test]
fn sender_note_helpers_match_explicit_events() {
    let (sender, receiver) = MidiMailbox::pair();
    sender.note_on(MidiChannel::FIRST, 60, 100);
    sender.note_off(MidiChannel::FIRST, 60);
    let mut buf = [(0, Ump([0; 4])); 4];
    assert_eq!(receiver.poll_into(&mut buf), 2);
    assert_eq!(buf[0], raw(note_on(60, 100)));
    assert_eq!(buf[1], raw(note_off(60)));
}

/// A push into a ring whose receiver is gone still lands (the count says
/// so), rather than silently reporting zero.
#[test]
fn dropped_receiver_keeps_sender_pushable() {
    let (sender, receiver) = MidiMailbox::pair();
    drop(receiver);
    assert_eq!(sender.queue(&[note_on(60, 100), note_on(64, 100)]), 2);
    assert!(sender.note_on(MidiChannel::FIRST, 67, 100));
}

/// A partial accept reports the prefix that landed, not the count offered.
///
/// Mutation: `queue` returning `events.len()` → fails.
#[test]
fn a_partial_accept_reports_how_many_landed() {
    let (sender, _receiver) = MidiMailbox::with_capacity(5);
    assert_eq!(sender.queue(&[note_on(1, 1), note_on(2, 1)]), 2);
    let batch = [
        note_on(60, 100),
        note_on(61, 100),
        note_on(62, 100),
        note_on(63, 100),
    ];
    assert_eq!(sender.queue(&batch), 3);
}
