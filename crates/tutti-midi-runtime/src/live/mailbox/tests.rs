use super::*;

fn note_on(note: u8, vel_u7: u8) -> MidiEvent {
    MidiEvent::note_on_7bit(MidiGroup::FIRST, MidiChannel::FIRST, note, vel_u7)
}

fn note_off(note: u8) -> MidiEvent {
    MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, note, 0)
}

#[test]
fn sender_pushes_to_receiver() {
    let (sender, receiver) = MidiMailbox::pair();
    sender.queue(&[note_on(60, 100), note_off(60)]);
    assert!(receiver.has_events());
    let mut buf = [MidiEvent::noop(); 16];
    assert_eq!(receiver.poll_into(&mut buf), 2);
    assert_eq!(buf[..2], [note_on(60, 100), note_off(60)]);
    assert!(!receiver.has_events());
}

#[test]
fn cloned_sender_pushes_to_same_receiver() {
    let (sender, receiver) = MidiMailbox::pair();
    let s2 = sender.clone();
    sender.queue(&[note_on(60, 100)]);
    s2.queue(&[note_on(64, 100)]);
    let mut buf = [MidiEvent::noop(); 16];
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
    let mut buf = [MidiEvent::noop(); 4];
    assert_eq!(receiver.poll_into(&mut buf), 2);
    assert_eq!(buf[0], note_on(60, 100));
    assert_eq!(buf[1], note_off(60));
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
