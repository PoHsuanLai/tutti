use super::*;

use tutti_fixtures::midi;
use tutti_ump::{
    Address, BendValue, CCNumber, Message, MidiChannel, MidiGroup, NoteNumber, NoteVelocity, U7,
    Voice1, Voice2,
};

const G0: MidiGroup = MidiGroup::FIRST;
const G1: MidiGroup = MidiGroup::of(1);

fn drain(held: &mut HeldState, group: Option<MidiGroup>) -> Vec<Ump> {
    let mut out = Vec::new();
    assert!(held.release(group, |u| {
        out.push(u);
        true
    }));
    out
}

fn on(group: MidiGroup, channel: u8, note: u8) -> Ump {
    midi::note_on(Address::new(group, MidiChannel::of(channel)), note, 0xFFFF)
}

/// **A release ends exactly what was started**: two keys down and one up,
/// the release is one Note Off for the key still down, in the key's own
/// protocol; and a second release has nothing to send.
#[test]
fn a_release_ends_the_keys_still_down_and_only_those() {
    let mut held = HeldState::new();
    held.observe(on(G0, 0, 60));
    held.observe(on(G0, 0, 64));
    held.observe(Message::midi2(
        Address::new(G0, MidiChannel::of(0)),
        Voice2::note_off(NoteNumber::of(60), NoteVelocity::MIN),
    ));
    let sent = drain(&mut held, None);
    assert_eq!(
        sent,
        vec![midi::note_off(Address::new(G0, MidiChannel::of(0)), 64, 0)]
    );
    assert!(held.is_empty());
    assert!(drain(&mut held, None).is_empty());
}

/// **A MIDI 1.0 Note On at velocity 0 is an off** (MIDI 1.0 Detailed
/// Specification, Note On): the key it names is not held; the same bytes in
/// MIDI 2.0 are a note on (M2-104 section 7.4.2).
#[test]
fn a_midi1_note_on_at_velocity_zero_ends_the_key() {
    let mut held = HeldState::new();
    let m1_on = |v: u8| midi::m1_note_on(MidiChannel::of(2), 60, U7::of(v));
    held.observe(m1_on(100));
    assert!(!held.is_empty());
    held.observe(m1_on(0));
    assert!(held.is_empty());
}

/// **Notes first, then pedals and bend** (MIDI 1.0 Detailed Specification
/// A-5: a pedal that is down defeats the offs): one key, the sustain pedal
/// down and the bend off centre give the Note Off, then the pedal-up, then the
/// centred bend; a pedal back up and a bend back at centre are not sent.
#[test]
fn a_release_ends_notes_before_it_lifts_pedals_and_centres_the_bend() {
    let mut held = HeldState::new();
    let ch = MidiChannel::of(3);
    held.observe(on(G0, 3, 60));
    held.observe(Message::midi1(
        ch,
        Voice1::control_change(CCNumber::SUSTAIN, U7::of(127)),
    ));
    held.observe(Message::midi1(
        ch,
        Voice1::control_change(CCNumber::SOSTENUTO, U7::of(127)),
    ));
    held.observe(Message::midi1(
        ch,
        Voice1::control_change(CCNumber::SOSTENUTO, U7::of(0)),
    ));
    held.observe(Message::midi2(
        Address::new(G0, ch),
        Voice2::PitchBend(BendValue::new(0xC000_0000)),
    ));
    let sent = drain(&mut held, None);
    assert_eq!(sent.len(), 3, "{sent:?}");
    assert_eq!(sent[0], midi::note_off(Address::new(G0, ch), 60, 0));
    assert_eq!(sent[1], midi::m1_cc(ch, CCNumber::SUSTAIN, U7::of(0)));
    assert_eq!(sent[2], midi::pitch_bend(Address::new(G0, ch), 0x8000_0000));
}

/// **A release for one Group leaves the other's alone** (a Group is a cable,
/// M2-104 section 2.1.2): notes on Groups 1 and 2, release Group 2, and
/// Group 1's note is still held.
/// Spec: m2-104/2.1.2#2
#[test]
fn a_release_for_one_group_leaves_the_others_held() {
    let mut held = HeldState::new();
    held.observe(on(G0, 0, 60));
    held.observe(on(G1, 0, 61));
    let sent = drain(&mut held, Some(G1));
    assert_eq!(
        sent,
        vec![midi::note_off(Address::new(G1, MidiChannel::of(0)), 61, 0)]
    );
    assert!(!held.is_empty());
    let rest = drain(&mut held, Some(G0));
    assert_eq!(
        rest,
        vec![midi::note_off(Address::new(G0, MidiChannel::of(0)), 60, 0)]
    );
}

/// **A receiver with no room is asked again for what it refused**: refusing
/// the second of three keys leaves it and the third held, and the next
/// release sends exactly those.
#[test]
fn a_refused_release_stays_held_for_the_next() {
    let mut held = HeldState::new();
    for n in [60, 61, 62] {
        held.observe(on(G0, 0, n));
    }
    let mut taken = 0;
    let done = held.release(None, |_| {
        taken += 1;
        taken < 2
    });
    assert!(!done);
    assert_eq!(taken, 2);
    let rest = drain(&mut held, None);
    assert_eq!(
        rest,
        vec![
            midi::note_off(Address::new(G0, MidiChannel::of(0)), 61, 0),
            midi::note_off(Address::new(G0, MidiChannel::of(0)), 62, 0)
        ]
    );
}
