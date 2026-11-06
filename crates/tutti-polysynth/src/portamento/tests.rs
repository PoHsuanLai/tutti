use super::*;

#[test]
fn test_no_glide() {
    let config = PortamentoConfig {
        mode: PortamentoMode::Off,
        ..Default::default()
    };
    let mut porta = Portamento::new(config, 44100.0);

    porta.set_target(880.0, false);
    assert!((porta.tick().get() - 880.0).abs() < 0.01);
    assert!(!porta.is_gliding());
}

#[test]
fn test_always_glide() {
    let config = PortamentoConfig {
        mode: PortamentoMode::Always,
        time: Seconds(0.01), // 10ms
        ..Default::default()
    };
    let mut porta = Portamento::new(config, 44100.0);

    porta.reset(440.0);
    porta.set_target(880.0, false);

    // Should start near 440
    let first = porta.tick();
    assert!(first.get() < 500.0);

    // Run until glide finishes
    while porta.is_gliding() {
        porta.tick();
    }

    // Should end at 880
    assert!((porta.current().get() - 880.0).abs() < 1.0);
}

#[test]
fn test_legato_only() {
    let config = PortamentoConfig {
        mode: PortamentoMode::LegatoOnly,
        time: Seconds(0.01),
        ..Default::default()
    };
    let mut porta = Portamento::new(config, 44100.0);

    porta.reset(440.0);

    // Non-legato should not glide
    porta.set_target(880.0, false);
    assert!(!porta.is_gliding());
    assert!((porta.current().get() - 880.0).abs() < 0.01);

    // Legato should glide
    porta.set_target(440.0, true);
    assert!(porta.is_gliding());
}

#[test]
fn test_curve_shapes() {
    for curve in [
        PortamentoCurve::Linear,
        PortamentoCurve::Exponential,
        PortamentoCurve::Logarithmic,
    ] {
        let config = PortamentoConfig {
            mode: PortamentoMode::Always,
            curve,
            time: Seconds(0.01),
            ..Default::default()
        };
        let mut porta = Portamento::new(config, 44100.0);

        porta.reset(440.0);
        porta.set_target(880.0, false);

        // Run to completion
        while porta.is_gliding() {
            porta.tick();
        }

        // All curves should reach the target
        assert!((porta.current().get() - 880.0).abs() < 1.0);
    }
}

/// **`advance(n)` is `n` ticks**: the same frequency returned, the same
/// `current()` and the same progress after, bit for bit (one machine, one
/// libm), for every curve and several intervals, at step lengths that end inside the glide, on
/// its last frame and past it, and for zero frames.
///
/// Mutation (run): `advance` returning `current_freq` where the glide had
/// finished at a tick's entry (`tick` returns the target there) → the
/// steps past the glide's end fail. Mutation (run): the pitch not
/// recomputed before that return (`current_freq` left at the previous
/// step's) → fails on `current()`.
#[test]
fn advance_is_that_many_ticks() {
    for curve in [
        PortamentoCurve::Linear,
        PortamentoCurve::Exponential,
        PortamentoCurve::Logarithmic,
    ] {
        let config = PortamentoConfig {
            mode: PortamentoMode::Always,
            curve,
            // 44.1 frames: a glide that ends mid-step.
            time: Seconds(0.001),
            ..Default::default()
        };
        let (mut by_tick, mut by_advance) = (
            Portamento::new(config.clone(), 44100.0),
            Portamento::new(config, 44100.0),
        );
        for target in [587.0, 311.13, 1234.5, 97.3, 2093.0] {
            for p in [&mut by_tick, &mut by_advance] {
                p.reset(220.0);
                p.set_target(target, false);
            }
            for steps in [0usize, 1, 7, 16, 0, 19, 3, 16, 16] {
                let mut want = by_tick.current();
                for _ in 0..steps {
                    want = by_tick.tick();
                }
                let got = by_advance.advance(steps);
                assert_eq!(
                    got.get().to_bits(),
                    want.get().to_bits(),
                    "{curve:?} +{steps}"
                );
                assert_eq!(
                    by_advance.current().get().to_bits(),
                    by_tick.current().get().to_bits(),
                    "{curve:?} +{steps}: current"
                );
                assert_eq!(by_advance.progress.to_bits(), by_tick.progress.to_bits());
            }
            assert!(!by_advance.is_gliding(), "the steps run past the glide");
            assert_eq!(
                by_advance.advance(1).get(),
                target,
                "a finished glide is its target"
            );
        }
    }
}
