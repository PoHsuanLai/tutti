use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde::{Deserialize, Serialize};
use tutti_types::{BusTopology, Db, Depth, Feedback, Hz, Phase, PhaseIncrement, SampleRate};

use super::*;
use crate::Realized;
use crate::contract::{FamilyCase, Figure, LtiCase, RecurrenceCase, built, noise, render};

const RATE: SampleRate = SampleRate::SR_48K;

fn controls(rate: f32, depth: f32, feedback: f32) -> PhaserControls {
    PhaserControls {
        rate: Hz(rate),
        depth: Depth::of(depth),
        feedback: Feedback(feedback),
    }
}

fn bus(width: u16) -> BusTopology {
    BusTopology::assumed(width.into()).expect("a test width is one bus")
}

fn step(x: f32) -> Stagger {
    Stagger::of_step(PhaseIncrement(x))
}

fn family(width: u16, stages: usize, range: SweepRange, stagger: Stagger) -> PhaserFamily {
    PhaserFamily::new(bus(width), Stages::of(stages), range, stagger).expect("a stagger that fits")
}

fn range(min: f32, max: f32) -> SweepRange {
    SweepRange::of(Hz(min), Hz(max))
}

// ---- the contract ---------------------------------------------------------

impl FamilyCase for PhaserFamily {
    fn capacity(&self, _: SampleRate) {}

    /// Rates from the floor to audio rate, every depth from inverted to full, and feedbacks
    /// up to the greatest stable.
    fn reachable(&self) -> Vec<PhaserControls> {
        let mut all = Vec::new();
        for rate in [0.01f32, 0.3, 1.0, 5.0, 20.0, 400.0] {
            for depth in [-1.0f32, 0.0, 0.5, 1.0] {
                for fb in [0.0f32, 0.3, 0.9, 0.99] {
                    all.push(controls(rate, depth, fb));
                }
            }
        }
        let mut rng = StdRng::seed_from_u64(14);
        all.extend((0..32).map(|_| {
            controls(
                rng.gen_range(0.01..30.0),
                rng.gen_range(-1.0..=1.0),
                rng.gen_range(0.0..=0.99),
            )
        }));
        all
    }

    /// A rate or feedback that is not finite, negative or far past its range: each a member
    /// the family can run. (A depth is range-checked where a `Depth` is made.)
    fn hostile(&self) -> Vec<PhaserControls> {
        let base = controls(1.0, 0.5, 0.5);
        let mut all = Vec::new();
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -1.0, 0.0, 1e9] {
            all.push(PhaserControls {
                rate: Hz(bad),
                ..base
            });
            all.push(PhaserControls {
                feedback: Feedback(bad),
                ..base
            });
        }
        all
    }
}

impl RecurrenceCase for PhaserFamily {
    /// The sweep **moving**, at a few rates, depths and feedbacks: block independence,
    /// reset and the cases that render run over what the render does.
    fn probes(&self) -> Vec<PhaserControls> {
        vec![
            controls(0.5, 0.5, 0.5),
            controls(7.0, 1.0, 0.9),
            controls(0.1, 0.25, 0.0),
        ]
    }

    /// The loop at its greatest gain, swept fast.
    fn extremes(&self) -> Vec<PhaserControls> {
        vec![
            controls(5.0, 1.0, 0.99),
            controls(20.0, 0.5, 0.9),
            controls(0.05, -1.0, 0.99),
        ]
    }

    /// A depth that moves, a rate that moves and a feedback that moves.
    fn steps(&self) -> Vec<(PhaserControls, PhaserControls)> {
        vec![
            (controls(1.0, 0.25, 0.4), controls(1.0, 0.75, 0.4)),
            (controls(1.0, 0.5, 0.4), controls(3.0, 0.5, 0.4)),
            (controls(1.0, 0.5, 0.2), controls(1.0, 0.5, 0.8)),
        ]
    }

    /// The sweep **frozen**: at no depth the all-pass sits at the bottom of the range, so
    /// the model is linear and time invariant and its response is the render's.
    fn static_probes(&self) -> Vec<PhaserControls> {
        vec![
            controls(1.0, 0.0, 0.5),
            controls(5.0, 0.0, 0.9),
            controls(0.2, 0.0, 0.99),
            controls(1.0, 0.0, 0.0),
        ]
    }
}

/// How much of `f32::EPSILON / (1 - r)` the phaser's render differs from its model by, fitted
/// once on the most marginal design (12 stages parked at a 20 Hz bottom, feedback 0.99,
/// slowest at 96 kHz): 0.15 passes its `response` case and 0.1 fails it, so 0.25 holds it with
/// a margin of about two. It absorbs what the first-order figure leaves out: the roundoff
/// accumulating over the chain's stages, and the window the contract's comparison is taken
/// over.
const ROUNDOFF_GAIN: f64 = 0.25;

