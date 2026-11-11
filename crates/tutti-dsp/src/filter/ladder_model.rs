//! The Moog ladder as a **small-signal** model: [`LadderModel`] (design 024 step
//! 2).
//!
//! The ladder is not linear: the resonance feedback runs through a `tanh`, and
//! that saturation is its character. The model is its **linearisation at the
//! origin**, where `tanh x = x`: the system the ladder is for signals small against
//! the saturator's knee (`|x| << 1`, with the drive in the gain). It is class 1 (an
//! [`Lti`]) by that definition, and every query is the linearised system's, so
//! it says nothing about what a loud signal does: past the knee the response
//! flattens and the resonance is limited, and a loop whose linearisation has a pole
//! outside the unit circle (a resonance near 1) settles into the saturator's limit
//! cycle instead of growing. `is_stable` is the small-signal figure: false there.
//!
//! One stage is the trapezoidal one-pole `L(z) = g1 (1 + z^-1) / (1 - p z^-1)`
//! (`g1 = g / (1 + g)`, `p = 1 - 2 g1`); the feedback reads the last stage's state,
//! `F(z) = 2 g1 z^-1 / (1 - p z^-1)` times the third stage's output, so the
//! input to the stages is `u = x / (1 + k F L^3)`. The four outputs are
//! `L^2 u`, `L^4 u`, `(1 - L)^2 u` and `(1 - L)^4 u`.

use core::f64::consts::PI;

use tutti_types::{Db, Drive, Hz, Resonance, SampleRate, Samples};

use super::biquad::decay_ring;
use super::{LadderCoeffs, LadderType};
use crate::{Complex64, Lti, Ring, System};

/// What a ladder's controls are: its type, cutoff, resonance and the drive into
/// the saturator.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LadderControls {
    /// The tap taken as the output.
    pub ty: LadderType,
    /// The cutoff.
    pub freq: Hz,
    /// The feedback, `0..=1` (not a [`Q`](tutti_types::Q)).
    pub res: Resonance,
    /// The gain into the saturator.
    pub drive: Drive,
}

impl LadderControls {
    /// The controls a non-finite value falls back to: a 1 kHz 12 dB low-pass with
    /// no resonance and unity drive.
    pub const DEFAULT: LadderControls = LadderControls {
        ty: LadderType::LP12,
        freq: Hz(1_000.0),
        res: Resonance(0.0),
        drive: Drive::UNITY,
    };

    /// Whether these controls are past the dead-band of `applied`, the controls a
    /// model was last designed from: a cutoff or resonance past `FREQ_EPS` or
    /// `RES_EPS`, or any change of the type or the drive. Drive is exact: it
    /// multiplies straight into the saturator, so a small step is not inaudible
    /// the way a small cutoff one is.
    ///
    /// The bands alone would not keep a NaN out (`|NaN - last| > eps` is false, so
    /// a NaN is held off only until another control moves); a node reads its cells
    /// through `LastGood`, which does.
    #[must_use]
    pub fn moved_from(&self, applied: &Self) -> bool {
        const FREQ_EPS: f32 = 0.01;
        const RES_EPS: f32 = 0.0001;
        self.ty != applied.ty
            || self.drive != applied.drive
            || (self.freq.get() - applied.freq.get()).abs() > FREQ_EPS
            || (self.res.get() - applied.res.get()).abs() > RES_EPS
    }

    /// These controls with every non-finite figure replaced by the default's.
    /// A cutoff, resonance or drive out of range is the coefficients' to clamp.
    fn sanitized(self) -> Self {
        let finite = |v: f32, d: f32| if v.is_finite() { v } else { d };
        Self {
            ty: self.ty,
            freq: Hz(finite(self.freq.get(), Self::DEFAULT.freq.get())),
            res: Resonance(finite(self.res.get(), Self::DEFAULT.res.get())),
            drive: Drive(finite(self.drive.get(), Self::DEFAULT.drive.get()).max(0.0)),
        }
    }
}

