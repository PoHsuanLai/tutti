use tutti_types::{Amplitude, SampleRate};

use super::super::{
    Absorption, AllPass, AllPassForm, Band, Bounds, Fdn, FirSummary, Gains, Inline, Line, Matrix,
    Modulation, Network, OutputMap, Stage, Tap, Taps, loop_gain,
};
use super::*;
use crate::{Lti as _, Sizes as _};

const RATE: SampleRate = SampleRate(48_000.0);

fn line(length: f32, absorption: Absorption) -> Line {
    Line {
        length,
        modulation: Modulation::NONE,
        absorption,
        diffuser: None,
    }
}

fn fdn(lines: &[Line], matrix: Matrix) -> Fdn {
    Fdn {
        lines: Inline::from_slice(lines).unwrap(),
        matrix,
        input: Gains::filled(1.0, lines.len()).unwrap(),
        output: OutputMap::default(),
    }
}

fn network(stages: &[Stage]) -> Network {
    let mut n = Network::new(RATE);
    for s in stages {
        n.stages.push(s.clone()).unwrap();
    }
    n
}

/// One 1 s line (48 000 frames) set for an RT60 of 2 s, after a 0.1 s
/// pre-delay: the tail to -60 dB is `0.1 + 1 + 2` = 3.1 s, to -30 dB
/// `0.1 + 1 + 1` = 2.1 s, and to -90 dB 4.1 s.
#[test]
fn the_tail_is_pre_delay_transit_and_the_decay_to_the_floor() {
    let g = loop_gain(48_000.0, Seconds(2.0), RATE);
    let mut n = network(&[Stage::Fdn(fdn(
        &[line(48_000.0, Absorption::Gain(g))],
        Matrix::Diagonal,
    ))]);
    n.pre_delay = Samples(4_800);
    let seconds = |floor| n.tail(Db(floor)).seconds().unwrap().get();
    assert!((seconds(-60.0) - 3.1).abs() < 1e-5, "{}", seconds(-60.0));
    assert!((seconds(-30.0) - 2.1).abs() < 1e-5);
    assert!((seconds(-90.0) - 4.1).abs() < 1e-5);
    assert!((n.rt60(Band::Mid).get() - 2.0).abs() < 1e-5);
}

/// A series all-pass rings for its delay times the trips its train needs to
/// fall to the floor: at `g = 0.5` to -60 dB that is `ceil(ln 1e-3 /
/// ln 0.5)` = 10 trips after the first, so 11 delays. It adds no RT60.
#[test]
fn a_series_all_pass_rings_for_its_geometric_train() {
    let ap = AllPass {
        delay: 100.0,
        gain: 0.5,
        modulation: Modulation::NONE,
        form: AllPassForm::Lattice,
    };
    let n = network(&[Stage::AllPass(Inline::from_slice(&[ap, ap]).unwrap())]);
    let t = n.tail(Db(-60.0)).seconds().unwrap().get();
    assert!((t - 1100.0 / 48_000.0).abs() < 1e-7, "{t}");
    assert_eq!(n.rt60(Band::Low).get(), 0.0);
    assert!(n.is_stable() && !n.is_lossless());
}

/// For a diagonal `A` the slowest line governs; for a mixing `A` the
/// loop is all the lines at once, `-3 Σm / (fs Σ log10 g)`, which for lines
/// each set from RT60 `T` is `T`, and for lines set for different RT60s is
/// between them.
#[test]
fn rt60_is_the_slowest_line_for_a_diagonal_and_the_length_weighted_loop_for_a_mixing() {
    let slow = line(
        40_000.0,
        Absorption::Gain(loop_gain(40_000.0, Seconds(3.0), RATE)),
    );
    let fast = line(
        20_000.0,
        Absorption::Gain(loop_gain(20_000.0, Seconds(1.0), RATE)),
    );
    let diagonal = network(&[Stage::Fdn(fdn(&[slow, fast], Matrix::Diagonal))]);
    assert!((diagonal.rt60(Band::High).get() - 3.0).abs() < 1e-4);
    // Mixed: -3 (40000 + 20000) / (48000 (log10 g1 + log10 g2)) with
    // log10 g = -3 m / (T fs): = (m1 + m2) / (m1/T1 + m2/T2) = 60000 / (40000/3 + 20000).
    let mixed = network(&[Stage::Fdn(fdn(&[slow, fast], Matrix::Hadamard))]);
    let want = 60_000.0 / (40_000.0 / 3.0 + 20_000.0);
    assert!(
        (mixed.rt60(Band::High).get() - want).abs() < 1e-4,
        "{:?}",
        mixed.rt60(Band::High)
    );
    let equal =
        [20_000.0, 30_000.0].map(|m| line(m, Absorption::Gain(loop_gain(m, Seconds(2.0), RATE))));
    let same = network(&[Stage::Fdn(fdn(&equal, Matrix::Householder))]);
    assert!((same.rt60(Band::Low).get() - 2.0).abs() < 1e-4);
}

