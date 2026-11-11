use tutti_types::{Amplitude, Db, Seconds, StereoWidth};

use super::*;
use crate::network::{Gains, Inline, Modulation, OutputMap, Tap, Taps};

const RATE: SampleRate = SampleRate(48_000.0);

fn gains(v: &[f32]) -> Gains {
    Gains::from_slice(v).unwrap()
}

fn line(length: f32, absorption: Absorption) -> Line {
    Line {
        length,
        modulation: Modulation::NONE,
        absorption,
        diffuser: None,
    }
}

fn gain(g: f32) -> Absorption {
    Absorption::Gain(Amplitude(g))
}

/// An FDN over `lines`, fed `input` and read through one row `output`.
fn fdn(lines: &[Line], matrix: Matrix, input: &[f32], output: &[f32]) -> Fdn {
    let mut rows = Inline::new();
    rows.push(gains(output)).unwrap();
    Fdn {
        lines: Inline::from_slice(lines).unwrap(),
        matrix,
        input: gains(input),
        output: OutputMap { rows },
    }
}

fn network(stages: Vec<Stage>) -> Network {
    let mut n = Network::new(RATE);
    for s in stages {
        n.stages.push(s).unwrap();
    }
    n
}

/// `H` of `n` at the angular frequency `w` (radians a frame), output 0.
fn at(n: &Network, w: f64) -> Complex64 {
    let hz = w / std::f64::consts::TAU * RATE.get();
    let mut g = [ZERO];
    n.frequency_response(0, &[Hz(hz as f32)], &mut g);
    g[0]
}

fn close(a: Complex64, b: Complex64) -> bool {
    (a - b).norm() <= 1e-4 * b.norm().max(1e-3)
}

/// The Freeverb all-pass `((1 + g) z^-D - 1) / (1 - g z^-D)` is 1 at DC for
/// `g = 0.5` and `(2 + g) / (1 + g)` where `D w = pi`; the lattice form is
/// unit magnitude everywhere. The figures are the ones in the form's own
/// documentation (`AllPassForm`), derived from the transfer function.
#[test]
fn the_two_all_pass_forms_have_their_documented_gains() {
    let ap = |form| {
        let a = AllPass {
            delay: 8.0,
            gain: 0.5,
            modulation: Modulation::NONE,
            form,
        };
        let mut channels = Inline::new();
        channels.push(a).unwrap();
        network(vec![Stage::AllPass(channels)])
    };
    let (lattice, freeverb) = (ap(AllPassForm::Lattice), ap(AllPassForm::Freeverb));
    let pi_over_d = std::f64::consts::PI / 8.0;
    assert!((at(&freeverb, 0.0).norm() - 1.0).abs() < 1e-5);
    assert!((at(&freeverb, pi_over_d).norm() - 2.5 / 1.5).abs() < 1e-5);
    for w in [0.0, 0.3, pi_over_d, 2.0, 3.0] {
        assert!(
            (at(&lattice, w).norm() - 1.0).abs() < 1e-5,
            "lattice at {w}"
        );
    }
    // The peak the bound uses is that of the form.
    assert!((freeverb.hinf_norm() - 2.5 / 1.5).abs() < 1e-9);
    assert!((lattice.hinf_norm() - 1.0).abs() < 1e-9);
}