impl LtiCase for PhaserFamily {
    /// `max(1e-3, ROUNDOFF_GAIN * f32::EPSILON / (1 - r))`, with `r` the spectral radius of
    /// the design's slowest frozen probe (over the contract's rates): the render's `f32`
    /// against the model's `f64`. The model and the render share the coefficient, rounded to
    /// `f32`, so what differs is the `f32` roundoff of the loop's state, which a loop of radius
    /// `r` amplifies by about `1 / (1 - r)`. A design whose loop is far from the circle is held
    /// to 1e-3; the 12-stage chain parked at a 20 Hz bottom with feedback 0.99 (`r` within
    /// about 1e-6 of 1, some 10^6 frames of ringing) gets a few percent, where 3.3e-2 was
    /// measured at 48 kHz. A sign flipped in the loop is off by order one at every design.
    fn response_tolerance(&self) -> f64 {
        let slowest = self
            .rates()
            .into_iter()
            .flat_map(|rate| {
                self.static_probes()
                    .into_iter()
                    .map(move |c| self.member(&c, rate).spectral_radius())
            })
            .fold(0.0f64, f64::max);
        (ROUNDOFF_GAIN * f64::from(f32::EPSILON) / (1.0 - slowest)).max(1e-3)
    }

    /// The closed forms of the chain at its fixed points: `A = 1` at DC and `(-1)^N` at
    /// Nyquist (for any coefficient), so `|H| = 1 / (1 - g)` at DC and
    /// `1 / (1 + g (-1)^N)` at Nyquist; and flat at no feedback, where `|A| = 1` everywhere.
    fn figures(&self) -> Vec<Figure<PhaserControls>> {
        let n = self.stages().get();
        let mut all = Vec::new();
        for rate in [SampleRate::SR_48K, SampleRate::SR_96K] {
            for g in [0.0f32, 0.5, 0.9] {
                let c = controls(1.0, 0.0, g);
                let q = f64::from(g);
                let nyquist = if n.is_multiple_of(2) {
                    1.0 / (1.0 + q)
                } else {
                    1.0 / (1.0 - q)
                };
                for (freq, gain) in [(0.0, 1.0 / (1.0 - q)), (rate.get() / 2.0, nyquist)] {
                    all.push(Figure {
                        controls: c,
                        rate,
                        output: 0,
                        freq: Hz(freq as f32),
                        gain,
                        tolerance: 1e-6,
                    });
                }
            }
            all.push(Figure {
                controls: controls(1.0, 0.0, 0.0),
                rate,
                output: 0,
                freq: Hz(1_234.0),
                gain: 1.0,
                tolerance: 1e-9,
            });
        }
        all
    }
}

/// The contract of one design: the family's cases with the `Lti` group, and the recurrence's
/// with `Lti` and `BlockIndependent`, like every other `Lti` family. The phaser's tail is
/// [`Ring::Unbounded`] by design (the closed loop's poles have no closed form to cut a tail
/// at), so the contract reads that from the member: the cases that measure a finite tail do
/// not apply to it, and `response` renders for as long as the member's spectral radius says.
macro_rules! phaser_contract {
    () => {
        crate::family_contract!(design(), Lti);
        crate::recurrence_contract!(design(), Lti, BlockIndependent);
    };
}

mod mono_four {
    use super::*;
    fn design() -> PhaserFamily {
        family(1, 4, SweepRange::DEFAULT, Stagger::NONE)
    }
    phaser_contract!();
}

/// Two channels a quarter cycle apart on a narrow, high range: the diagonal case.
mod stereo_staggered {
    use super::*;
    fn design() -> PhaserFamily {
        family(2, 6, range(300.0, 3_000.0), step(0.25))
    }
    phaser_contract!();
}

/// Six channels, three stages (odd: a notch at Nyquist), each its own sweep.
mod six_odd {
    use super::*;
    fn design() -> PhaserFamily {
        family(6, 3, SweepRange::DEFAULT, step(0.1))
    }
    phaser_contract!();
}

/// The longest chain, on a range from the lowest bottom up past the cap at 44.1 kHz (0.9 of
/// Nyquist is 19.8 kHz there).
mod twelve_wide_range {
    use super::*;
    fn design() -> PhaserFamily {
        family(2, 12, range(20.0, 40_000.0), step(0.5))
    }
    phaser_contract!();
}

/// The shortest chain on the lowest fixed range: the family's cases alone.
mod two_lowest {
    use super::*;
    fn design() -> PhaserFamily {
        family(1, 2, range(20.0, 20.0), Stagger::NONE)
    }
    crate::family_contract!(design(), Lti);
}

// ---- the figures of the definition ---------------------------------------

/// The coefficient of the textbook all-pass `(c + z^-1) / (1 + c z^-1)` tuned to `hz`:
/// `tan` of the normalised centre, folded into the all-pass form.
fn textbook_c(hz: f64, sr: f64) -> f64 {
    let t = (std::f64::consts::PI * hz / sr).tan();
    (t - 1.0) / (t + 1.0)
}

/// The phase of one stage at `hz`, from the textbook `H` on the unit circle.
fn stage_phase(c: f64, hz: f64, sr: f64) -> f64 {
    let w = std::f64::consts::TAU * hz / sr;
    let (zr, zi) = (w.cos(), -w.sin()); // z^-1 on the unit circle
    let (nr, ni) = (c + zr, zi);
    let (dr, di) = (1.0 + c * zr, c * zi);
    (ni * dr - nr * di).atan2(nr * dr + ni * di)
}