/// The bands of a one-pole at pole 0.5 and gain 0.8: DC `0.8`, a quarter of
/// the rate `0.8 * 0.5 / sqrt(1.25)`, Nyquist `0.8 * 0.5 / 1.5`; explicit
/// bands read straight.
#[test]
fn a_one_pole_has_its_gain_at_dc_a_quarter_rate_and_nyquist() {
    let a = Absorption::OnePole {
        gain: Amplitude(0.8),
        damp: 0.5,
    };
    assert!((a.gain(Band::Low).get() - 0.8).abs() < 1e-7);
    assert!((a.gain(Band::Mid).get() - 0.8 * 0.5 / 1.25f32.sqrt()).abs() < 1e-6);
    assert!((a.gain(Band::High).get() - 0.8 * 0.5 / 1.5).abs() < 1e-6);
    let b = Absorption::Bands {
        low: Amplitude(0.9),
        mid: Amplitude(0.8),
        high: Amplitude(0.7),
        crossover: [tutti_types::Hz(200.0), tutti_types::Hz(2000.0)],
    };
    assert_eq!(Band::ALL.map(|x| b.gain(x)), [0.9, 0.8, 0.7].map(Amplitude));
    let from = Absorption::from_rt60(
        [Seconds(2.0), Seconds(1.0), Seconds(0.5)],
        [tutti_types::Hz(200.0), tutti_types::Hz(2000.0)],
        4800.0,
        RATE,
    );
    let want = [2.0, 1.0, 0.5].map(|t| loop_gain(4800.0, Seconds(t), RATE));
    assert_eq!(Band::ALL.map(|x| from.gain(x)), want);
}

/// A nested all-pass adds its group delay at DC, `D (1 + g) / (1 - g)`, to
/// the line's length: 100 frames at 0.5 add 300, and the RT60 follows the
/// longer loop.
#[test]
fn a_nested_all_pass_lengthens_the_loop_by_its_low_frequency_delay() {
    let ap = AllPass {
        delay: 100.0,
        gain: 0.5,
        modulation: Modulation::NONE,
        form: AllPassForm::Lattice,
    };
    assert_eq!(ap.low_frequency_delay(), 300.0);
    assert_eq!(
        AllPass { gain: 1.0, ..ap }.low_frequency_delay(),
        f32::INFINITY
    );
    let g = 0.9;
    let plain = line(1000.0, Absorption::Gain(Amplitude(g)));
    let nested = Line {
        diffuser: Some(ap),
        ..plain
    };
    assert_eq!(nested.effective_length(), 1300.0);
    let r = |l| {
        network(&[Stage::Fdn(fdn(&[l], Matrix::Diagonal))])
            .rt60(Band::Mid)
            .get()
    };
    assert!((r(nested) / r(plain) - 1.3).abs() < 1e-5);
}