/// The l1 norm of each all-pass form is the sum of its impulse response
/// (`1 + 2|g|` for the lattice, `1 + 1 / (1 - |g|)` for Freeverb's), both
/// signs of `g`.
#[test]
fn an_all_pass_has_the_l1_norm_of_its_impulse_response() {
    for (form, g, want) in [
        (AllPassForm::Lattice, 0.5, 2.0),
        (AllPassForm::Lattice, -0.5, 2.0),
        (AllPassForm::Freeverb, 0.5, 3.0),
        (AllPassForm::Freeverb, -0.5, 3.0),
    ] {
        let a = AllPass {
            delay: 4.0,
            gain: g,
            modulation: Modulation::NONE,
            form,
        };
        let mut channels = Inline::new();
        channels.push(a).unwrap();
        let n = network(vec![Stage::AllPass(channels)]);
        // The impulse response from the difference equation.
        let d = 4usize;
        let len = 80 * d;
        let (mut h, mut buf) = (vec![0.0f64; len], vec![0.0f64; len]);
        let g = f64::from(g);
        for t in 0..len {
            let x = if t == 0 { 1.0 } else { 0.0 };
            let back = |v: &[f64]| if t >= d { v[t - d] } else { 0.0 };
            match form {
                AllPassForm::Lattice => {
                    h[t] = -g * x + (if t == d { 1.0 } else { 0.0 }) + g * back(&h);
                }
                AllPassForm::Freeverb => {
                    buf[t] = x + g * back(&buf);
                    h[t] = back(&buf) - x;
                }
            }
        }
        let measured: f64 = h.iter().map(|x| x.abs()).sum();
        assert!((measured - want).abs() < 1e-6, "{form:?} {g}: {measured}");
        assert!(
            (n.l1_norm() - want).abs() < 1e-9,
            "{form:?} {g}: {}",
            n.l1_norm()
        );
    }
}

/// A circulant's first row is row 0 of `A`, each row after it shifted one
/// place right: `A[i][j] = c[(j - i) mod n]`. With `c = (0, 1, 0)` line 0 feeds
/// line 2, line 2 feeds line 1 and line 1 feeds line 0, so an input on line 0
/// read on line 2 is `b c g z^-(m0 + m2) / (1 - g^3 z^-(m0 + m1 + m2))`.
#[test]
fn a_circulant_feeds_the_next_line_round_the_cycle() {
    let (m, g) = ([30.0f32, 41.0, 53.0], 0.6f32);
    let lines = m.map(|m| line(m, gain(g)));
    let f = fdn(
        &lines,
        Matrix::Circulant(gains(&[0.0, 1.0, 0.0])),
        &[1.0, 0.0, 0.0],
        &[0.0, 0.0, 1.0],
    );
    let n = network(vec![Stage::Fdn(f)]);
    for w in [0.0, 0.1, 0.77, 2.0, 3.1] {
        let z = |d: f32| delay(w, f64::from(d));
        let want =
            z(m[0] + m[2]) * f64::from(g) / (ONE - z(m[0] + m[1] + m[2]) * f64::from(g).powi(3));
        assert!(
            close(at(&n, w), want),
            "w {w}: {:?} against {want:?}",
            at(&n, w)
        );
    }
}

/// A Hadamard matrix is the Sylvester one over `sqrt(n)`, solved by
/// elimination: it agrees with the same matrix written out entry by entry
/// as a dense one (the literal below is `H_4 / 2`).
#[test]
fn a_hadamard_is_the_sylvester_matrix() {
    let m = [31.0f32, 37.0, 43.0, 47.0];
    let lines = m.map(|m| line(m, gain(0.7)));
    let (b, c) = ([1.0, 0.5, 0.25, 0.125], [0.3, 0.1, 0.2, 0.4]);
    let hadamard = network(vec![Stage::Fdn(fdn(&lines, Matrix::Hadamard, &b, &c))]);
    let h = [
        [1.0f32, 1.0, 1.0, 1.0],
        [1.0, -1.0, 1.0, -1.0],
        [1.0, 1.0, -1.0, -1.0],
        [1.0, -1.0, -1.0, 1.0],
    ];
    let flat: Vec<f32> = h.iter().flatten().map(|x| x / 2.0).collect();
    let dense = network(vec![Stage::Fdn(fdn(
        &lines,
        Matrix::Dense(Inline::from_slice(&flat).unwrap()),
        &b,
        &c,
    ))]);
    for w in [0.0, 0.2, 1.1, 3.0] {
        assert!(close(at(&hadamard, w), at(&dense, w)), "w {w}");
    }
}

