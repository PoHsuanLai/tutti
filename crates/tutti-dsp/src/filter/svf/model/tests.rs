use super::*;

fn model(ty: SvfType, freq: f32, q: f32, gain: f32, rate: f64) -> SvfModel {
    SvfModel::new(
        SvfControls {
            ty,
            freq: Hz(freq),
            q: Q(q),
            gain: Db(gain),
        },
        SampleRate(rate),
    )
}

/// The RBJ cookbook biquad of a response (`Q` form), as coefficients: an
/// independent derivation of the same `H(z)` (the shelves use `2 sqrt(A) alpha`).
fn rbj(ty: SvfType, fc: f64, q: f64, gain_db: f64, rate: f64) -> Biquad {
    let a = 10f64.powf(gain_db / 40.0);
    let w0 = 2.0 * PI * fc / rate;
    let (sn, cs) = w0.sin_cos();
    let al = sn / (2.0 * q);
    let sa = 2.0 * a.sqrt() * al;
    let (b, d) = match ty {
        SvfType::LowPass => (
            [(1.0 - cs) / 2.0, 1.0 - cs, (1.0 - cs) / 2.0],
            [1.0 + al, -2.0 * cs, 1.0 - al],
        ),
        SvfType::HighPass => (
            [(1.0 + cs) / 2.0, -(1.0 + cs), (1.0 + cs) / 2.0],
            [1.0 + al, -2.0 * cs, 1.0 - al],
        ),
        SvfType::BandPass => ([sn / 2.0, 0.0, -sn / 2.0], [1.0 + al, -2.0 * cs, 1.0 - al]),
        SvfType::Notch => ([1.0, -2.0 * cs, 1.0], [1.0 + al, -2.0 * cs, 1.0 - al]),
        SvfType::Allpass => (
            [1.0 - al, -2.0 * cs, 1.0 + al],
            [1.0 + al, -2.0 * cs, 1.0 - al],
        ),
        SvfType::Bell => (
            [1.0 + al * a, -2.0 * cs, 1.0 - al * a],
            [1.0 + al / a, -2.0 * cs, 1.0 - al / a],
        ),
        SvfType::LowShelf => (
            [
                a * ((a + 1.0) - (a - 1.0) * cs + sa),
                2.0 * a * ((a - 1.0) - (a + 1.0) * cs),
                a * ((a + 1.0) - (a - 1.0) * cs - sa),
            ],
            [
                (a + 1.0) + (a - 1.0) * cs + sa,
                -2.0 * ((a - 1.0) + (a + 1.0) * cs),
                (a + 1.0) + (a - 1.0) * cs - sa,
            ],
        ),
        SvfType::HighShelf => (
            [
                a * ((a + 1.0) + (a - 1.0) * cs + sa),
                -2.0 * a * ((a - 1.0) + (a + 1.0) * cs),
                a * ((a + 1.0) + (a - 1.0) * cs - sa),
            ],
            [
                (a + 1.0) - (a - 1.0) * cs + sa,
                2.0 * ((a - 1.0) - (a + 1.0) * cs),
                (a + 1.0) - (a - 1.0) * cs - sa,
            ],
        ),
    };
    Biquad {
        b0: b[0] / d[0],
        b1: b[1] / d[0],
        b2: b[2] / d[0],
        a1: d[1] / d[0],
        a2: d[2] / d[0],
    }
}

const TYPES: [SvfType; 8] = [
    SvfType::LowPass,
    SvfType::HighPass,
    SvfType::BandPass,
    SvfType::Notch,
    SvfType::Allpass,
    SvfType::Bell,
    SvfType::LowShelf,
    SvfType::HighShelf,
];