/// Stability: a gain below 1 under an orthogonal matrix is stable; a gain
/// of 1 in one band only is not (it never settles) and its tail is
/// unbounded; a Hadamard of 3 lines, a dense matrix of the wrong size, a
/// non-orthogonal circulant with `g ||A||_F >= 1`, and a negative or NaN
/// gain are not; all-pass gains of 1 or more are not.
#[test]
fn stability_follows_the_gains_and_the_matrix() {
    let stable = |lines: &[Line], m| network(&[Stage::Fdn(fdn(lines, m))]).is_stable();
    let two = |g| {
        [
            line(100.0, Absorption::Gain(Amplitude(g))),
            line(130.0, Absorption::Gain(Amplitude(g))),
        ]
    };
    for m in [Matrix::Diagonal, Matrix::Householder, Matrix::Hadamard] {
        assert!(stable(&two(0.99), m.clone()));
        assert!(!stable(&two(1.01), m));
    }
    let low_only = Absorption::Bands {
        low: Amplitude(1.0),
        mid: Amplitude(0.9),
        high: Amplitude(0.9),
        crossover: [tutti_types::Hz(200.0), tutti_types::Hz(2000.0)],
    };
    let n = network(&[Stage::Fdn(fdn(
        &[line(100.0, low_only), line(130.0, low_only)],
        Matrix::Hadamard,
    ))]);
    assert!(!n.is_stable() && !n.is_lossless());
    assert_eq!(n.tail(Db(-60.0)), Ring::Unbounded);
    let three = [100.0, 130.0, 170.0].map(|m| line(m, Absorption::Gain(Amplitude(0.5))));
    assert!(!stable(&three, Matrix::Hadamard));
    assert!(stable(&three, Matrix::Householder));
    assert!(!stable(
        &two(0.5),
        Matrix::Dense(Inline::from_slice(&[1.0, 0.0, 0.0]).unwrap())
    ));
    // A shift is an orthogonal circulant; [1, 1] is not, and is stable only
    // when g times its Frobenius norm 2 is below 1.
    let shift = Matrix::Circulant(Inline::from_slice(&[0.0, 1.0]).unwrap());
    assert!(stable(&two(0.99), shift));
    let ones = Matrix::Circulant(Inline::from_slice(&[1.0, 1.0]).unwrap());
    assert!(!stable(&two(0.6), ones.clone()));
    assert!(stable(&two(0.4), ones));
    let dense_orthogonal = Matrix::Dense(Inline::from_slice(&[0.0, 1.0, -1.0, 0.0]).unwrap());
    assert!(stable(&two(0.99), dense_orthogonal));
    let dense_big = Matrix::Dense(Inline::from_slice(&[0.0, 2.0, 2.0, 0.0]).unwrap());
    assert!(!stable(&two(0.6), dense_big));
    assert!(!stable(
        &[line(100.0, Absorption::Gain(Amplitude(-0.5)))],
        Matrix::Diagonal
    ));
    assert!(!stable(
        &[line(100.0, Absorption::Gain(Amplitude(f32::NAN)))],
        Matrix::Diagonal
    ));
    let ap = |gain| {
        network(&[Stage::AllPass(
            Inline::from_slice(&[AllPass {
                delay: 10.0,
                gain,
                modulation: Modulation::NONE,
                form: AllPassForm::Lattice,
            }])
            .unwrap(),
        )])
    };
    assert!(ap(0.9).is_stable() && !ap(1.0).is_stable() && !ap(-1.5).is_stable());
}

/// Every gain 1 under an orthogonal matrix is lossless and marginally
/// stable, with an unbounded tail; one band at 0.999 is not.
#[test]
fn a_lossless_loop_is_marginally_stable_with_an_unbounded_tail() {
    let one = Absorption::Gain(Amplitude(1.0));
    let frozen = network(&[Stage::Fdn(fdn(
        &[line(100.0, one), line(130.0, one)],
        Matrix::Hadamard,
    ))]);
    assert!(frozen.is_lossless() && frozen.is_stable());
    assert_eq!(frozen.tail(Db(-60.0)), Ring::Unbounded);
    assert_eq!(frozen.rt60(Band::Mid).to_f32(), f32::INFINITY);
    let almost = Absorption::Bands {
        low: Amplitude(1.0),
        mid: Amplitude(1.0),
        high: Amplitude(0.999),
        crossover: [tutti_types::Hz(200.0), tutti_types::Hz(2000.0)],
    };
    let n = network(&[Stage::Fdn(fdn(
        &[line(100.0, almost), line(130.0, almost)],
        Matrix::Hadamard,
    ))]);
    assert!(!n.is_lossless());
    assert_eq!(n.tail(Db(f32::NAN)), Ring::Unbounded);
    assert!(!network(&[]).is_lossless());
}

