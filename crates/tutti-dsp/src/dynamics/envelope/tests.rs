use super::*;

/// The coefficient is the one-pole formula on the exact time:
/// `1 - exp(-1 / (0.01 * 48000))` in `f64`, rounded once to `f32`
/// (0.0020811646). Rounding 0.01 through `f32` first gives
/// 0.0020811649, a different `f32`.
#[test]
fn coefficients_come_from_the_exact_times() {
    let want = |t: f64| (1.0 - libm::exp(-1.0 / (t * 48_000.0))) as f32;
    let mut env = EnvelopeFollower::new(
        Seconds::new(0.01),
        Seconds::new(0.007),
        SampleRate::new(48_000.0),
    );
    assert_eq!(env.attack_coeff(), want(0.01));
    assert_eq!(env.release_coeff(), want(0.007));
    env.update_coefficients(Seconds::new(0.007), Seconds::new(0.01));
    assert_eq!(env.attack_coeff(), want(0.007));
    assert_eq!(env.release_coeff(), want(0.01));
}

#[test]
fn test_envelope_follower_attack() {
    let mut env = EnvelopeFollower::new(
        Seconds::new(0.001),
        Seconds::new(0.1),
        SampleRate::new(44100.0),
    );
    for _ in 0..200 {
        env.smooth(1.0);
    }
    assert!(
        env.value() > 0.9,
        "Should converge toward 1.0: {}",
        env.value()
    );
}

#[test]
fn test_envelope_follower_release() {
    let mut env = EnvelopeFollower::new(
        Seconds::new(0.001),
        Seconds::new(0.01),
        SampleRate::new(44100.0),
    );
    env.set_value(1.0);
    for _ in 0..3000 {
        env.smooth(0.0);
    }
    assert!(
        env.value() < 0.1,
        "Should converge toward 0.0: {}",
        env.value()
    );
}

#[test]
fn test_envelope_follower_reset() {
    let mut env = EnvelopeFollower::new(
        Seconds::new(0.001),
        Seconds::new(0.1),
        SampleRate::new(44100.0),
    );
    env.set_value(0.8);
    env.reset();
    assert_eq!(env.value(), 0.0);
}

#[test]
fn test_envelope_follower_lazy_coefficients() {
    let mut env = EnvelopeFollower::new(
        Seconds::new(0.001),
        Seconds::new(0.1),
        SampleRate::new(44100.0),
    );
    let orig_attack = env.attack_coeff();
    // Same values: no change
    env.update_coefficients(Seconds::new(0.001), Seconds::new(0.1));
    assert_eq!(env.attack_coeff(), orig_attack);
    // Different values: recalculate
    env.update_coefficients(Seconds::new(0.01), Seconds::new(0.1));
    assert_ne!(env.attack_coeff(), orig_attack);
}

/// **A zero attack or release is instant**: the follower lands on its
/// target in one sample, rising and falling, and a gate opens and
/// closes in one. (It used to freeze: the coefficient was the
/// retention, 1 at zero time, so a zero-attack compressor never
/// reduced.)
#[test]
fn a_zero_attack_or_release_is_instant() {
    let mut env = EnvelopeFollower::new(
        Seconds::new(0.0),
        Seconds::new(0.0),
        SampleRate::new(48_000.0),
    );
    assert_eq!(env.smooth(0.7), 0.7);
    assert!((env.smooth(0.2) - 0.2).abs() < 1e-7, "{}", env.value());

    let mut gate = GateEnvelopeFollower::new(
        Seconds::new(0.0),
        Seconds::new(0.0),
        Seconds::new(0.0),
        SampleRate::new(48_000.0),
    );
    assert_eq!(gate.step(true), 1.0);
    assert_eq!(gate.step(false), 0.0);
}

#[test]
fn test_gate_envelope_hold_phase() {
    let mut gate = GateEnvelopeFollower::new(
        Seconds::new(0.0001),
        Seconds::new(0.01),
        Seconds::new(0.001),
        SampleRate::new(44100.0),
    );
    // Open the gate
    for _ in 0..100 {
        gate.step(true);
    }
    assert!(gate.value() > 0.9);

    // Close the gate — should hold first
    gate.step(false);
    assert!(gate.value() > 0.9, "During hold, level should stay high");

    // Exhaust hold, then release
    for _ in 0..1000 {
        gate.step(false);
    }
    assert!(
        gate.value() < 0.1,
        "After hold+release, level should be low: {}",
        gate.value()
    );
}

#[test]
fn test_gate_envelope_reset() {
    let mut gate = GateEnvelopeFollower::new(
        Seconds::new(0.001),
        Seconds::new(0.01),
        Seconds::new(0.1),
        SampleRate::new(44100.0),
    );
    for _ in 0..100 {
        gate.step(true);
    }
    gate.reset();
    assert_eq!(gate.value(), 0.0);
}
