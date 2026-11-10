//! `narrow_*` against figures that do not come from their own code: Table 8 of
//! M2-115 section 3.4, the bit-level statement of the rule at the other widths,
//! and the round trip through `widen` for every 7-bit and 14-bit value.
//!
//! Mutation record (each planted in `src/narrow.rs`, seen to fail at the named
//! tests, restored):
//! - `narrow_velocity` shifting by 8 (masked to 7 bits) -> `m2_115_table_8_velocity`,
//!   `the_extremes_and_the_centre_keep_their_places`, `the_top_bits_are_kept` and
//!   `widen_then_narrow_returns_every_seven_bit_value`;
//! - `narrow_velocity` rounding (adding `0x100` before the shift) -> the same four;
//! - `narrow_value` shifting by 24 -> `the_extremes_and_the_centre_keep_their_places`,
//!   `the_top_bits_are_kept` and `widen_then_narrow_returns_every_seven_bit_value`;
//! - `narrow_bend` shifting by 17 -> `the_extremes_and_the_centre_keep_their_places`,
//!   `the_top_bits_are_kept` and `widen_then_narrow_returns_every_fourteen_bit_bend`;
//! - `narrow_bend` shifting by 25 (the 7-bit shift) -> the same three.

use tutti_midi_protocol::{narrow_bend, narrow_value, narrow_velocity, widen};
use tutti_ump::{CCNumber, NoteNumber, U7, U14, Voice1, Voice2};

/// Table 8, downscale 16 to 7 bits. `0x15FF` is not in the table: it is the
/// value Table 10's rounding method sends to 11, which the plain shift of
/// section 3.4 sends to 10.
#[test]
fn m2_115_table_8_velocity() {
    for (v, want) in [
        (5120, 10),
        (32768, 64),
        (44730, 87),
        (65535, 127),
        (0x15FF, 10),
    ] {
        assert_eq!(narrow_velocity(v).get(), want, "{v:#x}");
    }
}

/// The centre and the extremes land on the centre and the extremes (section
/// 3.2), at every width.
#[test]
fn the_extremes_and_the_centre_keep_their_places() {
    assert_eq!(
        [0, 0x8000, 0xFFFF].map(|v| narrow_velocity(v).get()),
        [0, 64, 127]
    );
    assert_eq!(
        [0, 0x8000_0000, u32::MAX].map(|v| narrow_value(v).get()),
        [0, 64, 127]
    );
    assert_eq!(
        [0, 0x8000_0000, u32::MAX].map(|v| narrow_bend(v).get()),
        [0, 8192, 16383]
    );
}

/// Section 3.4 stated bit by bit: the result is the source's top bits, so the
/// value is exact on the multiple of the step and constant over the step.
#[test]
fn the_top_bits_are_kept() {
    for v in 0..=127u32 {
        let step = 1u32 << 9;
        assert_eq!(narrow_velocity((v * step) as u16).get(), v as u8);
        assert_eq!(narrow_velocity((v * step + step - 1) as u16).get(), v as u8);
        let step = 1u32 << 25;
        assert_eq!(narrow_value(v * step).get(), v as u8);
        assert_eq!(narrow_value(v * step + (step - 1)).get(), v as u8);
    }
    for v in 0..=16383u32 {
        let step = 1u32 << 18;
        assert_eq!(narrow_bend(v * step).get(), v as u16);
        assert_eq!(narrow_bend(v * step + (step - 1)).get(), v as u16);
    }
}

#[test]
fn widen_then_narrow_returns_every_seven_bit_value() {
    for v in 0..=127u8 {
        let off = widen(Voice1::NoteOff {
            note: NoteNumber::of(60),
            velocity: U7::of(v),
        });
        let Voice2::NoteOff { velocity, .. } = off else {
            panic!("{off:?}")
        };
        assert_eq!(narrow_velocity(velocity), U7::of(v), "velocity {v}");

        let cc = widen(Voice1::ControlChange {
            controller: CCNumber::of(7),
            value: U7::of(v),
        });
        let Voice2::ControlChange { value, .. } = cc else {
            panic!("{cc:?}")
        };
        assert_eq!(narrow_value(value), U7::of(v), "value {v}");
    }
}

#[test]
fn widen_then_narrow_returns_every_fourteen_bit_bend() {
    for v in 0..=16383u16 {
        let bend = U14::of(v);
        let Voice2::PitchBend(wide) = widen(Voice1::PitchBend(bend)) else {
            panic!("{v}")
        };
        assert_eq!(narrow_bend(wide), bend, "bend {v}");
    }
}