/// Modal density is the total delay in seconds. Echo density at 1 kHz: a
/// diagonal of lines 10 and 20 frames long has nothing before 10 ms, 100 per
/// second from then, 150 from 20 ms (the times are a little past the line
/// lengths, so a delay never ties the probe); a Hadamard of the same lines multiplies
/// each trip (mean 15 ms): `2^1 / 0.015` at 10 ms, `2^3 / 0.015` at 45 ms,
/// capped at the rate.
#[test]
fn the_densities_are_the_modes_per_hertz_and_the_echoes_per_second() {
    let slow = SampleRate(1_000.0);
    let ls = [
        line(10.0, Absorption::Gain(Amplitude(0.5))),
        line(20.0, Absorption::Gain(Amplitude(0.5))),
    ];
    let mut diag = Network::new(slow);
    diag.stages
        .push(Stage::Fdn(fdn(&ls, Matrix::Diagonal)))
        .unwrap();
    assert!((diag.modal_density().get() - 0.03).abs() < 1e-7);
    let e = |n: &Network, t: f32| n.echo_density(Seconds::from_f32(t)).get();
    assert_eq!(e(&diag, 0.005), 0.0);
    assert!((e(&diag, 0.0105) - 100.0).abs() < 1e-3);
    assert!((e(&diag, 0.0205) - 150.0).abs() < 1e-3);
    assert!(
        (e(&diag, 5.0) - 150.0).abs() < 1e-3,
        "a diagonal never densifies"
    );
    let mut mixed = Network::new(slow);
    mixed
        .stages
        .push(Stage::Fdn(fdn(&ls, Matrix::Hadamard)))
        .unwrap();
    assert_eq!(e(&mixed, 0.005), 0.0);
    assert!(
        (e(&mixed, 0.0105) - 2.0 / 0.015).abs() < 0.01,
        "{}",
        e(&mixed, 0.0105)
    );
    assert!(
        (e(&mixed, 0.0455) - 8.0 / 0.015).abs() < 0.1,
        "{}",
        e(&mixed, 0.0455)
    );
    assert_eq!(e(&mixed, 5.0), 1_000.0, "capped at one echo per frame");
    let fir = FirSummary {
        length: Samples(500),
        onset: Samples(0),
        latency: Samples(64),
        rt60: [Seconds(1.0); 3],
        energy: 1.0,
    };
    let mut f = Network::new(slow);
    f.stages.push(Stage::Fir(fir)).unwrap();
    assert_eq!(e(&f, 0.1), 1_000.0);
    assert_eq!(e(&f, 0.6), 0.0);
    assert_eq!(f.modal_density().get(), 0.5);
}

/// A FIR reports its summary for `rt60` and its latency; the time-domain
/// stages have none; its length is its tail.
#[test]
fn a_fir_reports_its_summary() {
    let fir = FirSummary {
        length: Samples(24_000),
        onset: Samples(100),
        latency: Samples(256),
        rt60: [Seconds(2.0), Seconds(1.5), Seconds(1.0)],
        energy: 1.0,
    };
    let n = network(&[Stage::Fir(fir)]);
    assert_eq!(Band::ALL.map(|b| n.rt60(b).get()), [2.0, 1.5, 1.0]);
    assert_eq!(n.latency(), Samples(256));
    assert_eq!(n.tail(Db(-60.0)), Ring::Finite(Seconds(0.5)));
    assert_eq!(n.min_delay(), 100.0);
    assert_eq!(network(&[]).latency(), Samples::ZERO);
}