/// The rank-one solve of a Householder or a reflection equals the
/// elimination over the same matrix written as a dense one.
#[test]
fn the_rank_one_solve_equals_elimination() {
    let m = [101.0f32, 113.0, 127.0, 139.0, 149.0];
    let lines = m.map(|m| {
        line(
            m,
            Absorption::OnePole {
                gain: Amplitude(0.8),
                damp: 0.3,
            },
        )
    });
    let (b, c) = ([1.0, 0.5, 0.25, 0.125, 0.0625], [0.3, -0.1, 0.2, 0.4, -0.2]);
    let n = 5usize;
    let v = [0.5f32, 0.5, 0.5, 0.5, 0.0];
    let norm = libm::sqrt(f64::from(v.iter().map(|x| x * x).sum::<f32>()));
    let v: Vec<f32> = v.iter().map(|x| (f64::from(*x) / norm) as f32).collect();
    let dense_of = |entry: &dyn Fn(usize, usize) -> f64| {
        let all: Vec<f32> = (0..n * n).map(|k| entry(k / n, k % n) as f32).collect();
        Matrix::Dense(Inline::from_slice(&all).unwrap())
    };
    let id = |i: usize, j: usize| f64::from(u8::from(i == j));
    let householder = dense_of(&|i, j| id(i, j) - 2.0 / n as f64);
    let reflection = dense_of(&|i, j| id(i, j) - 2.0 * f64::from(v[i]) * f64::from(v[j]));
    for (fast, slow) in [
        (Matrix::Householder, householder),
        (Matrix::Reflection(gains(&v)), reflection),
    ] {
        let a = network(vec![Stage::Fdn(fdn(&lines, fast, &b, &c))]);
        let d = network(vec![Stage::Fdn(fdn(&lines, slow, &b, &c))]);
        for w in [0.0, 0.05, 0.9, 2.5, std::f64::consts::PI] {
            assert!(
                close(at(&a, w), at(&d, w)),
                "w {w}: {:?} {:?}",
                at(&a, w),
                at(&d, w)
            );
        }
    }
}

/// The two-line dense solve is Cramer's rule: `s = (I - A T)^-1 b` with `A =
/// [[a, b], [c, d]]` and `T = diag(t0, t1)`.
///
/// The lines' gain is 3 (the algebra does not care), so the second row's pivot is
/// the larger and the elimination swaps.
#[test]
fn elimination_is_cramers_rule_on_two_lines() {
    let m = [20.0f32, 33.0];
    let lines = m.map(|m| line(m, gain(3.0)));
    let a = [[0.0f32, 1.0], [1.0, 0.0]];
    let flat: Vec<f32> = a.iter().flatten().copied().collect();
    let n = network(vec![Stage::Fdn(fdn(
        &lines,
        Matrix::Dense(Inline::from_slice(&flat).unwrap()),
        &[1.0, 0.0],
        &[0.0, 1.0],
    ))]);
    for w in [0.0, 0.4, 1.7] {
        let t = [delay(w, 20.0) * 3.0, delay(w, 33.0) * 3.0];
        // (I - A T) = [[1, -t1], [-t0, 1]]; s = M^-1 (1, 0)^T = (1, t0) / (1 - t0 t1).
        let det = ONE - t[0] * t[1];
        let s1 = t[0] / det;
        let want = s1 * delay(w, 33.0);
        assert!(close(at(&n, w), want), "w {w}");
    }
}