/// At no feedback the frozen chain is flat and its phase is `N` times a stage's, from the
/// textbook transfer function and a coefficient solved here from the centre. The centre of a
/// depth-zero member is the bottom of the range, at any LFO position.
#[test]
fn a_frozen_chain_is_flat_with_n_times_the_stage_phase_at_no_feedback() {
    for stages in [2usize, 3, 4, 7, 12] {
        let f = family(1, stages, range(1_000.0, 4_000.0), Stagger::NONE);
        let m = f.member(&controls(0.5, 0.0, 0.0), RATE);
        assert_eq!(m.centre_hz(), Hz(1_000.0));
        let c = textbook_c(1_000.0, RATE.get());
        let freqs: Vec<Hz> = [30.0f32, 250.0, 1_000.0, 3_000.0, 11_000.0, 23_000.0]
            .into_iter()
            .map(Hz)
            .collect();
        let mut got = vec![Complex64::new(0.0, 0.0); freqs.len()];
        m.frequency_response(0, &freqs, &mut got);
        for (h, g) in freqs.iter().zip(&got) {
            let phase = stages as f64 * stage_phase(c, f64::from(h.get()), RATE.get());
            assert!(
                (g.norm() - 1.0).abs() < 1e-6,
                "{stages} stages at {h:?}: |H| = {}",
                g.norm()
            );
            // Compare on the circle: the phase wraps.
            let want = Complex64::from_polar(1.0, phase);
            assert!(
                (g - want).norm() < 1e-4,
                "{stages} stages at {h:?}: H = {g}, want {want}"
            );
        }
    }
}

/// At the centre a stage is a quarter turn, so two stages invert a sine: the wet and the dry
/// of a half mix cancel there. The model says `H(f0) = -1`, and the render does it.
#[test]
fn two_stages_invert_at_their_centre() {
    let f = family(1, 2, range(1_000.0, 2_000.0), Stagger::NONE);
    let m = f.member(&controls(0.5, 0.0, 0.0), RATE);
    let mut g = [Complex64::new(0.0, 0.0)];
    m.frequency_response(0, &[Hz(1_000.0)], &mut g);
    assert!((g[0] + 1.0).norm() < 1e-4, "H(f0) = {}", g[0]);
    let (mut k, _) = built(&f, &controls(0.5, 0.0, 0.0), RATE);
    let x: Vec<f32> = (0..6_000)
        .map(|n| (std::f64::consts::TAU * 1_000.0 * n as f64 / RATE.get()).sin() as f32)
        .collect();
    let y = render(&mut k, crate::contract::widths(&f), &x, 64).remove(0);
    let worst = (5_000..6_000)
        .map(|n| (y[n] + x[n]).abs())
        .fold(0.0, f32::max);
    assert!(worst < 2e-3, "wet is not the inverted input: {worst}");
}