/// The small-signal model of a ladder filter at a rate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LadderModel {
    controls: LadderControls,
    coeffs: LadderCoeffs,
    rate: SampleRate,
}

impl LadderModel {
    /// The model of `controls` at `rate`; the drive is a gain on the input of the
    /// linearised system. Total: a non-finite control takes the default's, the
    /// cutoff and resonance are clamped as [`LadderCoeffs::new`] clamps them and a
    /// negative drive is 0.
    #[must_use]
    pub fn new(controls: LadderControls, rate: SampleRate) -> Self {
        let controls = controls.sanitized();
        Self {
            controls,
            coeffs: LadderCoeffs::new(controls.freq, controls.res, rate),
            rate,
        }
    }

    /// The controls it is the model of (sanitised).
    #[must_use]
    pub fn controls(&self) -> LadderControls {
        self.controls
    }

    /// The type.
    #[must_use]
    pub fn ladder_type(&self) -> LadderType {
        self.controls.ty
    }

    /// The drive, a gain on the input, as the `f32` it was set from.
    #[must_use]
    pub fn drive(&self) -> f64 {
        f64::from(self.controls.drive.get())
    }

    /// The coefficients the ladder is solved at.
    #[must_use]
    pub fn coeffs(&self) -> &LadderCoeffs {
        &self.coeffs
    }

    /// The stage gain `g / (1 + g)`.
    fn g1(&self) -> f64 {
        self.coeffs.g / (1.0 + self.coeffs.g)
    }

    /// `(L, F)` at `z^-1 = zi`.
    fn stage(&self, zi: Complex64) -> (Complex64, Complex64) {
        let g1 = self.g1();
        let den = 1.0 - (1.0 - 2.0 * g1) * zi;
        (g1 * (1.0 + zi) / den, 2.0 * g1 * zi / den)
    }

    /// The response at `z^-1 = zi`.
    fn at(&self, zi: Complex64) -> Complex64 {
        let (l, f) = self.stage(zi);
        let u = f64::from(self.controls.drive.get()) / (1.0 + self.coeffs.k * f * l * l * l);
        let one_minus = 1.0 - l;
        u * match self.controls.ty {
            LadderType::LP12 => l * l,
            LadderType::LP24 => l * l * l * l,
            LadderType::HP12 => one_minus * one_minus,
            LadderType::HP24 => (one_minus * one_minus) * (one_minus * one_minus),
        }
    }

    /// The four poles: the roots of `(z - p)^4 + 2 k g1^4 (z + 1)^3`, by
    /// Durand and Kerner's iteration (control side).
    #[must_use]
    pub fn poles(&self) -> [Complex64; 4] {
        let g1 = self.g1();
        let p = 1.0 - 2.0 * g1;
        let c = 2.0 * self.coeffs.k * g1.powi(4);
        // Monic coefficients of z^4 .. z^0.
        let a = [
            1.0,
            -4.0 * p + c,
            6.0 * p * p + 3.0 * c,
            -4.0 * p.powi(3) + 3.0 * c,
            p.powi(4) + c,
        ];
        let eval = |z: Complex64| {
            a.iter()
                .fold(Complex64::new(0.0, 0.0), |acc, &k| acc * z + k)
        };
        let mut roots: [Complex64; 4] = core::array::from_fn(|i| {
            Complex64::from_polar(0.6 + 0.1 * i as f64, 0.9 + 1.7 * i as f64)
        });
        for _ in 0..500 {
            let mut moved = 0.0f64;
            for i in 0..4 {
                let mut den = Complex64::new(1.0, 0.0);
                for j in 0..4 {
                    if j != i {
                        den *= roots[i] - roots[j];
                    }
                }
                let step = eval(roots[i]) / den;
                roots[i] -= step;
                moved = moved.max(step.norm());
            }
            if moved < 1e-15 {
                break;
            }
        }
        roots
    }
}

impl System for LadderModel {
    /// Zero.
    fn latency(&self) -> Samples {
        Samples::ZERO
    }