/// The model's complex response is the RBJ cookbook's, the closed form of each
/// response, at DC, the cutoff, Nyquist and a log sweep between: magnitude and
/// phase, to 1e-9 of the larger. The trapezoidal form that `SvfModel` reads
/// and the direct form of the cookbook are two derivations of one `H(z)`.
#[test]
fn the_response_is_the_rbj_closed_form() {
    let rate = 48_000.0;
    for ty in TYPES {
        for (fc, q, gain) in [
            (1_000.0f32, 0.707f32, 9.0f32),
            (250.0, 4.0, -12.0),
            (7_000.0, 1.3, 6.0),
        ] {
            // The controls are `f32`: the closed form is read at the same figures.
            let (fc, q, gain) = (f64::from(fc), f64::from(q), f64::from(gain));
            let want = rbj(ty, fc, q, gain, rate);
            let m = model(ty, fc as f32, q as f32, gain as f32, rate);
            let mut freqs = vec![Hz(0.0), Hz(fc as f32), Hz(24_000.0)];
            freqs.extend(
                (0..40).map(|i| Hz(libm::exp(libm::log(10.0) + 7.0 * f64::from(i) / 39.0) as f32)),
            );
            let mut got = vec![Complex64::new(0.0, 0.0); freqs.len()];
            m.frequency_response(0, &freqs, &mut got);
            for (f, g) in freqs.iter().zip(&got) {
                let w = 2.0 * PI * f64::from(f.get()) / rate;
                let h = want.response(w);
                let scale = h.norm().max(g.norm()).max(1e-12);
                assert!(
                    (g - h).norm() <= 1e-9 * scale + 1e-12,
                    "{ty:?} fc {fc} Q {q} {gain} dB at {f:?}: {g} against {h}"
                );
            }
        }
    }
}

/// `biquad()` is the same `H(z)` as `frequency_response`: the direct form that
/// the two other realizations compute.
#[test]
fn the_direct_form_is_the_same_response() {
    for ty in TYPES {
        let m = model(ty, 1_500.0, 2.5, 7.0, 44_100.0);
        let b = m.biquad();
        for f in [30.0f32, 500.0, 1_500.0, 9_000.0, 20_000.0] {
            let mut g = [Complex64::new(0.0, 0.0)];
            m.frequency_response(0, &[Hz(f)], &mut g);
            let h = b.response(2.0 * PI * f64::from(f) / 44_100.0);
            assert!(
                (g[0] - h).norm() <= 1e-9,
                "{ty:?} at {f}: {} against {h}",
                g[0]
            );
        }
    }
}

/// The spectral radius is the slowest pole of the direct form's own
/// denominator, found independently from `a1` and `a2` (a conjugate pair has
/// `|p|^2 = a2`; a real pair is the quadratic's roots), and the model's poles are
/// its roots.
#[test]
fn the_radius_is_the_slowest_root_of_the_denominator() {
    for (q, fc) in [
        (0.1, 500.0),
        (0.4, 2_000.0),
        (0.707, 1_000.0),
        (5.0, 80.0),
        (300.0, 40.0),
    ] {
        let m = model(SvfType::LowPass, fc, q, 0.0, 48_000.0);
        let b = m.biquad();
        let disc = b.a1 * b.a1 - 4.0 * b.a2;
        let want = if disc < 0.0 {
            b.a2.sqrt()
        } else {
            let s = disc.sqrt();
            ((-b.a1 + s) / 2.0).abs().max(((-b.a1 - s) / 2.0).abs())
        };
        assert!(
            (m.spectral_radius() - want).abs() <= 1e-12 + 1e-9 * (1.0 - want),
            "Q {q}: {} against {want}",
            m.spectral_radius()
        );
        for p in m.poles() {
            assert!((p * p + b.a1 * p + b.a2).norm() < 1e-9, "Q {q}: {p}");
        }
    }
}

/// Non-finite controls take the family's default and a huge `Q` is held at
/// `MAX_Q`, so every model is one that can be run: finite coefficients, a
/// radius under 1.
#[test]
fn a_hostile_control_still_gives_a_stable_model() {
    let rate = SampleRate(48_000.0);
    for (f, q, g) in [
        (f32::NAN, 1.0, 0.0),
        (1_000.0, f32::NAN, 0.0),
        (1_000.0, f32::INFINITY, 6.0),
        (1.0, 1e30, 1e30),
        (-5.0, -1.0, f32::NEG_INFINITY),
    ] {
        let m = SvfModel::new(
            SvfControls {
                ty: SvfType::Bell,
                freq: Hz(f),
                q: Q(q),
                gain: Db(g),
            },
            rate,
        );
        let c = m.coeffs();
        assert!(
            [c.g, c.k, c.a1, c.a2, c.a3, c.m0, c.m1, c.m2]
                .iter()
                .all(|v| v.is_finite())
        );
        assert!(m.spectral_radius() < 1.0, "{f} {q} {g}");
    }
}
