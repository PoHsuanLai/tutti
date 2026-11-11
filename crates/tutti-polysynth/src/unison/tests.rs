use super::*;

/// **A driven detune gets `set_detune`'s floor**: driven to −10 cents
/// the engine detunes by 0, as `set_detune(-10)` leaves it, and does not
/// recompute again for the same drive.
#[test]
fn a_driven_detune_is_floored_as_the_setter_floors_it() {
    let mut engine = UnisonEngine::new(UnisonConfig {
        voice_count: 3,
        detune_cents: Cents(12.0),
        ..UnisonConfig::default()
    });
    engine.sync_from_atomics([Some(-10.0), None]);
    assert_eq!(engine.config().detune_cents, Cents(0.0));
    let mut by_setter = UnisonEngine::new(UnisonConfig {
        voice_count: 3,
        ..UnisonConfig::default()
    });
    by_setter.set_detune(Cents(-10.0));
    assert_eq!(
        engine.config().detune_cents,
        by_setter.config().detune_cents
    );
}

#[test]
fn test_single_voice() {
    let config = UnisonConfig {
        voice_count: 1,
        ..Default::default()
    };
    let unison = UnisonEngine::new(config);

    assert_eq!(unison.voice_count(), 1);

    let params = unison.voice_params(0);
    assert!((params.freq_ratio - 1.0).abs() < 0.001);
    assert!((params.pan.get() - 0.0).abs() < 0.001);
    assert!((params.amplitude.get() - 1.0).abs() < 0.001);
}

#[test]
fn test_detune_spread() {
    let config = UnisonConfig {
        voice_count: 3,
        detune_cents: Cents(12.0), // ~1/8 semitone spread
        stereo_spread: Spread::POINT,
        phase_randomize: false,
    };
    let unison = UnisonEngine::new(config);

    assert_eq!(unison.voice_count(), 3);

    // Center voice should be at ratio 1.0
    let center = unison.voice_params(1);
    assert!((center.freq_ratio - 1.0).abs() < 0.001);

    // First voice should be detuned down
    let low = unison.voice_params(0);
    assert!(low.freq_ratio < 1.0);

    // Last voice should be detuned up
    let high = unison.voice_params(2);
    assert!(high.freq_ratio > 1.0);

    // Symmetric detune
    let low_ratio = 1.0 / low.freq_ratio;
    let high_ratio = high.freq_ratio;
    assert!((low_ratio - high_ratio).abs() < 0.001);
}

#[test]
fn test_stereo_spread() {
    let config = UnisonConfig {
        voice_count: 3,
        detune_cents: Cents(0.0),
        stereo_spread: Spread(1.0),
        phase_randomize: false,
    };
    let unison = UnisonEngine::new(config);

    let left = unison.voice_params(0);
    let center = unison.voice_params(1);
    let right = unison.voice_params(2);

    assert!((left.pan.get() - (-1.0)).abs() < 0.001);
    assert!((center.pan.get() - 0.0).abs() < 0.001);
    assert!((right.pan.get() - 1.0).abs() < 0.001);
}

#[test]
fn test_equal_power_amplitude() {
    for count in 1..=8 {
        let config = UnisonConfig {
            voice_count: count,
            ..Default::default()
        };
        let unison = UnisonEngine::new(config);

        // Sum of squared amplitudes should equal 1.0 (equal power)
        let sum_sq: f32 = unison
            .all_params()
            .iter()
            .map(|p| p.amplitude.get() * p.amplitude.get())
            .sum();

        assert!(
            (sum_sq - 1.0).abs() < 0.01,
            "Equal power failed for {} voices: {}",
            count,
            sum_sq
        );
    }
}

#[test]
fn test_phase_randomization() {
    let config = UnisonConfig {
        voice_count: 4,
        phase_randomize: true,
        ..Default::default()
    };
    let mut unison = UnisonEngine::new(config);

    // Seed for reproducibility
    unison.seed_rng(42);
    unison.randomize_phases();

    // Phases should be different
    let phases: Vec<f32> = unison
        .all_params()
        .iter()
        .map(|p| p.phase_offset.get())
        .collect();

    // Check phases are in valid range
    for phase in &phases {
        assert!(*phase >= 0.0 && *phase <= 1.0);
    }

    // Check they're not all the same
    let first = phases[0];
    let all_same = phases.iter().all(|p| (*p - first).abs() < 0.001);
    assert!(!all_same, "Phases should be randomized");
}

#[test]
fn test_config_update() {
    let mut unison = UnisonEngine::new(UnisonConfig::default());

    assert_eq!(unison.voice_count(), 1);

    unison.set_voice_count(5);
    assert_eq!(unison.voice_count(), 5);

    unison.set_detune(Cents::new(20.0));
    assert!((unison.config().detune_cents.get() - 20.0).abs() < 0.001);

    unison.set_stereo_spread(0.5);
    assert!((unison.config().stereo_spread.get() - 0.5).abs() < 0.001);
}

#[test]
fn test_clamp_voice_count() {
    let config = UnisonConfig {
        voice_count: 100, // Over max
        ..Default::default()
    };
    let unison = UnisonEngine::new(config);

    assert_eq!(unison.voice_count(), MAX_UNISON_VOICES);
}

/// Recomputing detune/spread keeps the drawn phases.
///
/// `sync_from_atomics` recomputes whenever a modulated detune or spread
/// moves, and in `tick` that runs after MIDI is polled — so a recompute
/// that reset the phases wiped the note-on's draw before anything used it.
#[test]
fn recompute_keeps_the_drawn_phases() {
    let mut unison = UnisonEngine::new(UnisonConfig {
        voice_count: 4,
        phase_randomize: true,
        ..Default::default()
    });
    unison.seed_rng(7);
    let drawn: Vec<f32> = unison
        .randomize_phases()
        .iter()
        .map(|p| p.phase_offset.get())
        .collect();
    unison.set_detune(Cents(25.0));
    let kept: Vec<f32> = unison
        .all_params()
        .iter()
        .map(|p| p.phase_offset.get())
        .collect();
    assert_eq!(drawn, kept, "a detune change wiped the drawn phases");
}
