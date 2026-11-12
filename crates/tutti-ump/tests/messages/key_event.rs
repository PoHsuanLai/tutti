//! `Ump::key_event` reads the same facts `decode` does, and the closing
//! packets it builds decode back to what they close.
//!
//! The expectation comes from `decode`, so the raw bit reads in `key_event`
//! cannot drift from the codec.

use crate::common;

use proptest::prelude::*;
use tutti_ump::{
    Address, Key, KeyEvent, Message, MidiChannel, MidiGroup, NoteNumber, Protocol, Ump, Voice1,
    Voice2,
};

const SUSTAIN: u8 = 64;

fn expected(message: Message) -> Option<KeyEvent> {
    match message {
        Message::Midi1(a, v) => {
            let key = |note: NoteNumber| Key::new(Protocol::Midi1, a, note).unwrap();
            match v {
                Voice1::NoteOn { note, velocity } if velocity.get() == 0 => {
                    Some(KeyEvent::Off(key(note)))
                }
                Voice1::NoteOn { note, .. } => Some(KeyEvent::On(key(note))),
                Voice1::NoteOff { note, .. } => Some(KeyEvent::Off(key(note))),
                Voice1::ControlChange { controller, value } if controller.get() == SUSTAIN => {
                    Some(KeyEvent::Sustain {
                        protocol: Protocol::Midi1,
                        address: a,
                        down: value.get() >= 64,
                    })
                }
                _ => None,
            }
        }
        Message::Midi2(a, v) => {
            let key = |note: NoteNumber| Key::new(Protocol::Midi2, a, note).unwrap();
            match v {
                Voice2::NoteOn { note, .. } => Some(KeyEvent::On(key(note))),
                Voice2::NoteOff { note, .. } => Some(KeyEvent::Off(key(note))),
                Voice2::ControlChange { controller, value } if controller.get() == SUSTAIN => {
                    Some(KeyEvent::Sustain {
                        protocol: Protocol::Midi2,
                        address: a,
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
        let want = ump.decode().ok().and_then(expected);
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
        prop_assert!(off.decode().is_ok());
        let protocol = if m2 { Protocol::Midi2 } else { Protocol::Midi1 };
        let address = Address::from_index(usize::from(lane_)).unwrap();
        let up = Ump::sustain_up(protocol, address).unwrap();
        prop_assert_eq!(up.key_event(), Some(KeyEvent::Sustain { protocol, address, down: false }));
        prop_assert!(up.decode().is_ok());
    }
}

#[test]
fn a_key_carries_its_fields() {
    let address = Address::new(MidiGroup::of(1), MidiChannel::of(0xf));
    let key = Key::new(Protocol::Midi2, address, NoteNumber::of(0x45)).unwrap();
    assert_eq!(
        (key.protocol(), key.address(), key.note()),
        (Protocol::Midi2, address, NoteNumber::of(0x45))
    );
    assert_eq!(Key::from_index(key.index()), key);
}

/// A reserved protocol has no channel voice message, so it opens no key and
/// closes no pedal; nothing is packed under a wrong protocol.
#[test]
fn a_reserved_protocol_has_no_key_and_no_pedal() {
    let a = Address::first(MidiChannel::FIRST);
    assert_eq!(Key::new(Protocol::Reserved(3), a, NoteNumber::of(1)), None);
    assert_eq!(Ump::sustain_up(Protocol::Reserved(3), a), None);
}

/// A table index is group * 16 + channel, and `from_index` inverts it for
/// exactly the 256 addresses there are.
#[test]
fn an_address_index_is_group_times_sixteen_plus_channel() {
    for i in 0..300usize {
        match Address::from_index(i) {
            Some(a) => {
                assert!(i < 256);
                assert_eq!(a.index(), i);
                assert_eq!((a.group.index(), a.channel.index()), (i / 16, i % 16));
            }
            None => assert!(i >= 256),
        }
    }
}

/// MIDI 2.0 reads the pedal from the 32-bit value (down from half), and the
/// controller index as `decode` does: seven bits, the eighth ignored.
#[test]
fn the_midi2_pedal_reads_index_and_value_as_decode_does() {
    let pedal = |w0: u32, w1: u32| Ump([w0, w1, 0, 0]).key_event();
    let sustain = |down| {
        Some(KeyEvent::Sustain {
            protocol: Protocol::Midi2,
            address: Address::new(MidiGroup::of(3), MidiChannel::of(3)),
            down,
        })
    };
    assert_eq!(pedal(0x43b3_4000, 0x8000_0000), sustain(true));
    assert_eq!(pedal(0x43b3_4000, 0x7fff_ffff), sustain(false));
    assert_eq!(pedal(0x43b3_4000, 0), sustain(false));
    // Controller 65 is not the pedal.
    assert_eq!(pedal(0x43b3_4100, 0xffff_ffff), None);
    // The eighth index bit is ignored, as in `decode`.
    let w = [0x43b3_c000, 0xffff_ffff, 0, 0];
    assert_eq!(
        Ump(w).decode().ok().and_then(expected),
        Ump(w).key_event(),
        "agrees with decode on an index with bit 7 set"
    );
}
