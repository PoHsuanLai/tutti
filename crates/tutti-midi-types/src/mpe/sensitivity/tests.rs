use super::*;

#[test]
fn pitch_bend_sensitivity_rpn_roundtrip() {
    for s in [2u8, 24, 48, 96] {
        let sens = PitchBendSensitivity::from_semitones(s);
        assert_eq!(
            PitchBendSensitivity::from_rpn_bits(sens.to_rpn_bits()),
            sens
        );
        assert_eq!(sens.as_semitones_f32(), f32::from(s));
    }
}