/// The spectral radius of a diagonal network of pure gains is the exact
/// `max g_i^(1/m_i)`, of a lossless one 1, of taps and a FIR 0, and of
/// an all-pass `|g|^(1/D)`; of a loop the analysis cannot accept, infinity.
#[test]
fn the_spectral_radius_is_the_slowest_decay_per_frame() {
    let (m, g) = ([1000.0f32, 2000.0], [0.9f32, 0.99]);
    let f = fdn(
        &[line(m[0], gain(g[0])), line(m[1], gain(g[1]))],
        Matrix::Diagonal,
        &[1.0, 1.0],
        &[1.0, 1.0],
    );
    let want = (0..2)
        .map(|i| libm::pow(f64::from(g[i]), 1.0 / f64::from(m[i])))
        .fold(0.0, f64::max);
    let n = network(vec![Stage::Fdn(f)]);
    assert!(
        (n.spectral_radius() - want).abs() < 1e-6,
        "{}",
        n.spectral_radius()
    );
    assert!(n.spectral_radius() < 1.0);

    let frozen = network(vec![Stage::Fdn(fdn(
        &[line(500.0, gain(1.0)), line(700.0, gain(1.0))],
        Matrix::Householder,
        &[1.0, 1.0],
        &[1.0, 1.0],
    ))]);
    assert_eq!(frozen.spectral_radius(), 1.0);
    // A reflection of a vector a rounding off unit length is still lossless
    // (to the tolerance), and its radius is 1, not the rounding's `1 + 2e-7`.
    let nearly = network(vec![Stage::Fdn(fdn(
        &[
            line(500.0, gain(1.0)),
            line(700.0, gain(1.0)),
            line(900.0, gain(1.0)),
            line(1100.0, gain(1.0)),
        ],
        Matrix::Reflection(gains(&[0.500_000_1, 0.5, 0.5, 0.5])),
        &[1.0; 4],
        &[1.0; 4],
    ))]);
    assert!(
        nearly
            .stages
            .iter()
            .any(|s| matches!(s, Stage::Fdn(f) if f.is_lossless()))
    );
    assert_eq!(nearly.spectral_radius(), 1.0);
    // Stable, though the default `radius < 1` would refuse it.
    assert!(frozen.is_stable());

    let mut channels = Inline::new();
    channels
        .push(AllPass {
            delay: 100.0,
            gain: 0.5,
            ..AllPass::default()
        })
        .unwrap();
    let ap = network(vec![Stage::AllPass(channels)]);
    assert!((ap.spectral_radius() - libm::pow(0.5, 0.01)).abs() < 1e-12);

    let taps = Taps {
        taps: Inline::from_slice(&[Tap {
            delay: 10.0,
            gain: 0.5,
            channel: 0,
        }])
        .unwrap(),
    };
    assert_eq!(network(vec![Stage::Taps(taps)]).spectral_radius(), 0.0);

    let hostile = network(vec![Stage::Fdn(fdn(
        &[line(0.0, gain(0.5))],
        Matrix::Diagonal,
        &[1.0],
        &[1.0],
    ))]);
    assert!(hostile.spectral_radius().is_infinite());
    assert!(!hostile.is_stable());
}

/// A diagonal loop's l1 norm is `|b c| / (1 - g)` exactly (its impulse
/// response is `b c g^k` at `(k + 1) m`), a bound the measured sum meets, and
/// the Householder bound is above the measured sum of the same lines' impulse
/// response, which is run here from the difference equations.
#[test]
fn the_l1_norm_is_exact_for_a_comb_and_a_bound_for_a_mixing_loop() {
    let comb = network(vec![Stage::Fdn(fdn(
        &[line(7.0, gain(0.8))],
        Matrix::Diagonal,
        &[0.5],
        &[0.25],
    ))]);
    assert!(
        (comb.l1_norm() - 0.125 / 0.2).abs() < 1e-6,
        "{}",
        comb.l1_norm()
    );

    // Three Householder-mixed lines of gain 0.6, run in `f64`.
    let (m, g) = ([5usize, 7, 11], 0.6f64);
    let n = network(vec![Stage::Fdn(fdn(
        &m.map(|m| line(m as f32, gain(g as f32))),
        Matrix::Householder,
        &[1.0, 0.5, 0.25],
        &[0.2, 0.4, 0.8],
    ))]);
    let len = 4000;
    let mut s = vec![[0.0f64; 3]; len];
    let mut h = vec![0.0f64; len];
    for t in 0..len {
        let x = if t == 0 { 1.0 } else { 0.0 };
        let e: Vec<f64> = (0..3)
            .map(|i| if t >= m[i] { g * s[t - m[i]][i] } else { 0.0 })
            .collect();
        let sum: f64 = e.iter().sum::<f64>() * 2.0 / 3.0;
        for i in 0..3 {
            s[t][i] = [1.0, 0.5, 0.25][i] * x + e[i] - sum;
        }
        h[t] = (0..3)
            .map(|i| {
                if t >= m[i] {
                    [0.2, 0.4, 0.8][i] * s[t - m[i]][i]
                } else {
                    0.0
                }
            })
            .sum();
    }
    let measured: f64 = h.iter().map(|x| x.abs()).sum();
    assert!(n.l1_norm().is_finite(), "no bound at all");
    assert!(
        measured <= n.l1_norm(),
        "measured {measured} above the bound {}",
        n.l1_norm()
    );
    assert!(n.hinf_norm() <= n.l1_norm());
}