/// The shortest path: a series all-pass has a direct term, a tap or a line
/// its delay, the pre-delay adds; a network with no path has none to find.
#[test]
fn the_shortest_path_decides_the_direct_path() {
    let ap = Stage::AllPass(
        Inline::from_slice(&[AllPass {
            delay: 100.0,
            gain: 0.5,
            modulation: Modulation::NONE,
            form: AllPassForm::Lattice,
        }])
        .unwrap(),
    );
    assert!(network(std::slice::from_ref(&ap)).has_direct_path());
    let comb = Stage::Fdn(fdn(
        &[line(100.0, Absorption::Gain(Amplitude(0.5)))],
        Matrix::Diagonal,
    ));
    let mut n = network(&[comb.clone(), ap]);
    assert_eq!(n.min_delay(), 100.0);
    assert!(!n.has_direct_path());
    n.pre_delay = Samples(20);
    assert_eq!(n.min_delay(), 120.0);
    // A lattice at g = 0 is a plain delay, no direct path; Freeverb's form
    // keeps its -1 at g = 0 and is direct.
    let plain = |form| {
        Stage::AllPass(
            Inline::from_slice(&[AllPass {
                delay: 100.0,
                gain: 0.0,
                modulation: Modulation::NONE,
                form,
            }])
            .unwrap(),
        )
    };
    assert_eq!(network(&[plain(AllPassForm::Lattice)]).min_delay(), 100.0);
    assert!(!network(&[plain(AllPassForm::Lattice)]).has_direct_path());
    assert!(network(&[plain(AllPassForm::Freeverb)]).has_direct_path());
    let tap = |d| {
        Stage::Taps(Taps {
            taps: Inline::from_slice(&[Tap {
                delay: d,
                gain: 1.0,
                channel: 0,
            }])
            .unwrap(),
        })
    };
    assert!(network(&[tap(0.5)]).has_direct_path());
    assert!(!network(&[tap(1.0)]).has_direct_path());
    assert_eq!(network(&[tap(7.0), comb]).min_delay(), 107.0);
    assert_eq!(
        network(&[]).min_delay(),
        0.0,
        "no stages: nothing is added to the pre-delay"
    );
}

/// `Bounds::dominates` needs every figure, and `of` measures them.
#[test]
fn a_bound_dominates_only_when_every_figure_does() {
    let g = Absorption::Gain(Amplitude(0.5));
    let mut f = fdn(
        &[
            line(100.0, g),
            Line {
                modulation: Modulation {
                    depth: 2.0,
                    rate: tutti_types::Hz(1.0),
                },
                ..line(200.0, g)
            },
        ],
        Matrix::Hadamard,
    );
    f.output.rows.push(Gains::filled(1.0, 2).unwrap()).unwrap();
    let mut n = network(&[Stage::Fdn(f)]);
    n.pre_delay = Samples(10);
    let b = Bounds::measure(&n, Db(-60.0));
    assert_eq!((b.lines, b.stages, b.taps, b.outputs), (2, 1, 0, 1));
    assert_eq!((b.line, b.delay, b.pre_delay), (202.0, 302.0, Samples(10)));
    assert!(b.dominates(&b));
    let smaller: [fn(&mut Bounds); 8] = [
        |b| b.lines -= 1,
        |b| b.stages -= 1,
        |b| b.outputs -= 1,
        |b| b.line -= 1.0,
        |b| b.delay -= 1.0,
        |b| b.pre_delay = Samples(b.pre_delay.get() - 1),
        |b| b.tail = Ring::Finite(Seconds(0.0)),
        |b| b.ir = Samples::ZERO,
    ];
    let mut with_taps_and_ir = b;
    with_taps_and_ir.taps = 1;
    with_taps_and_ir.ir = Samples(5);
    for (i, shrink) in smaller.iter().enumerate() {
        let mut s = if i == 7 { with_taps_and_ir } else { b };
        shrink(&mut s);
        let (big, small) = if i == 7 {
            (&with_taps_and_ir, &s)
        } else {
            (&b, &s)
        };
        assert!(big.dominates(small), "row {i} the other way round");
        assert!(!small.dominates(big), "row {i}");
    }
    assert!(!b.dominates(&with_taps_and_ir), "taps");
}

fn one_pole(gain: f32, damp: f32) -> Absorption {
    Absorption::OnePole {
        gain: Amplitude(gain),
        damp,
    }
}

/// A one-pole whose pole is at or past the unit circle (or not a number)
/// has no response and is not stable, whatever its DC gain: the three-point
/// read of `|H|` alone would call `damp = 2.0` fine. A pole in
/// `(-1, 0)` is valid but peaks above its gain at Nyquist, which counts.
#[test]
fn a_one_pole_outside_the_unit_circle_is_not_stable() {
    let stable = |a| {
        network(&[Stage::Fdn(fdn(
            &[line(100.0, a), line(130.0, a)],
            Matrix::Hadamard,
        ))])
        .is_stable()
    };
    for damp in [
        2.0,
        -2.0,
        1.0,
        -1.0,
        f32::NAN,
        f32::INFINITY,
        f32::NEG_INFINITY,
    ] {
        let a = one_pole(0.9, damp);
        for band in Band::ALL {
            assert_eq!(a.gain(band).get(), f32::INFINITY, "damp {damp}");
        }
        assert!(!stable(a), "damp {damp}");
        let f = fdn(&[line(100.0, a)], Matrix::Diagonal);
        assert_eq!(f.rt60_bound(Band::Mid, RATE.get()).to_f32(), f32::INFINITY);
        let n = network(&[Stage::Fdn(f)]);
        assert_eq!(n.tail(Db(-60.0)), Ring::Unbounded);
    }
    // A valid low-pass pole is stable; a valid negative pole at gain 0.9
    // reads 0.9 * 1.5 / 0.5 = 2.7 at Nyquist and is not.
    assert!(stable(one_pole(0.9, 0.5)));
    let high = one_pole(0.9, -0.5).gain(Band::High).get();
    assert!((high - 2.7).abs() < 1e-5, "{high}");
    assert!(!stable(one_pole(0.9, -0.5)));
}

