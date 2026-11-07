//! `Ump::key_event` reads the same facts `decode` does, and the closing
//! packets it builds decode back to what they close.
//!
//! The expectation comes from `decode`, so the raw bit reads in `key_event`
//! cannot drift from the codec.
//!
//! Mutation record (each planted, seen to fail, restored):
//! - read a MIDI 1.0 Note On at velocity 0 as an on -> `key_event_agrees_with_decode`;
//! - read a MIDI 2.0 Note On at velocity 0 as an off -> `key_event_agrees_with_decode`;
//! - build `note_off` with status 9 -> `a_close_decodes_to_the_off_of_its_key`;
//! - drop the protocol bit from `Key::new` -> `a_key_carries_its_fields` and
//!   `a_close_decodes_to_the_off_of_its_key`.

mod common;

use proptest::prelude::*;
use tutti_ump::{Address, Key, KeyEvent, Message, Ump, Voice1, Voice2, decode};

const SUSTAIN: u8 = 64;

fn lane(a: Address) -> u8 {
    a.group.get() * 16 + a.channel.get()
}

fn expected(message: Message) -> Option<KeyEvent> {
    match message {
        Message::Midi1(a, v) => {
            let key = |note: u8| Key::new(false, lane(a), note);
            match v {
                Voice1::NoteOn { note, velocity } if velocity.get() == 0 => {
                    Some(KeyEvent::Off(key(note.get())))
                }
                Voice1::NoteOn { note, .. } => Some(KeyEvent::On(key(note.get()))),
                Voice1::NoteOff { note, .. } => Some(KeyEvent::Off(key(note.get()))),
                Voice1::ControlChange { controller, value } if controller.get() == SUSTAIN => {
                    Some(KeyEvent::Sustain {
                        m2: false,
                        lane: lane(a),
                        down: value.get() >= 64,
                    })
                }
                _ => None,
            }
        }
        Message::Midi2(a, v) => {
            let key = |note: u8| Key::new(true, lane(a), note);
            match v {
                Voice2::NoteOn { note, .. } => Some(KeyEvent::On(key(note.get()))),
                Voice2::NoteOff { note, .. } => Some(KeyEvent::Off(key(note.get()))),
                Voice2::ControlChange { controller, value } if controller.get() == SUSTAIN => {
                    Some(KeyEvent::Sustain {
                        m2: true,
                        lane: lane(a),
                        down: value >= 0x8000_0000,
                    })
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn words() -> impl Strategy<Value = [u32; 4]> {
    prop_oneof![
        any::<[u32; 4]>(),
        (any::<[u32; 4]>(), any::<u8>()).prop_map(|(w, sel)| common::shape(w, sel)),
        // Channel voice words, so the note and pedal arms are reached often.
        (
            prop_oneof![Just(0x2u32), Just(0x4)],
            0u32..16,
            prop_oneof![Just(0x8u32), Just(0x9), Just(0xb)],
            0u32..16,
            any::<u32>(),
            prop_oneof![Just(0u32), 0u32..128],
            any::<u32>()
        )
            .prop_map(|(t, g, s, c, low, vel, w1)| {
                let note = if s == 0xb && low % 2 == 0 {
                    64
                } else {
                    low & 0x7f
                };
                // The M2 velocity is in the second word; M1 carries it in the first.
                let w1 = if t == 0x4 {
                    (vel << 16) | (w1 & 0xffff)
                } else {
                    w1
                };
                let vel0 = if t == 0x2 { vel } else { 0 };
                [
                    (t << 28) | (g << 24) | (s << 20) | (c << 16) | (note << 8) | vel0,
                    w1,
                    0,
                    0,
                ]
            }),
    ]
}

proptest! {
    #[test]
    fn key_event_agrees_with_decode(w in words()) {
        let ump = Ump(w);
        let want = decode(ump).ok().and_then(expected);
        prop_assert_eq!(ump.key_event(), want);
    }

    /// The close of every key is an off of that key, in its own protocol, and
    /// the pedal-up is a pedal event, up.
    #[test]
    fn a_close_decodes_to_the_off_of_its_key(index in any::<u16>(), m2 in any::<bool>(), lane_ in any::<u8>()) {
        let key = Key::from_index(index);
        let off = Ump::note_off(key);
        prop_assert_eq!(off.key_event(), Some(KeyEvent::Off(key)));
        prop_assert!(off.is_note_off());
        prop_assert!(decode(off).is_ok());
        let up = Ump::sustain_up(m2, lane_);
        prop_assert_eq!(up.key_event(), Some(KeyEvent::Sustain { m2, lane: lane_, down: false }));
        prop_assert!(decode(up).is_ok());
    }
}

#[test]
fn a_key_carries_its_fields() {
    let key = Key::new(true, 0x1f, 0x45);
    assert_eq!((key.is_m2(), key.lane(), key.note()), (true, 0x1f, 0x45));
    assert_eq!(Key::from_index(key.index()), key);
}