    /// The linearised loop's ring: its slowest pole, `ln(eps) / ln r` frames.
    fn tail(&self, floor: Db) -> Ring {
        decay_ring(self.spectral_radius(), 1, floor, self.rate)
    }
}

impl Lti for LadderModel {
    fn frequency_response(&self, output: usize, freqs: &[Hz], gain: &mut [Complex64]) {
        for (g, f) in gain.iter_mut().zip(freqs) {
            let w = 2.0 * PI * f64::from(f.get()) / self.rate.get();
            *g = if output == 0 {
                self.at(Complex64::from_polar(1.0, -w))
            } else {
                Complex64::new(f64::NAN, f64::NAN)
            };
        }
    }

    /// The largest pole modulus of the linearisation (the roots as the iteration
    /// found them): at or above 1 the small-signal system is unstable and the
    /// saturator is what holds the real filter.
    fn spectral_radius(&self) -> f64 {
        self.poles().iter().map(|p| p.norm()).fold(0.0, f64::max)
    }

    /// Infinity: the linearisation has no bound the model can state (past the
    /// saturator's knee the filter does not follow it).
    fn l1_norm(&self) -> f64 {
        f64::INFINITY
    }

    /// The peak of the small-signal response on a grid of 2048 frequencies and a
    /// run around each pole's angle, refined by golden section, then raised by a
    /// part in a million.
    fn hinf_norm(&self) -> f64 {
        let mut pts: Vec<f64> = (0..=2048).map(|i| PI * f64::from(i) / 2048.0).collect();
        for p in self.poles() {
            let (w0, width) = (p.arg().abs(), (1.0 - p.norm()).abs().max(1e-12));
            for i in 0..60 {
                let d = width * libm::pow(10.0, (f64::from(i) - 30.0) / 8.0);
                pts.extend([w0 + d, w0 - d]);
            }
        }
        pts.retain(|w| (0.0..=PI).contains(w));
        pts.sort_by(f64::total_cmp);
        pts.dedup();
        let mag = |w: f64| self.at(Complex64::from_polar(1.0, -w)).norm();
        let best = pts
            .iter()
            .enumerate()
            .max_by(|a, b| mag(*a.1).total_cmp(&mag(*b.1)))
            .map_or(0, |(i, _)| i);
        let (mut a, mut b) = (
            pts[best.saturating_sub(1)],
            pts[(best + 1).min(pts.len() - 1)],
        );
        let phi = 0.618_033_988_749_894_9;
        for _ in 0..80 {
            let (x, y) = (b - phi * (b - a), a + phi * (b - a));
            if mag(x) < mag(y) {
                a = x;
            } else {
                b = y;
            }
        }
        mag(pts[best]).max(mag(0.5 * (a + b))) * (1.0 + 1e-6)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::ladder::{LadderCoefficients, ladder_step};
    use crate::{Complex32, RealFft};

    const RATE: SampleRate = SampleRate(48_000.0);

    fn model(ty: LadderType, fc: f32, res: f32) -> LadderModel {
        LadderModel::new(
            LadderControls {
                ty,
                freq: Hz(fc),
                res: Resonance(res),
                drive: Drive::UNITY,
            },
            RATE,
        )
    }

    fn coefficients(freq: Hz, res: Resonance) -> LadderCoefficients<f64> {
        LadderCoefficients::from(&LadderCoeffs::new(freq, res, RATE))
    }

    fn at(m: &LadderModel, f: f32) -> Complex64 {
        let mut g = [Complex64::new(0.0, 0.0)];
        m.frequency_response(0, &[Hz(f)], &mut g);
        g[0]
    }

    /// The closed forms of the linearisation: DC is `1 / (1 + k)` for a low-pass
    /// and 0 for a high-pass, the stages are 1/sqrt(2) at the cutoff, so with no
    /// resonance a 12 dB type is 1/2 there and a 24 dB type 1/4 (the stage is a
    /// trapezoidal one-pole, prewarped to put its corner exactly on the cutoff), and
    /// Nyquist is 0 for a low-pass and 1 for a high-pass.
    #[test]
    fn the_response_has_the_closed_forms() {
        for res in [0.0f32, 0.5, 0.9] {
            let k = 4.0 * f64::from(res);
            let lp = model(LadderType::LP24, 1_000.0, res);
            assert!(
                (at(&lp, 0.0).norm() - 1.0 / (1.0 + k)).abs() < 1e-12,
                "res {res}"
            );
            assert!(at(&lp, 24_000.0).norm() < 1e-12);
            let hp = model(LadderType::HP12, 1_000.0, res);
            assert!(at(&hp, 0.0).norm() < 1e-12);
            assert!((at(&hp, 24_000.0).norm() - 1.0).abs() < 1e-9, "res {res}");
        }
        for (ty, want) in [
            (LadderType::LP12, 0.5),
            (LadderType::LP24, 0.25),
            (LadderType::HP12, 0.5),
            (LadderType::HP24, 0.25),
        ] {
            let g = at(&model(ty, 1_000.0, 0.0), 1_000.0).norm();
            assert!((g - want).abs() < 1e-9, "{ty:?}: {g}");
        }
    }

    /// The ladder's response, **at small signal**, is the model's: an impulse of
    /// 1e-7 through `ladder_step` (where `tanh x = x` to 1e-14) has the model's
    /// spectrum to 1e-6 of its norm, for each type and three resonances.
    #[test]
    fn the_small_signal_response_is_the_models() {
        const N: usize = 1 << 15;
        let amp = 1e-7f64;
        for ty in [
            LadderType::LP12,
            LadderType::LP24,
            LadderType::HP12,
            LadderType::HP24,
        ] {
            for res in [0.0f32, 0.4, 0.8] {
                let m = model(ty, 1_200.0, res);
                let c = coefficients(Hz(1_200.0), Resonance(res));
                let mut s = [0.0f64; 4];
                let ir: Vec<f32> = (0..N)
                    .map(|n| {
                        (ladder_step(&c, ty, &mut s, if n == 0 { amp } else { 0.0 }, 1.0) / amp)
                            as f32
                    })
                    .collect();
                let fft = RealFft::new(N);
                let mut scratch = fft.new_scratch();
                let mut bins = vec![Complex32::new(0.0, 0.0); fft.bins()];
                fft.forward(&ir, &mut bins, &mut scratch);
                let (mut num, mut den) = (0.0f64, 0.0f64);
                for (i, b) in bins.iter().enumerate().step_by(7) {
                    let want = at(&m, (i as f64 * RATE.get() / N as f64) as f32);
                    let got = Complex64::new(f64::from(b.re), f64::from(b.im));
                    num += (got - want).norm_sqr();
                    den += want.norm_sqr();
                }
                let err = (num / den).sqrt();
                assert!(err < 1e-4, "{ty:?} res {res}: {err:.2e}");
            }
        }
    }

    /// The linearisation is where the model stops: a loud impulse through the
    /// real ladder at a high resonance is not the model's response (the `tanh`
    /// limits the loop), which is what keeps the class honest. An impulse of 3
    /// through a resonance of 0.9 differs from the small-signal one by more than
    /// 10% of the response's norm.
    #[test]
    fn the_model_is_the_small_signal_system_and_loud_signals_leave_it() {
        const N: usize = 1 << 14;
        let c = coefficients(Hz(1_200.0), Resonance(0.9));
        let run = |amp: f64| {
            let mut s = [0.0f64; 4];
            (0..N)
                .map(|n| {
                    ladder_step(
                        &c,
                        LadderType::LP24,
                        &mut s,
                        if n == 0 { amp } else { 0.0 },
                        1.0,
                    ) / amp
                })
                .collect::<Vec<f64>>()
        };
        let (small, loud) = (run(1e-7), run(3.0));
        let diff: f64 = small.iter().zip(&loud).map(|(a, b)| (a - b).powi(2)).sum();
        let norm: f64 = small.iter().map(|a| a * a).sum();
        assert!((diff / norm).sqrt() > 0.1, "{}", (diff / norm).sqrt());
    }

    /// The ring: the slowest pole of the linearisation against the measured decay
    /// of the small-signal impulse response, and the tail against the measured time
    /// to -60 dB (covers it, within 3 times).
    #[test]
    fn the_radius_and_the_tail_are_what_the_linearised_filter_rings() {
        const N: usize = 1 << 17;
        let amp = 1e-7f64;
        let m = model(LadderType::LP24, 300.0, 0.8);
        let c = coefficients(Hz(300.0), Resonance(0.8));
        let mut s = [0.0f64; 4];
        let ir: Vec<f64> = (0..N)
            .map(|n| {
                ladder_step(
                    &c,
                    LadderType::LP24,
                    &mut s,
                    if n == 0 { amp } else { 0.0 },
                    1.0,
                ) / amp
            })
            .collect();
        let r = m.spectral_radius();
        assert!(r < 1.0 && r > 0.99, "{r}");
        // The envelope over windows of 256 frames: the energy decays as r^(2 n).
        let w = 256;
        let e: Vec<f64> = ir
            .chunks(w)
            .map(|c| c.iter().map(|x| x * x).sum::<f64>())
            .collect();
        let top = e.iter().copied().fold(0.0, f64::max);
        let pts: Vec<(f64, f64)> = e
            .iter()
            .enumerate()
            .filter(|(_, v)| **v < top * 1e-3 && **v > top * 1e-9)
            .map(|(i, v)| (i as f64, libm::log(*v)))
            .collect();
        let n = pts.len() as f64;
        assert!(pts.len() > 5, "{} points, top {top}", pts.len());
        let (sx, sy) = pts.iter().fold((0.0, 0.0), |(a, b), p| (a + p.0, b + p.1));
        let sxx: f64 = pts.iter().map(|p| p.0 * p.0).sum();
        let sxy: f64 = pts.iter().map(|p| p.0 * p.1).sum();
        let measured = -((n * sxy - sx * sy) / (n * sxx - sx * sx)) / (2.0 * w as f64);
        let bound = -libm::log(r);
        assert!(
            measured >= 0.97 * bound && measured <= 1.1 * bound,
            "measured {measured:.3e}, the radius says {bound:.3e}"
        );
        // Time to -60 dB of the peak envelope, in 256-frame windows.
        let win = 256;
        let env: Vec<f64> = ir
            .chunks(win)
            .map(|c| (c.iter().map(|x| x * x).sum::<f64>() / win as f64).sqrt())
            .collect();
        let peak = env.iter().copied().fold(0.0, f64::max);
        let last = env.iter().rposition(|&v| v >= peak * 1e-3).unwrap();
        let took = ((last + 1) * win) as f64 / RATE.get();
        let Ring::Finite(tail) = m.tail(Db(-60.0)) else {
            panic!("finite")
        };
        assert!(
            tail.get() >= took,
            "tail {} against measured {took}",
            tail.get()
        );
        assert!(
            tail.get() <= 3.0 * took,
            "tail {} against measured {took}",
            tail.get()
        );
    }

    /// A resonance near 1 has a pole outside the unit circle in the linearisation
    /// (the loop gain `4 res` passes what the unassigned loop supports): the model says
    /// so, and `is_stable` is false: the real filter is held by the saturator.
    #[test]
    fn a_full_resonance_is_unstable_at_small_signal() {
        assert!(model(LadderType::LP24, 1_000.0, 1.0).spectral_radius() >= 1.0);
        assert!(!model(LadderType::LP24, 1_000.0, 1.0).is_stable());
        assert!(model(LadderType::LP24, 1_000.0, 0.5).is_stable());
        assert!(
            model(LadderType::LP24, 1_000.0, 0.5)
                .tail(Db(-60.0))
                .seconds()
                .is_some()
        );
        assert_eq!(
            model(LadderType::LP24, 1_000.0, 1.0).tail(Db(-60.0)),
            Ring::Unbounded
        );
    }
}