/// A line must have a finite, positive length: a delay-free loop (length 0),
/// a negative or non-finite one, or a non-finite modulation depth is not
/// stable and not lossless, even at a gain of 1 under an orthogonal matrix.
#[test]
fn a_line_without_a_finite_positive_length_is_not_stable() {
    let mut modulated = line(100.0, Absorption::Gain(Amplitude(0.5)));
    modulated.modulation.depth = f32::NAN;
    let mut lines = vec![modulated];
    for length in [0.0, -5.0, f32::NAN, f32::INFINITY] {
        lines.push(line(length, Absorption::Gain(Amplitude(0.5))));
    }
    for l in lines {
        for gain in [0.5, 1.0] {
            let l = Line {
                absorption: Absorption::Gain(Amplitude(gain)),
                ..l
            };
            let f = fdn(
                &[l, line(130.0, Absorption::Gain(Amplitude(gain)))],
                Matrix::Hadamard,
            );
            assert!(!f.is_stable() && !f.is_lossless(), "{l:?} at {gain}");
        }
    }
    let ok = fdn(
        &[line(1.0, Absorption::Gain(Amplitude(0.5)))],
        Matrix::Diagonal,
    );
    assert!(ok.is_stable(), "one frame is a loop");
}

/// The orthogonality tolerance is sound. A circulant `1.0000045 I` of 16
/// lines has `A Aᵀ - I` entries of 9e-6: loose enough to pass for
/// orthogonal at a tolerance of 1e-5, which would let a gain of `1 - 2e-6`
/// (not lossless) through although the loop gain is `1 + 2.5e-6`. At 1e-6 it
/// is not orthogonal, the Frobenius bound (`4 g` here) applies, and it is
/// refused. The bound folded into the test is a bound: for a nearly
/// orthogonal matrix it is at least the true norm, and exactly 1 for an
/// exactly orthogonal one.
#[test]
fn the_orthogonality_tolerance_cannot_let_a_growing_loop_through() {
    let scaled = |c0: f32, n: usize| {
        let mut row = Gains::filled(0.0, n).unwrap();
        row.as_mut_slice()[0] = c0;
        let lines: Vec<Line> = (0..n)
            .map(|i| line(100.0 + i as f32, Absorption::Gain(Amplitude(1.0 - 2e-6))))
            .collect();
        fdn(&lines, Matrix::Circulant(row))
    };
    let loose = scaled(1.000_004_5, 16);
    assert!(!loose.orthogonal());
    assert!(
        !loose.is_stable(),
        "g = 1 - 2e-6 times norm 1.0000045 grows"
    );
    let exact = scaled(1.0, 64);
    assert!(exact.orthogonal() && exact.is_stable());
    assert_eq!(exact.norm_bound(), 1.0);
    // Nearly orthogonal: c = [1 + e/2, e/2] has norm a + b = 1 + e exactly.
    let eta = 9e-7f32;
    let near = fdn(
        &[
            line(100.0, Absorption::Gain(Amplitude(0.5))),
            line(130.0, Absorption::Gain(Amplitude(0.5))),
        ],
        Matrix::Circulant(Inline::from_slice(&[1.0 + eta / 2.0, eta / 2.0]).unwrap()),
    );
    let Matrix::Circulant(c) = &near.matrix else {
        unreachable!()
    };
    let truth = f64::from(c[0]) + f64::from(c[1]);
    assert!(near.orthogonal());
    assert!(
        near.norm_bound() >= truth,
        "{} < {truth}",
        near.norm_bound()
    );
    assert!(near.norm_bound() > 1.0);
}

