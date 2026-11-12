use super::*;

#[test]
fn pitch_bend_sensitivity_rpn_roundtrip() {
    for s in [2u8, 24, 48, 96] {
        let sens = PitchBendSensitivity::of(s);
        assert_eq!(
            PitchBendSensitivity::from_rpn_bits(sens.to_rpn_bits()),
            sens
        );
        assert_eq!(sens.semitones().get(), f32::from(s));
    }
}

/// **A whole-semitone count is 7 integer bits**: up to 127 it is exact, above
/// it `new` refuses rather than shifting bits out (128 was 0, 200 was 72).
#[test]
fn a_whole_semitone_count_above_127_is_refused() {
    for s in 0..=PitchBendSensitivity::MAX_SEMITONES {
        let sens = PitchBendSensitivity::new(s).expect("in range");
        assert_eq!(sens.semitones().get(), f32::from(s), "{s} semitones");
        assert_eq!(PitchBendSensitivity::of(s), sens);
    }
    for s in 128..=u8::MAX {
        assert_eq!(PitchBendSensitivity::new(s), None, "{s} semitones");
    }
}

#[test]
#[should_panic(expected = "at most 127 semitones")]
fn of_panics_above_127() {
    let _ = PitchBendSensitivity::of(128);
}

/// **Every `u32` is a sensitivity** (M2-104 §7.4.13.3: up to almost 128
/// semitones), so a wire value is never refused and never changes.
#[test]
fn every_rpn_word_is_in_range() {
    for bits in [0, 1, 1 << 24, 48 << 25, 0x0C00_0000, u32::MAX - 1, u32::MAX] {
        let sens = PitchBendSensitivity::from_rpn_bits(bits);
        assert_eq!(sens.to_rpn_bits(), bits);
        // The top words round to exactly 128.0 in f32; the bits are the truth.
        assert!(sens.semitones().get() <= 128.0);
    }
}

#[cfg(feature = "serde")]
#[test]
fn serde_carries_the_bare_bits() {
    let sens = PitchBendSensitivity::of(48);
    let json = serde_json::to_string(&sens).unwrap();
    assert_eq!(json, (48u32 << 25).to_string());
    assert_eq!(
        serde_json::from_str::<PitchBendSensitivity>(&json).unwrap(),
        sens
    );
    let max = serde_json::to_string(&u32::MAX).unwrap();
    assert_eq!(
        serde_json::from_str::<PitchBendSensitivity>(&max).unwrap(),
        PitchBendSensitivity::from_rpn_bits(u32::MAX)
    );
}
