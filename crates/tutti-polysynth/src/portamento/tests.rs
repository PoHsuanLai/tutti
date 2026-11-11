use super::*;
use tutti_curve::Exponent;

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
    for curve in [
        Law::Linear,
        Law::PowerIn(Exponent::of(2.0)),
        Law::PowerIn(Exponent::of(0.5)),
    ] {
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
    for curve in [
        Law::Linear,
        Law::PowerIn(Exponent::of(2.0)),
        Law::PowerIn(Exponent::of(0.5)),
    ] {
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

/// **A glide follows its law in cents**: `begin(from, to, time)` over
/// `time · rate` frames puts the pitch, `k` frames in, at `law(k / frames)` of
/// the interval in cents (pitch is exponential in time for `Law::Linear`), and
/// it arrives exactly on the frame the glide's length rounds up to, where the
/// pitch the voice plays (`ratio`) is the target's, for each law and an interval up, down and across several octaves. The
/// tolerance (0.05 cents, inaudible) covers the `f32` progress sum and the
/// `ln`/`exp` of the pitch.
#[test]
fn a_glide_follows_its_law_in_cents() {
    let rate = SampleRate::new(48_000.0);
    for law in [
        Law::Linear,
        Law::PowerIn(Exponent::of(2.0)),
        Law::PowerIn(Exponent::of(0.5)),
    ] {
        for (from, to, time) in [
            (220.0f32, 1760.0f32, 0.01),
            (1760.0, 311.0, 0.0073),
            (95.0, 97.0, 0.05),
        ] {
            let mut porta = Portamento::new(
                PortamentoConfig {
                    mode: PortamentoMode::Always,
                    curve: law,
                    ..Default::default()
                },
                rate,
            );
            porta.begin(Hz::new(from), Hz::new(to), Seconds(time));
            let frames = time * rate.get();
            let interval = 1200.0 * f64::from(to / from).log2();
            let mut k = 0usize;
            while porta.is_gliding() {
                k += 1;
                let got = 1200.0 * f64::from(porta.tick().get() / from).log2();
                let p = ((k as f64) / frames).min(1.0) as f32;
                let want = interval * f64::from(law.eval(p));
                assert!(
                    (got - want).abs() < 0.05,
                    "{law:?} {from} -> {to} Hz, frame {k}: {got} cents, the law gives {want}"
                );
            }
            assert_eq!(
                k,
                frames.ceil() as usize,
                "{law:?} {from} -> {to} Hz: arrival frame"
            );
            assert_eq!(porta.ratio(), 1.0, "{law:?}: the voice plays the target");
        }
    }
}