/// For a mixing matrix with unequal lines the length-weighted RT60 is a
/// mean and undercuts the true decay, and the tail must not. Two lines of
/// 40 and 70 frames at 1 kHz, set for RT60 3 s and 1 s under a Hadamard
/// matrix: the estimate is `110 / (40/3 + 70)` = 1.32 s, the slowest line's
/// bound 3 s, and the state of the equations (simulated here, in `f64`)
/// last exceeds 1e-3 of its start at about 1.9 s: after the estimate, inside
/// the tail. For a diagonal matrix the bound equals the exact RT60.
#[test]
fn the_tail_covers_a_mixing_network_whose_lines_decay_differently() {
    let rate = SampleRate(1_000.0);
    let lines = [(40.0f32, 3.0f32), (70.0, 1.0)].map(|(m, t)| {
        line(
            m,
            Absorption::Gain(loop_gain(m, Seconds::from_f32(t), rate)),
        )
    });
    let mut mixed = Network::new(rate);
    mixed
        .stages
        .push(Stage::Fdn(fdn(&lines, Matrix::Hadamard)))
        .unwrap();
    let estimate = mixed.rt60(Band::Mid).to_f32();
    assert!((estimate - 1.32).abs() < 1e-3, "{estimate}");
    let bound = fdn(&lines, Matrix::Hadamard).rt60_bound(Band::Mid, rate.get());
    assert!((bound.get() - 3.0).abs() < 1e-3);
    let Ring::Finite(tail) = mixed.tail(Db(-60.0)) else {
        panic!("finite")
    };
    assert!((tail.get() - (0.07 + 3.0)).abs() < 1e-3, "{tail:?}");

    // The equations, run: Hadamard (1/sqrt 2 [[1, 1], [1, -1]]) over two
    // shift registers, gain at the exit, an impulse in the first line.
    let g: Vec<f64> = lines
        .iter()
        .map(|l| f64::from(l.absorption.gain(Band::Mid).get()))
        .collect();
    let mut regs = [vec![0.0f64; 40], vec![0.0f64; 70]];
    regs[0][0] = 1.0;
    let energy = |r: &[Vec<f64>; 2]| r.iter().flatten().map(|x| x * x).sum::<f64>();
    let start = energy(&regs);
    let mut last_above = 0usize;
    for t in 1..20_000usize {
        let y = [regs[0].pop().unwrap() * g[0], regs[1].pop().unwrap() * g[1]];
        let h = std::f64::consts::FRAC_1_SQRT_2;
        regs[0].insert(0, h * (y[0] + y[1]));
        regs[1].insert(0, h * (y[0] - y[1]));
        if (energy(&regs) / start).sqrt() > 1e-3 {
            last_above = t;
        }
    }
    let actual = last_above as f32 / 1_000.0;
    assert!(
        actual > estimate,
        "the estimate {estimate} s undercuts the {actual} s the state needs"
    );
    assert!(
        actual <= tail.to_f32(),
        "the tail {tail:?} must cover {actual} s"
    );

    let mut diagonal = Network::new(rate);
    diagonal
        .stages
        .push(Stage::Fdn(fdn(&lines, Matrix::Diagonal)))
        .unwrap();
    let exact = diagonal.rt60(Band::Mid).get();
    let bound = fdn(&lines, Matrix::Diagonal).rt60_bound(Band::Mid, rate.get());
    assert!((bound.get() - exact).abs() < 1e-4);
}

/// A measured-orthogonal matrix has a norm bound just above 1, and the
/// lag of the proof grows by `(m-1) log σ / -log(σρ)` frames; a structured
/// one has none.
#[test]
fn a_nearly_orthogonal_matrix_adds_a_lag_a_structured_one_does_not() {
    let g = Absorption::Gain(Amplitude(0.9));
    let lines = [line(100.0, g), line(130.0, g)];
    let structured = fdn(&lines, Matrix::Householder);
    assert_eq!(structured.lag_extra_frames(Band::Mid), 0.0);
    let c = Matrix::Circulant(Inline::from_slice(&[1.000_000_2, 0.0]).unwrap());
    let near = fdn(&lines, c);
    assert!(near.orthogonal() && near.lag_extra_frames(Band::Mid) > 0.0);
    assert!(
        near.rt60_bound(Band::Mid, 48_000.0).get()
            > structured.rt60_bound(Band::Mid, 48_000.0).get()
    );
}

