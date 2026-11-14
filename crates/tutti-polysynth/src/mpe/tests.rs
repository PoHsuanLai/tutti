use super::*;

/// An expression value is within `0..=1`: `new` and `of` agree, and NaN, the
/// infinities and everything outside the range are refused.
#[test]
fn expression_keeps_the_range_checked_shape() {
    for raw in [0.0, 0.25, 0.5, 1.0] {
        let value = Expression::new(raw).expect("in range");
        assert_eq!(value.get(), raw);
        assert_eq!(Expression::of(raw), value);
    }
    for raw in [-0.001, 1.001, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert_eq!(Expression::new(raw), None, "new accepted {raw}");
        assert!(std::panic::catch_unwind(|| Expression::of(raw)).is_err());
    }
}

/// **A controller word spans the whole range**: zero is the bottom, `u32::MAX`
/// the top, and the middle word the middle.
#[test]
fn a_controller_word_spans_the_range() {
    assert_eq!(
        Expression::from_control(ControlValue::new(0)),
        Expression::MIN
    );
    assert_eq!(
        Expression::from_control(ControlValue::new(u32::MAX)),
        Expression::MAX
    );
    let middle = Expression::from_control(ControlValue::new(u32::MAX / 2)).get();
    assert!((middle - 0.5).abs() < 1e-6, "the middle word is {middle}");
}

/// **A voice's expression resets to its defaults**: no pressure, the slide at
/// its centre, the gain at unity.
#[test]
fn reset_returns_the_controllers_to_their_defaults() {
    let mut state = MpeVoiceState::new();
    state.apply(MpeControl::Pressure(Expression::MAX));
    state.apply(MpeControl::Slide(Expression::MAX));
    state.apply(MpeControl::Gain(Expression::MIN));
    state.reset();
    assert_eq!(state.pressure, Expression::MIN);
    assert_eq!(state.slide, SLIDE_CENTER);
    assert_eq!(state.gain, Expression::MAX);
}