/// The notches of a half mix are where the chain's phase is an odd multiple of `-pi`: a
/// chain of `N` stages crosses `-pi, -3 pi, ..` on the way from DC to Nyquist, so it has
/// `ceil(N / 2)` of them, an odd chain's last at Nyquist, each found here by bisection on
/// the textbook phase, and the member's response is `-1` there (wet and dry cancel).
#[test]
fn a_half_mix_of_the_chain_has_ceil_n_over_two_notches_where_the_phase_is_odd_pi() {
    let sr = RATE.get();
    for stages in 2usize..=12 {
        let f = family(1, stages, range(700.0, 700.0), Stagger::NONE);
        let m = f.member(&controls(0.5, 0.0, 0.0), RATE);
        let c = textbook_c(700.0, sr);
        // The chain's phase, unwrapped from 0 at DC to `-N pi` at Nyquist: a stage's alone
        // is monotone, so the sum of `N` equal ones is too.
        let total = |hz: f64| {
            let mut p = stage_phase(c, hz, sr);
            if p > 1e-9 {
                p -= std::f64::consts::TAU;
            }
            stages as f64 * p
        };
        let mut notches = Vec::new();
        let mut k = 0;
        while 2 * k < stages {
            let target = -((2 * k + 1) as f64) * std::f64::consts::PI;
            let (mut lo, mut hi) = (0.0, sr / 2.0);
            for _ in 0..80 {
                let mid = 0.5 * (lo + hi);
                if total(mid) > target {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
            notches.push(0.5 * (lo + hi));
            k += 1;
        }
        assert_eq!(notches.len(), stages.div_ceil(2), "{stages} stages");
        if stages % 2 == 1 {
            let last = *notches.last().unwrap();
            assert!((last - sr / 2.0).abs() < 1e-3, "{stages}: Nyquist, {last}");
        }
        for hz in notches {
            let mut g = [Complex64::new(0.0, 0.0)];
            m.frequency_response(0, &[Hz(hz as f32)], &mut g);
            let mix = (g[0] + 1.0) * 0.5;
            assert!(
                mix.norm() < 2e-3,
                "{stages} stages: the half mix at {hz} Hz is {}",
                mix.norm()
            );
        }
    }
}

/// The roots `w_k` of `(1 + c w)^N - g w (c + w)^N`, the closed loop's characteristic
/// polynomial in `w = z^-1`, by Durand-Kerner: the poles are `1 / w_k`.
fn closed_loop_poles(c: f64, g: f64, n: usize) -> Vec<Complex64> {
    // `(1 + c w)^N - g w (c + w)^N`, degree `N + 1`, its leading coefficient `-g`. It is
    // evaluated in this factored form, not from expanded coefficients: near `c = +-1` the
    // `N` roots crowd around `-1/c`, and the binomial expansion cancels there.
    let degree = n + 1;
    let one = Complex64::new(1.0, 0.0);
    let eval = |w: Complex64| -> Complex64 {
        ((one + w * c).powu(n as u32) - w * g * (w + c).powu(n as u32)) / -g
    };
    let mut roots: Vec<Complex64> = (0..degree)
        .map(|i| Complex64::from_polar(1.5, 0.4 + std::f64::consts::TAU * i as f64 / degree as f64))
        .collect();
    for _ in 0..500 {
        for i in 0..degree {
            let others = roots
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .fold(Complex64::new(1.0, 0.0), |a, (_, r)| a * (roots[i] - r));
            let here = roots[i];
            roots[i] = here - eval(here) / others;
        }
    }
    roots
}

/// The spectral radius is a bound the closed loop's poles respect: it is under 1 for every
/// stable feedback, no pole of `1 - g z^-1 A(z)` lies outside it (the roots are found here
/// from the characteristic polynomial, not from the member), and it is exact where the
/// closed form is: the `N` poles of the chain are at `-c` at no feedback, and a chain
/// tuned to a quarter of the rate (`c = 0`) has poles at `g^(1/(N+1))`.
#[test]
fn the_spectral_radius_bounds_the_closed_loop_poles_and_is_exact_at_the_closed_forms() {
    let sr = RATE.get();
    for stages in [2usize, 3, 5, 8, 12] {
        for hz in [
            50.0f32, 200.0, 1_500.0, 6_000.0, 9_000.0, 15_000.0, 19_000.0,
        ] {
            for g in [0.0f32, 0.1, 0.3, 0.5, 0.8, 0.95, 0.99] {
                let f = family(1, stages, range(hz, hz), Stagger::NONE);
                let m = f.member(&controls(1.0, 0.0, g), RATE);
                let bound = m.spectral_radius();
                assert!(bound < 1.0, "{stages} stages at {hz} Hz, g {g}: {bound}");
                if g == 0.0 {
                    // The loop is open: the characteristic polynomial has lost its degree.
                    continue;
                }
                let c = textbook_c(f64::from(hz), sr);
                for w in closed_loop_poles(c, f64::from(g), stages) {
                    let pole = 1.0 / w.norm();
                    assert!(
                        pole <= bound * (1.0 + 1e-6),
                        "{stages} stages at {hz} Hz, g {g}: a pole at {pole} outside {bound}"
                    );
                }
            }
        }
        // No feedback: the poles are `-c`.
        let f = family(1, stages, range(1_000.0, 1_000.0), Stagger::NONE);
        let m = f.member(&controls(1.0, 0.0, 0.0), RATE);
        let c = textbook_c(1_000.0, sr).abs();
        assert!((m.spectral_radius() - c).abs() < 1e-6);
        // `c = 0` at a quarter of the rate: the loop is `1 - g z^-(N+1)`.
        let f = family(1, stages, range(12_000.0, 12_000.0), Stagger::NONE);
        let m = f.member(&controls(1.0, 0.0, 0.8), RATE);
        let exact = 0.8f64.powf(1.0 / (stages + 1) as f64);
        assert!(
            (m.spectral_radius() - exact).abs() < 1e-5,
            "{stages}: {} against {exact}",
            m.spectral_radius()
        );
    }
}

/// The peak of the response is at DC for a feedback above zero, where `A = 1`: `1 / (1 - g)`,
/// the figure `hinf_norm` states, which `|A| = 1` on the circle bounds everywhere.
#[test]
fn the_response_never_exceeds_one_over_one_minus_the_feedback() {
    for g in [0.0f32, 0.5, 0.9, 0.99] {
        let f = family(1, 6, range(200.0, 200.0), Stagger::NONE);
        let m = f.member(&controls(1.0, 0.0, g), RATE);
        let freqs: Vec<Hz> = (0..=2_000).map(|k| Hz(k as f32 * 12.0)).collect();
        let mut h = vec![Complex64::new(0.0, 0.0); freqs.len()];
        m.frequency_response(0, &freqs, &mut h);
        let peak = h.iter().map(|x| x.norm()).fold(0.0, f64::max);
        let want = 1.0 / (1.0 - f64::from(g));
        assert!((peak - want).abs() < 1e-9 * want, "g {g}: {peak} vs {want}");
        assert_eq!(m.hinf_norm(), want);
    }
}

// ---- the controls ----------------------------------------------------------

/// A control is clamped into its range, and one that is not finite reads as the default's:
/// feedback into `0..=MAX_STABLE`, and the depth into `-1..=1`.
#[test]
fn a_control_out_of_range_is_clamped_and_one_not_finite_is_the_defaults() {
    let f = family(1, 4, SweepRange::DEFAULT, Stagger::NONE);
    let m = |c: PhaserControls| f.member(&c, RATE);
    assert_eq!(m(controls(1.0, 0.5, 5.0)).feedback(), Feedback::MAX_STABLE);
    assert_eq!(m(controls(1.0, 0.5, -0.5)).feedback(), Feedback::NONE);
    let d = PhaserControls::DEFAULT;
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let fb = PhaserControls {
            feedback: Feedback(bad),
            ..controls(2.0, 0.2, 0.0)
        };
        assert_eq!(m(fb).feedback(), d.feedback, "{bad}");
    }
    assert_eq!(m(controls(1.0, 1.0, 0.0)).depth(), Depth::FULL);
    assert_eq!(m(controls(1.0, -1.0, 0.0)).depth(), Depth::INVERTED);
}

/// The rate is **floored at 0.01 Hz**, a cell's value and a lane's alike: a rate of -3 Hz, of
/// zero and of 0.005 Hz is the 0.01 Hz sweep, a rate that is not a number is the default's, whose LFO phase is the f32 walk of
/// `0.01 / sr` a frame (walked here per frame, not read off the member), and a rate above the
/// floor is not.
#[test]
fn a_rate_below_the_floor_is_the_floor() {
    let f = family(1, 4, SweepRange::DEFAULT, Stagger::NONE);
    let inc = |hz: f32| PhaseIncrement::per_sample(Hz(hz), RATE);
    let walked = |hz: f32, frames: usize| {
        let mut p = Phase::START;
        for _ in 0..frames {
            p = p.advance(inc(hz));
        }
        p
    };
    // 4801 frames solve the control points at 0, 16, .., 4800: 301 segments of 16, the phase
    // at the end of the last.
    // A rate that is not a number is the default's.
    let default = PhaserControls::DEFAULT.rate.get();
    for (rate, effective) in [
        (-3.0f32, 0.01),
        (0.0, 0.01),
        (0.005, 0.01),
        (0.01, 0.01),
        (f32::NAN, default),
        (f32::INFINITY, default),
        (f32::NEG_INFINITY, default),
    ] {
        let (mut k, _) = built(&f, &controls(rate, 1.0, 0.5), RATE);
        render(
            &mut k,
            crate::contract::widths(&f),
            &noise(1, 4_801, 1.0),
            64,
        );
        let (_, _, state) = k.parts_mut();
        assert_eq!(state.phase, walked(effective, 4_800 + 16), "rate {rate}");
    }
    let (mut k, _) = built(&f, &controls(0.5, 1.0, 0.5), RATE);
    render(
        &mut k,
        crate::contract::widths(&f),
        &noise(1, 4_801, 1.0),
        64,
    );
    let (_, _, state) = k.parts_mut();
    assert_eq!(state.phase, walked(0.5, 4_800 + 16));
    assert_ne!(walked(0.5, 4_816), walked(0.01, 4_816));
}

/// The sweep's top is held at 0.90 of Nyquist at the rate, and the authored top is kept: a
/// low rate caps it, a higher rate after it restores it (a cap that ratcheted the top down
/// for good would stay at 3.6 kHz).
#[test]
fn the_range_is_capped_at_0_9_nyquist_and_a_rising_rate_restores_the_authored_top() {
    let f = family(1, 4, SweepRange::DEFAULT, Stagger::NONE);
    let top = |f: &PhaserFamily, sr: f64| {
        f.member(&controls(1.0, 1.0, 0.5), SampleRate::of(sr))
            .max_hz
    };
    assert_eq!(top(&f, 8_000.0), Hz(3_600.0), "0.90 of a 4 kHz Nyquist");
    assert_eq!(top(&f, 48_000.0), Hz(4_000.0));
    let wide = family(1, 4, range(300.0, 30_000.0), Stagger::NONE);
    assert_eq!(top(&wide, 48_000.0), Hz(21_600.0));
    assert_eq!(top(&wide, 96_000.0), Hz(30_000.0));
    assert_eq!(
        wide.member(&controls(1.0, 1.0, 0.5), RATE).min_hz,
        Hz(300.0)
    );
}

/// What the render sweeps is the definition: at the control points the coefficient is
/// `allpass_coeff(sweep_hz(..))` of the sine of the LFO's own phase, which accumulates in
/// `f32` one frame at a time: frame 0 (phase 0) is the sweep at its centre, and frame 16 is
/// a little past it, at depth 1.
#[test]
fn the_coefficient_at_a_control_point_is_the_sweep_at_the_lfo_phase() {
    let f = family(1, 4, range(300.0, 3_000.0), Stagger::NONE);
    let c = controls(5.0, 1.0, 0.5);
    let (mut k, m) = built(&f, &c, RATE);
    // One frame: the first segment is solved, `[the coefficient at frame 0, at frame 16]`.
    render(&mut k, crate::contract::widths(&f), &[0.5], 64);
    let inc = PhaseIncrement::per_sample(Hz(5.0), RATE);
    let at = |frame: usize| {
        let mut p = Phase::START;
        for _ in 0..frame {
            p = p.advance(inc);
        }
        let lfo = (std::f64::consts::TAU * f64::from(p.get())).sin() as f32;
        let hz = sweep_hz(m.min_hz, m.max_hz, lfo, Depth::FULL);
        allpass_coeff(hz.get(), RATE.get() as f32)
    };
    let (_, _, state) = k.parts_mut();
    assert!((state.points[0][0] - at(0)).abs() < 1e-6, "at frame 0");
    assert!((state.points[0][1] - at(16)).abs() < 1e-6, "at frame 16");
    assert_ne!(state.points[0][0], state.points[0][1], "the sweep moves");
}

// ---- the stagger and the channels -----------------------------------------

/// Channel `c`'s sweep is `c` times the step ahead (wrapped), and a stagger of a whole
/// cycle is none: from the definition, each channel's coefficient at frame 0 is the sweep
/// at the sine of its own offset.
#[test]
fn each_channel_is_staggered_by_its_step_and_a_full_cycle_is_none() {
    let band = range(300.0, 3_000.0);
    let c = controls(1.0, 1.0, 0.5);
    for (stagger, offsets) in [
        (step(0.25), [0.0f64, 0.25, 0.5]),
        (step(1.0), [0.0, 0.0, 0.0]),
        (step(1.25), [0.0, 0.25, 0.5]),
    ] {
        let f = PhaserFamily::new(bus(3), Stages::of(4), band, stagger).unwrap();
        let (mut k, m) = built(&f, &c, RATE);
        render(&mut k, crate::contract::widths(&f), &[0.5], 64);
        let (_, _, state) = k.parts_mut();
        for (ch, offset) in offsets.into_iter().enumerate() {
            let lfo = (std::f64::consts::TAU * offset).sin() as f32;
            let hz = sweep_hz(m.min_hz, m.max_hz, lfo, Depth::FULL);
            let want = allpass_coeff(hz.get(), RATE.get() as f32);
            assert!(
                (state.points[ch][0] - want).abs() < 1e-5,
                "channel {ch}: {} against {want}",
                state.points[ch][0]
            );
        }
    }
}

/// Channels of input through a runner, one slice each (unlike the contract's `render`, which
/// feeds one signal to all), in blocks of `block`.
fn run(k: &mut Realized<PhaserRecurrence>, audio: &[Vec<f32>], block: usize) -> Vec<Vec<f32>> {
    let n = audio[0].len();
    let mut out = vec![vec![0.0f32; n]; audio.len()];
    let mut at = 0;
    while at < n {
        let end = (at + block).min(n);
        let a: Vec<&[f32]> = audio.iter().map(|c| &c[at..end]).collect();
        let feed = Feed::new(&a);
        let mut outs: Vec<&mut [f32]> = out.iter_mut().map(|c| &mut c[at..end]).collect();
        k.process(&feed, &mut outs, end - at);
        at = end;
    }
    out
}

/// A silent channel stays silent, exactly 0.0, whatever its neighbours carry (full-scale
/// noise, the whole sweep, the most feedback): each channel's all-pass state is its own,
/// and the feedback is its own output's. And the channels with the same
/// stagger share a sweep, so equal inputs give equal outputs (a full-cycle stagger of
/// channel 4 is channel 0's).
#[test]
fn a_silent_channel_stays_silent_and_each_channel_keeps_its_own_state() {
    let f = PhaserFamily::new(bus(6), Stages::of(6), range(300.0, 3_000.0), step(0.25)).unwrap();
    let (mut k, _) = built(&f, &controls(2.0, 1.0, 0.99), RATE);
    let x = noise(5, 4_096, 1.0);
    let audio = [
        x.clone(),
        x.clone(),
        x.clone(),
        vec![0.0f32; 4_096],
        x.clone(),
        noise(6, 4_096, 1.0),
    ];
    let out = run(&mut k, &audio, 64);
    assert!(out[3].iter().all(|&s| s == 0.0), "no bleed into a silence");
    assert_eq!(out[4], out[0], "a full-cycle stagger is no stagger");
    assert_ne!(out[1], out[0], "a quarter-cycle stagger sweeps elsewhere");
    assert_ne!(out[2], out[0], "a half-cycle stagger sweeps elsewhere");
    assert!(out[5].iter().any(|&s| s != 0.0));
}

/// A re-prepare restarts the sweep and empties the chain (the coefficient was the old rate's):
/// the LFO is at its start again, the control points are unsolved and the history is zero.
#[test]
fn a_re_prepare_restarts_the_sweep() {
    let f = family(2, 4, SweepRange::DEFAULT, step(0.25));
    let (mut k, _) = built(&f, &controls(3.0, 1.0, 0.7), RATE);
    render(
        &mut k,
        crate::contract::widths(&f),
        &noise(2, 1_000, 1.0),
        64,
    );
    {
        let (_, _, state) = k.parts_mut();
        assert_ne!(state.phase, Phase::START, "the sweep was running");
        assert!(state.x1.iter().any(|&s| s != 0.0));
    }
    k.prepare(&f.capacity(RATE), &crate::contract::setup(RATE));
    let (_, _, state) = k.parts_mut();
    assert_eq!(state.phase, Phase::START);
    assert_eq!((state.pos, state.primed), (0, false));
    assert!(state.x1.iter().chain(&state.y1).all(|&s| s == 0.0));
    assert!(state.feedback_sample.iter().all(|&s| s == 0.0));
}

// ---- the glides and the grid ---------------------------------------------------

/// The depth and the feedback glide over `GAIN_GLIDE` (240 frames at 48 kHz) from where they
/// were, halfway at the middle frame and landing exactly: not at the block, and not in one
/// step.
#[test]
fn depth_and_feedback_glide_over_the_gain_glide_and_land_exactly() {
    let f = family(1, 4, SweepRange::DEFAULT, Stagger::NONE);
    let (mut k, _) = built(&f, &controls(1.0, 0.0, 0.2), RATE);
    k.retarget(&f.member(&controls(1.0, 1.0, 0.8), RATE));
    let steps = 240; // GAIN_GLIDE, 5 ms at 48 kHz
    let w = crate::contract::widths(&f);
    render(&mut k, w, &noise(3, steps / 2, 1.0), 40);
    {
        let (_, coeffs, _) = k.parts_mut();
        assert!((coeffs.depth.now() - 0.5).abs() < 1e-6, "depth halfway");
        assert!(
            (coeffs.feedback.now() - 0.5).abs() < 1e-6,
            "feedback halfway"
        );
    }
    render(&mut k, w, &noise(3, steps / 2, 1.0), 40);
    let (_, coeffs, _) = k.parts_mut();
    assert_eq!(coeffs.depth.now(), 1.0);
    assert_eq!(coeffs.feedback.now(), 0.8);
    assert!(!coeffs.depth.moving() && !coeffs.feedback.moving());
}

/// A rate that moves lands at its **grid point** and the LFO's phase is continuous across
/// it: the phase is one walk, the old rate's step until the first control point at or after
/// the retarget and the new rate's from there. A retarget anywhere after frame 32 up to the
/// point at 48 gives the same walk; at 32 itself the point at 32 already takes the new.
#[test]
fn a_rate_change_lands_at_its_grid_point_and_the_phase_stays_continuous() {
    let f = family(1, 4, SweepRange::DEFAULT, Stagger::NONE);
    let (slow, fast) = (controls(2.0, 1.0, 0.5), controls(9.0, 1.0, 0.5));
    let inc = |hz: f32| PhaseIncrement::per_sample(Hz(hz), RATE);
    let walk = |first: usize, second: usize| {
        let mut p = Phase::START;
        for _ in 0..first {
            p = p.advance(inc(2.0));
        }
        for _ in 0..second {
            p = p.advance(inc(9.0));
        }
        p
    };
    let after = |at: usize| {
        let (mut k, _) = built(&f, &slow, RATE);
        let w = crate::contract::widths(&f);
        render(&mut k, w, &noise(4, at, 1.0), 64);
        k.retarget(&f.member(&fast, RATE));
        // To frame 70: the control points at 0, 16, 32, 48 and 64 are solved.
        render(&mut k, w, &noise(4, 70 - at, 1.0), 64);
        let (_, _, state) = k.parts_mut();
        state.phase
    };
    // The points at 0, 16 and 32 solve at the old rate (the phase at 48), those at 48 and 64
    // at the new.
    for at in [33, 40, 47, 48] {
        assert_eq!(after(at), walk(48, 32), "retargeted at frame {at}");
    }
    // At 32 the point at frame 32 is solved after the retarget.
    assert_eq!(after(32), walk(32, 48), "retargeted at frame 32");
    assert_ne!(walk(48, 32), walk(32, 48));
}

// ---- the checked constants --------------------------------------------------
//
// The shape `check_range_contract` checks is serde's; `Stages` has none in the library
// (nothing persists it), so the check runs on the same `try_from` shape written here.
impl Serialize for Stages {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.get().serialize(s)
    }
}