/// A loop the analysis cannot bound has an infinite l1 norm and a
/// [`Stage::Fir`] has no response: NaN where asked, and radius 0.
#[test]
fn a_fir_has_no_response_and_no_l1_bound() {
    let n = network(vec![Stage::Fir(crate::network::FirSummary {
        length: tutti_types::Samples(100),
        onset: tutti_types::Samples(0),
        latency: tutti_types::Samples(0),
        rt60: [Seconds(1.0); 3],
        energy: 1.0,
    })]);
    assert!(n.l1_norm().is_infinite() && n.hinf_norm().is_infinite());
    assert!(at(&n, 0.3).re.is_nan());
    assert_eq!(n.spectral_radius(), 0.0);
}

/// Taps sum their gains at their delays: `H = sum g e^(-j w d)`, `l1 = sum |g|`;
/// the channels are the taps' channels, and width and wet follow the kernel's
/// mid and side: `(M + w S, M - w S) * wet`.
#[test]
fn taps_width_and_wet_follow_the_kernels_fold() {
    let taps = Taps {
        taps: Inline::from_slice(&[
            Tap {
                delay: 3.0,
                gain: 0.5,
                channel: 0,
            },
            Tap {
                delay: 5.0,
                gain: -0.25,
                channel: 1,
            },
        ])
        .unwrap(),
    };
    let mut n = network(vec![Stage::Taps(taps)]);
    assert_eq!(n.outputs(), 2);
    assert!((n.l1_norm() - 0.5).abs() < 1e-12);
    let w = 0.7;
    let (l, r) = (delay(w, 3.0) * 0.5, delay(w, 5.0) * -0.25);
    let both = |n: &Network| {
        let hz = [Hz((w / std::f64::consts::TAU * RATE.get()) as f32)];
        let (mut a, mut b) = ([ZERO], [ZERO]);
        n.frequency_response(0, &hz, &mut a);
        n.frequency_response(1, &hz, &mut b);
        (a[0], b[0])
    };
    let (a, b) = both(&n);
    assert!(close(a, l) && close(b, r));
    n.width = StereoWidth(0.0);
    n.wet = Db(-6.0);
    let (a, b) = both(&n);
    let (mid, wet) = ((l + r) * 0.5, f64::from(Db(-6.0).to_amplitude().get()));
    assert!(close(a, mid * wet) && close(b, mid * wet));
    // A width above 1 raises the l1 gain by the width.
    n.width = StereoWidth(2.0);
    n.wet = Db(0.0);
    assert!((n.l1_norm() - 1.0).abs() < 1e-12);
}

/// Pre-delay is a delay of `P` frames in front of the stages: `e^(-j w P)`.
#[test]
fn the_pre_delay_is_a_delay() {
    let mut n = network(vec![Stage::Taps(Taps {
        taps: Inline::from_slice(&[Tap {
            delay: 0.0,
            gain: 1.0,
            channel: 0,
        }])
        .unwrap(),
    })]);
    n.pre_delay = tutti_types::Samples(100);
    assert!(close(at(&n, 0.5), delay(0.5, 100.0)));
}

/// Outputs not in the network have no response, and the width is the last stage's.
#[test]
fn an_output_the_network_lacks_has_no_response() {
    let n = network(vec![]);
    assert_eq!((n.inputs(), n.outputs()), (1, 1));
    let mut g = [ZERO];
    n.frequency_response(1, &[Hz(100.0)], &mut g);
    assert!(g[0].re.is_nan());
}
