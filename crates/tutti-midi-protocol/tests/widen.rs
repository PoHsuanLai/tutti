//! `widen` against figures that do not come from its own code: the tables of
//! M2-115 section 3.3.3, a bit-by-bit statement of the repeat rule, and the
//! core rules of section 3.2, each over every value (128 seven-bit values and
//! 16,384 fourteen-bit values).
//!
//! Mutation record (each planted in `src/`, seen to fail at the named tests,
//! restored):
//! - the repeat dropped (`upscale` returns `shifted`) -> `m2_115_table_5_velocity`,
//!   `m2_115_table_6_controller_value`, `seven_bit_values_match_the_repeat_rule`
//!   and `the_core_rules_hold` (maximum);
//! - `value <= centre` made `value < centre` in `upscale` changes nothing: at the
//!   centre the low bits are zero, so the repeat adds nothing. Not a mutation
//!   any test can see;
//! - the velocity-0 Note On arm removed -> `a_note_on_at_velocity_0_is_a_note_off_at_the_centre`;
//! - its release velocity set to 0 -> the same test;
//! - the Note Off arm using `NOTE_ON_ZERO_RELEASE` -> `a_note_off_keeps_its_velocity`;
//! - Pitch Bend widened as 7 to 32 bits -> `fourteen_bit_bends_match_the_repeat_rule`;
//! - a Program Change given a bank -> `every_variant_widens_and_keeps_its_fields`;
//! - Poly Pressure widened through the 16-bit velocity scaler -> `every_variant_widens_and_keeps_its_fields`.

use tutti_midi_protocol::widen;
use tutti_ump::{
    Address, Attribute, CCNumber, Message, NoteNumber, U7, U14, Ump, Voice1, Voice2, decode,
};

/// M2-115 section 3.3 stated bit by bit: up to the centre a left shift; above
/// it, the low `src - 1` bits repeated, most significant first, into the
/// `dst - src` new low bits.
fn repeat_rule(v: u32, src: u32, dst: u32) -> u32 {
    let s = dst - src;
    let w = src - 1;
    if v <= 1 << w {
        return v << s;
    }
    let r = v & ((1 << w) - 1);
    let mut out = v << s;
    for k in 0..s {
        let source_bit = w - 1 - (k % w);
        if r >> source_bit & 1 == 1 {
            out |= 1 << (s - 1 - k);
        }
    }
    out
}

fn velocity(v: u8) -> u16 {
    match widen(Voice1::NoteOff {
        note: NoteNumber::of(60),
        velocity: U7::of(v),
    }) {
        Voice2::NoteOff { velocity, .. } => velocity,
        other => panic!("{other:?}"),
    }
}
fn value(v: u8) -> u32 {
    match widen(Voice1::ControlChange {
        controller: CCNumber::of(74),
        value: U7::of(v),
    }) {
        Voice2::ControlChange { value, .. } => value,
        other => panic!("{other:?}"),
    }
}
fn bend(v: u16) -> u32 {
    match widen(Voice1::PitchBend(U14::of(v))) {
        Voice2::PitchBend(b) => b,
        other => panic!("{other:?}"),
    }
}

/// Table 5, upscale 7 to 16 bits.
#[test]
fn m2_115_table_5_velocity() {
    for (v, want) in [
        (0, 0x0000),
        (5, 0x0A00),
        (30, 0x3C00),
        (32, 0x4000),
        (64, 0x8000),
        (70, 0x8C30),
        (96, 0xC104),
        (120, 0xF1C7),
        (127, 0xFFFF),
    ] {
        assert_eq!(velocity(v), want, "velocity {v}");
    }
}

/// Table 6, upscale 7 to 32 bits.
#[test]
fn m2_115_table_6_controller_value() {
    for (v, want) in [
        (0, 0x0000_0000),
        (5, 0x0A00_0000),
        (30, 0x3C00_0000),
        (32, 0x4000_0000),
        (64, 0x8000_0000),
        (70, 0x8C30_C30C),
        (96, 0xC104_1041),
        (120, 0xF1C7_1C71),
        (127, 0xFFFF_FFFF),
    ] {
        assert_eq!(value(v), want, "value {v}");
    }
}

#[test]
fn seven_bit_values_match_the_repeat_rule() {
    for v in 0..=127u8 {
        assert_eq!(velocity(v), repeat_rule(v.into(), 7, 16) as u16, "{v}");
        assert_eq!(value(v), repeat_rule(v.into(), 7, 32), "{v}");
    }
}

#[test]
fn fourteen_bit_bends_match_the_repeat_rule() {
    for v in 0..=16383u16 {
        assert_eq!(bend(v), repeat_rule(v.into(), 14, 32), "{v}");
    }
}

