use super::*;

#[test]
fn test_no_glide() {
    let config = PortamentoConfig {
        mode: PortamentoMode::Off,
        ..Default::default()
    };
    let mut porta = Portamento::new(config, SampleRate::new(44100.0));

    porta.set_target(Hz::new(880.0), false);
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
    let mut porta = Portamento::new(config, SampleRate::new(44100.0));

    porta.reset(Hz::new(440.0));
    porta.set_target(Hz::new(880.0), false);

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
    let mut porta = Portamento::new(config, SampleRate::new(44100.0));

    porta.reset(Hz::new(440.0));

    // Non-legato should not glide
    porta.set_target(Hz::new(880.0), false);
    assert!(!porta.is_gliding());
    assert!((porta.current().get() - 880.0).abs() < 0.01);

    // Legato should glide
    porta.set_target(Hz::new(440.0), true);
    assert!(porta.is_gliding());
}

#[test]
fn test_curve_shapes() {
    for curve in [Law::Linear, Law::PowerIn(2.0), Law::PowerIn(0.5)] {
        let config = PortamentoConfig {
            mode: PortamentoMode::Always,
            curve,
            time: Seconds(0.01),
            ..Default::default()
        };
        let mut porta = Portamento::new(config, SampleRate::new(44100.0));

        porta.reset(Hz::new(440.0));
        porta.set_target(Hz::new(880.0), false);

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
#[test]
fn advance_is_that_many_ticks() {
    for curve in [Law::Linear, Law::PowerIn(2.0), Law::PowerIn(0.5)] {
        let config = PortamentoConfig {
            mode: PortamentoMode::Always,
            curve,
            // 44.1 frames: a glide that ends mid-step.
            time: Seconds(0.001),
            ..Default::default()
        };
        let (mut by_tick, mut by_advance) = (
            Portamento::new(config.clone(), SampleRate::new(44100.0)),
            Portamento::new(config, SampleRate::new(44100.0)),
        );
        for target in [587.0, 311.13, 1234.5, 97.3, 2093.0] {
            for p in [&mut by_tick, &mut by_advance] {
                p.reset(Hz::new(220.0));
                p.set_target(Hz::new(target), false);
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

/// FNV-1a over the bits of every pitch a glide renders, per shape: two glides
/// (up, then a retarget half-way down) stepped by `tick` and by `advance`.
/// The pitch goes through `ln` and `exp`, which reach libm, so the digests are
/// goldens (`assert_golden`), recorded on the target named there.
#[test]
fn glide_render_digests() {
    fn digest(curve: Law) -> u64 {
        let config = PortamentoConfig {
            mode: PortamentoMode::Always,
            curve,
            time: Seconds(0.01),
            ..Default::default()
        };
        let mut porta = Portamento::new(config, SampleRate::new(48000.0));
        let mut hash = tutti_fixtures::Fnv::new();
        let mut eat = |hz: Hz| hash.f32(hz.get());
        porta.reset(Hz::new(220.0));
        porta.set_target(Hz::new(1760.0), false);
        for _ in 0..240 {
            eat(porta.tick());
        }
        porta.set_target(Hz::new(311.0), false);
        for _ in 0..300 {
            eat(porta.tick());
        }
        porta.set_target(Hz::new(95.0), false);
        for steps in [1usize, 7, 64, 3, 128, 500] {
            eat(porta.advance(steps));
        }
        hash.finish()
    }
    for (name, curve, recorded) in [
        ("linear", Law::Linear, 0xdf0c_b95a_5395_87d9),
        ("quadratic", Law::PowerIn(2.0), 0xb127_e94e_c43e_399e),
        ("square-root", Law::PowerIn(0.5), 0x4a68_7e6b_1b1a_9839),
    ] {
        tutti_fixtures::assert_golden(name, digest(curve), recorded);
    }
}