impl<'de> Deserialize<'de> for Stages {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::try_from(usize::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

/// `Stages` keeps the range-checked shape: `new` refuses a count outside
/// `2..=12` and never clamps.
#[test]
fn stages_keep_the_range_checked_shape() {
    tutti_fixtures::check_range_contract(&tutti_fixtures::RangeContract {
        new: Stages::new,
        get: Stages::get,
        of: Some(Stages::of),
        valid: &[2, 3, 7, 11, 12],
        invalid: &[0, 1, 13, 20, usize::MAX],
    });
    assert_eq!(Stages::try_from(13), Err(StagesError(13)));
    assert_eq!((Stages::MIN, Stages::MAX), (2, 12));
}

/// A [`SweepRange`] refuses what a sweep cannot hold and never clamps: a bound that is not
/// finite, a bottom under 20 Hz and a bottom above the top; the edge of each rule is in. The
/// 0.90-of-Nyquist cap is not here: it depends on the rate, and is the member's.
#[test]
fn a_sweep_range_refuses_what_a_sweep_cannot_hold() {
    let ok = |lo: f32, hi: f32| SweepRange::new(Hz(lo), Hz(hi));
    assert!(ok(20.0, 20.0).is_ok(), "the floor, a range of one value");
    assert!(ok(200.0, 4_000.0).is_ok());
    assert!(ok(20.0, 1e9).is_ok(), "the top is the rate's to cap");
    assert_eq!(
        ok(19.999, 4_000.0),
        Err(SweepRangeError::BelowFloor(Hz(19.999)))
    );
    assert_eq!(ok(-5.0, 100.0), Err(SweepRangeError::BelowFloor(Hz(-5.0))));
    assert_eq!(
        ok(300.0, 299.0),
        Err(SweepRangeError::Order {
            min: Hz(300.0),
            max: Hz(299.0)
        })
    );
    for (lo, hi) in [
        (f32::NAN, 100.0),
        (100.0, f32::NAN),
        (100.0, f32::INFINITY),
        (f32::INFINITY, f32::INFINITY),
        (f32::NEG_INFINITY, 100.0),
    ] {
        assert!(
            matches!(ok(lo, hi), Err(SweepRangeError::NotFinite { .. })),
            "{lo}..{hi}"
        );
    }
    let r = SweepRange::DEFAULT;
    assert_eq!((r.min(), r.max()), (Hz(200.0), Hz(4_000.0)));
    assert_eq!(SweepRange::default(), SweepRange::DEFAULT);
}

/// `of` panics where `new` refuses, the constant's spelling.
#[test]
#[should_panic(expected = "a sweep range is finite")]
fn a_sweep_range_of_a_transposed_pair_panics_where_it_is_a_constant() {
    let _ = SweepRange::of(Hz(4_000.0), Hz(200.0));
}

/// A family refuses offsets that are not one for each channel of its bus, and takes a step
/// at any width.
#[test]
fn a_family_refuses_offsets_that_do_not_fit_its_bus() {
    let two = Stagger::each(&[PhaseIncrement(0.0), PhaseIncrement(0.5)]).unwrap();
    let new = |w, s| PhaserFamily::new(bus(w), Stages::of(4), SweepRange::DEFAULT, s);
    assert!(new(2, two).is_ok());
    assert_eq!(
        new(3, two).unwrap_err(),
        StaggerError::Length { width: 3, got: 2 }
    );
    for width in [1, 2, 6, 16] {
        assert!(new(width, step(0.25)).is_ok());
        assert!(new(width, Stagger::NONE).is_ok());
    }
}

/// The member reports what the family was built to, and no latency (the sweep is the sound).
#[test]
fn the_member_states_what_it_was_built_to() {
    let f = family(2, 6, range(300.0, 3_000.0), step(0.25));
    let m = f.member(&controls(2.0, 0.5, 0.4), RATE);
    assert_eq!((m.width(), m.stages(), f.width()), (2, Stages::of(6), 2));
    assert_eq!(m.latency(), tutti_types::Samples::ZERO);
    assert_eq!(m.tail(Db(-60.0)), Ring::Unbounded);
    assert_eq!((m.min_hz, m.max_hz), (Hz(300.0), Hz(3_000.0)));
    // Frozen at the LFO's peak at half depth the centre is half way up the range, at its
    // trough the bottom.
    let (peak, trough) = (m.at(1.0), m.at(-1.0));
    assert!((peak.centre_hz().get() - (300.0 + 0.5 * 2_700.0)).abs() < 1e-2);
    assert_eq!(trough.centre_hz(), Hz(300.0));
}

// ---- the chord between control points ---------------------------------------

/// The coefficient `c(frame)` the sweep's definition gives, at depth `depth`, for a mono
/// chain on `range` at `rate` Hz: the sine of the LFO's `f32` phase, `sweep_hz`, `allpass_coeff`.
fn exact_coefficient(range: SweepRange, rate: f32, depth: f32, frame: usize) -> f32 {
    let inc = PhaseIncrement::per_sample(Hz(rate), RATE);
    let mut p = Phase::START;
    for _ in 0..frame {
        p = p.advance(inc);
    }
    let lfo = (std::f64::consts::TAU * f64::from(p.get())).sin() as f32;
    let hz = sweep_hz(range.min(), range.max(), lfo, Depth::clamped(depth));
    allpass_coeff(hz.get(), RATE.get() as f32)
}

/// Between two control points the coefficient is the chord: `prev + (next - prev) j / 16` at
/// frame `16 k + j`. An impulse at frame `n` of a fresh chain with no feedback comes out
/// as that frame's coefficient once a stage (`out = c (in - y1) + x1` with no history), so
/// through two stages as its square: the render shows it directly, on the stream's lattice
/// (points at multiples of 16) from the first block and whatever the blocks.
#[test]
fn between_control_points_the_coefficient_is_the_chord() {
    let band = range(300.0, 3_000.0);
    let f = family(1, 2, band, Stagger::NONE);
    let c = controls(5.0, 1.0, 0.0);
    for n in [0usize, 1, 7, 15, 16, 17, 31, 32, 40, 47] {
        let (mut k, _) = built(&f, &c, RATE);
        let mut x = vec![0.0f32; n + 1];
        x[n] = 1.0;
        // A block that ends on the impulse, in two pieces.
        let y = render(&mut k, crate::contract::widths(&f), &x, 5).remove(0);
        let (j, base) = (n % 16, n - n % 16);
        let (prev, next) = (
            exact_coefficient(band, 5.0, 1.0, base),
            exact_coefficient(band, 5.0, 1.0, base + 16),
        );
        let chord = prev + (next - prev) * (j as f32 / 16.0);
        assert!(
            (y[n] - chord * chord).abs() < 1e-6,
            "frame {n}: {} against the chord's square {}",
            y[n],
            chord * chord
        );
    }
}

/// A depth that glides is solved at a control point with the depth the glide has at the
/// **end** of the segment: the point's frame, 16 steps on. After one frame of a glide from
/// 0 to 1 over 240 steps the glide is at step 1, so the segment's start is solved at
/// `1 / 240` and its end at `17 / 240`.
#[test]
fn a_gliding_depth_is_solved_at_a_point_with_the_depth_the_glide_has_there() {
    let band = range(300.0, 3_000.0);
    let f = family(1, 4, band, Stagger::NONE);
    let (mut k, _) = built(&f, &controls(5.0, 0.0, 0.5), RATE);
    k.retarget(&f.member(&controls(5.0, 1.0, 0.5), RATE));
    render(&mut k, crate::contract::widths(&f), &[0.5], 64);
    let (_, _, state) = k.parts_mut();
    let start = exact_coefficient(band, 5.0, 1.0 / 240.0, 0);
    let end = exact_coefficient(band, 5.0, 17.0 / 240.0, 16);
    assert!((state.points[0][0] - start).abs() < 1e-6, "the start");
    assert!(
        (state.points[0][1] - end).abs() < 1e-6,
        "the end: {} against {end}",
        state.points[0][1]
    );
}