/// Section 3.2: the minimum, centre and maximum land on the same points, a
/// higher value never maps lower, and shifting back down recovers the value.
#[test]
fn the_core_rules_hold() {
    assert_eq!(
        (velocity(0), velocity(64), velocity(127)),
        (0, 0x8000, 0xFFFF)
    );
    assert_eq!(
        (value(0), value(64), value(127)),
        (0, 0x8000_0000, u32::MAX)
    );
    assert_eq!(
        (bend(0), bend(0x2000), bend(0x3FFF)),
        (0, 0x8000_0000, u32::MAX)
    );
    for v in 0..127u8 {
        assert!(velocity(v) < velocity(v + 1), "{v}");
        assert!(value(v) < value(v + 1), "{v}");
    }
    for v in 0..16383u16 {
        assert!(bend(v) < bend(v + 1), "{v}");
    }
    for v in 0..=127u8 {
        assert_eq!(velocity(v) >> 9, v.into(), "{v}");
        assert_eq!(value(v) >> 25, v.into(), "{v}");
    }
    for v in 0..=16383u16 {
        assert_eq!(bend(v) >> 18, v.into(), "{v}");
    }
}

/// D.3.1: a Note On at velocity 0 is a Note Off at `0x8000`, and 1 to 127 are
/// scaled (velocity 1 is `0x0200`).
#[test]
fn a_note_on_at_velocity_0_is_a_note_off_at_the_centre() {
    assert_eq!(
        widen(Voice1::NoteOn {
            note: NoteNumber::of(60),
            velocity: U7::of(0)
        }),
        Voice2::NoteOff {
            note: NoteNumber::of(60),
            velocity: 0x8000,
            attribute: Attribute::None,
        }
    );
    assert_eq!(
        widen(Voice1::NoteOn {
            note: NoteNumber::of(60),
            velocity: U7::of(1)
        }),
        Voice2::NoteOn {
            note: NoteNumber::of(60),
            velocity: 0x0200,
            attribute: Attribute::None,
        }
    );
    for v in 1..=127u8 {
        assert_eq!(
            widen(Voice1::NoteOn {
                note: NoteNumber::of(61),
                velocity: U7::of(v)
            }),
            Voice2::NoteOn {
                note: NoteNumber::of(61),
                velocity: velocity(v),
                attribute: Attribute::None,
            },
            "{v}"
        );
    }
}

/// A Note Off is not the velocity-0 rule: its release velocity is scaled, so 0
/// stays 0.
#[test]
fn a_note_off_keeps_its_velocity() {
    assert_eq!(velocity(0), 0);
    assert_eq!(velocity(64), 0x8000);
    assert_eq!(velocity(1), 0x0200);
}

#[test]
fn every_variant_widens_and_keeps_its_fields() {
    let n = NoteNumber::of(72);
    assert_eq!(
        widen(Voice1::PolyPressure {
            note: n,
            pressure: U7::of(96)
        }),
        Voice2::PolyPressure {
            note: n,
            value: 0xC104_1041
        }
    );
    assert_eq!(
        widen(Voice1::ChannelPressure(U7::of(120))),
        Voice2::ChannelPressure(0xF1C7_1C71)
    );
    // No bank: choosing one needs the latched Bank Select.
    assert_eq!(
        widen(Voice1::ProgramChange(U7::of(33))),
        Voice2::ProgramChange {
            program: U7::of(33),
            bank: None
        }
    );
    // Every controller, RPN/NRPN and Bank Select selectors included, widens by
    // value and stays a Control Change on the same controller.
    for c in 0..=127u8 {
        for v in [0u8, 1, 63, 64, 65, 127] {
            assert_eq!(
                widen(Voice1::ControlChange {
                    controller: CCNumber::of(c),
                    value: U7::of(v)
                }),
                Voice2::ControlChange {
                    controller: CCNumber::of(c),
                    value: value(v)
                },
                "CC {c} = {v}"
            );
        }
    }
}

/// The result is a message the codec writes and reads back, and the address is
/// the caller's: widening never touches it.
#[test]
fn the_widened_message_is_a_message() {
    let address = Address {
        group: tutti_ump::MidiGroup::of(5),
        channel: tutti_ump::MidiChannel::of(9),
    };
    let voice = widen(Voice1::NoteOn {
        note: NoteNumber::of(60),
        velocity: U7::of(64),
    });
    let ump = Ump::from(Message::Midi2(address, voice));
    // Group 5, channel 9, Note On, note 60, velocity 0x8000, no attribute.
    assert_eq!(ump, Ump([0x4599_3C00, 0x8000_0000, 0, 0]));
    assert_eq!(decode(ump), Ok(Message::Midi2(address, voice)));
}