/// A nested all-pass must be a lattice with `|g| < 1` for the loop analysis:
/// a Freeverb-form one (not unit magnitude) or `|g| >= 1` makes the line
/// unstable and not lossless, and a lattice adds its group delay without
/// loss.
#[test]
fn only_a_lattice_diffuser_is_inside_the_analysis() {
    let nested = |form, gain| Line {
        diffuser: Some(AllPass {
            delay: 20.0,
            gain,
            modulation: Modulation::NONE,
            form,
        }),
        ..line(100.0, Absorption::Gain(Amplitude(1.0)))
    };
    let f = |l| {
        fdn(
            &[l, line(130.0, Absorption::Gain(Amplitude(1.0)))],
            Matrix::Hadamard,
        )
    };
    assert!(f(nested(AllPassForm::Lattice, 0.5)).is_lossless());
    assert!(f(nested(AllPassForm::Lattice, 0.5)).is_stable());
    for l in [
        nested(AllPassForm::Freeverb, 0.5),
        nested(AllPassForm::Lattice, 1.0),
    ] {
        assert!(!f(l).is_stable() && !f(l).is_lossless(), "{l:?}");
    }
}

/// A reflection `I - 2 v v^T` of a unit `v` is orthogonal (so a lossless loop
/// can use it), of a shorter `v` a contraction (`|1 - 2|v|^2| <= 1`, never a
/// gain), and of a longer one a gain `2|v|^2 - 1` the stability test refuses
/// at a loop gain of 1; a `v` of the wrong length fits no line count. The
/// norm is exact, not the generic bound.
#[test]
fn a_reflection_is_orthogonal_when_its_vector_is_a_unit() {
    let lines = [100.0, 130.0, 170.0, 190.0].map(|m| line(m, Absorption::Gain(Amplitude(1.0))));
    let reflect = |v: &[f32]| {
        network(&[Stage::Fdn(fdn(
            &lines,
            Matrix::Reflection(Inline::from_slice(v).unwrap()),
        ))])
    };
    let unit = reflect(&[0.5, 0.5, 0.5, 0.5]);
    assert!(unit.is_lossless() && unit.is_stable());
    // A unit vector off the flat one is just as orthogonal.
    let slanted = reflect(&[0.6, 0.8, 0.0, 0.0]);
    assert!(slanted.is_lossless());
    // Half the length: the eigenvalue along v is 1 - 2 * 0.25 = 0.5, so the
    // matrix is a contraction, not orthogonal: stable at gain 1 is refused
    // for lack of loss only, not for growth.
    let short = reflect(&[0.25, 0.25, 0.25, 0.25]);
    assert!(!short.is_lossless());
    // Longer than a unit: the eigenvalue 1 - 2 * 1.25 = -1.5 is a gain of 1.5.
    let long = reflect(&[1.0, 0.5, 0.0, 0.0]);
    assert!(!long.is_lossless() && !long.is_stable());
    let Stage::Fdn(f) = &long.stages[0] else {
        unreachable!()
    };
    assert_eq!(f.norm_bound(), 1.5);
    // A unit vector that does not round exactly is a gain of 1 to within the
    // rounding of its entries, not the generic bound's `n` times that.
    let Stage::Fdn(f) = &slanted.stages[0] else {
        unreachable!()
    };
    assert!(f.norm_bound() < 1.0 + 1.0e-7, "{}", f.norm_bound());
    let Stage::Fdn(f) = &short.stages[0] else {
        unreachable!()
    };
    assert_eq!(
        f.norm_bound(),
        1.0,
        "a contraction counts as 1 where it is bounded"
    );
    // The wrong length fits no line count.
    let lossy = [100.0, 130.0, 170.0, 190.0].map(|m| line(m, Absorption::Gain(Amplitude(0.9))));
    let wrong = network(&[Stage::Fdn(fdn(
        &lossy,
        Matrix::Reflection(Inline::from_slice(&[0.5, 0.5, 0.5]).unwrap()),
    ))]);
    assert!(!wrong.is_stable());
}
