//! The second-order section both the crossover and the SVF's equivalent forms
//! share: its coefficients, its response and its poles.

use tutti_types::{Db, SampleRate, Seconds};

use crate::{Complex64, Ring};

/// A second-order section with `a0 = 1`:
/// `H(z) = (b0 + b1 z^-1 + b2 z^-2) / (1 + a1 z^-1 + a2 z^-2)`.
///
/// A plain value of coefficients. Several structures compute it (the transposed
/// direct form II the crossover runs, a lattice, the SVF's trapezoidal one), which
/// is the point of design 024: the coefficients are one *realization's* numbers
/// for a response, never the response itself.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Biquad {
    /// Numerator at `z^0`.
    pub b0: f64,
    /// Numerator at `z^-1`.
    pub b1: f64,
    /// Numerator at `z^-2`.
    pub b2: f64,
    /// Denominator at `z^-1`.
    pub a1: f64,
    /// Denominator at `z^-2`.
    pub a2: f64,
}

impl Biquad {
    /// `H(e^jw)`, evaluated directly from the coefficients. Poorly conditioned
    /// for a pole very near `z = 1` and a frequency near DC (the SVF's own
    /// response reads its trapezoidal form instead).
    #[must_use]
    pub fn response(&self, w: f64) -> Complex64 {
        let z1 = Complex64::from_polar(1.0, -w);
        let z2 = z1 * z1;
        (self.b0 + self.b1 * z1 + self.b2 * z2) / (1.0 + self.a1 * z1 + self.a2 * z2)
    }

    /// The two poles, the roots of `z^2 + a1 z + a2`.
    #[must_use]
    pub fn poles(&self) -> [Complex64; 2] {
        let disc = Complex64::new(self.a1 * self.a1 - 4.0 * self.a2, 0.0).sqrt();
        let half = Complex64::new(-self.a1 / 2.0, 0.0);
        [half + disc / 2.0, half - disc / 2.0]
    }

    /// The larger pole modulus: the slowest decay per frame.
    #[must_use]
    pub fn spectral_radius(&self) -> f64 {
        let [p, q] = self.poles();
        p.norm().max(q.norm())
    }
}

/// A [`Biquad`]'s direct form I history: the last two inputs and outputs, in
/// `f64`. The history is the filter's whole state and is independent of the
/// coefficients, so a caller may change them between blocks (the SoundFont voice
/// does, every chunk) or hold the history while it bypasses the section
/// ([`hold`](Self::hold)).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DirectForm1 {
    x: [f64; 2],
    y: [f64; 2],
}

impl DirectForm1 {
    /// Filter `block` in place through `c`.
    pub fn process(&mut self, c: &Biquad, block: &mut [f32]) {
        for sample in block {
            let x = f64::from(*sample);
            let y = c.b0 * x + c.b1 * self.x[0] + c.b2 * self.x[1]
                - c.a1 * self.y[0]
                - c.a2 * self.y[1];
            self.x = [x, self.x[0]];
            self.y = [y, self.y[0]];
            *sample = y as f32;
        }
    }

    /// Pass `block` through untouched, leaving the history as the filter's would
    /// be had its output been its input, so the section can take over without a
    /// step. A block shorter than two frames keeps what the history held.
    pub fn hold(&mut self, block: &[f32]) {
        for &sample in block.iter().rev().take(2).rev() {
            let x = f64::from(sample);
            self.x = [x, self.x[0]];
            self.y = [x, self.y[0]];
        }
    }

    /// Clear the history.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// `Ring` of a system whose slowest pole has modulus `radius` and whose response
/// is `n^(m - 1) radius^n` at its slowest (a pole of multiplicity `m`): the
/// frames until that falls to `floor`, which is the figure of a model that knows
/// only its spectral radius, plus one time constant (`1 / -ln r`): the response
/// peaks some frames in and not at `n = 0`, so a tail measured from the peak must
/// not be reported short. [`Ring::Unbounded`] for a radius that is not below 1.
pub(super) fn decay_ring(radius: f64, multiplicity: u32, floor: Db, rate: SampleRate) -> Ring {
    if radius.is_nan() || radius >= 1.0 {
        return Ring::Unbounded;
    }
    if radius <= 0.0 {
        return Ring::Finite(Seconds::new(0.0));
    }
    let eps = libm::log(libm::pow(10.0, f64::from(floor.get()) / 20.0));
    let per_frame = -libm::log(radius);
    let mut frames = eps / -per_frame;
    for _ in 0..3 {
        let poly = f64::from(multiplicity - 1) * libm::log(frames.max(2.0));
        frames = (eps.abs() + poly) / per_frame;
    }
    Ring::Finite(Seconds::new(
        (frames.max(0.0) + 1.0 / per_frame) / rate.get(),
    ))
}

/// An upper bound on the l1 norm of the impulse response of one section:
/// `(|b0| + |b1| + |b2|)` times the l1 bound of the all-pole part, which is
/// `min((1 - r)^-2, K / (1 - r))` for `r` its slowest pole and `K = 2r / |p1 - p2|`
/// (the all-pole response is at most `min(n + 1, K) r^n`).
pub(super) fn section_l1(c: &Biquad) -> f64 {
    let [p, q] = c.poles();
    let r = p.norm().max(q.norm());
    if r.is_nan() || r >= 1.0 {
        return f64::INFINITY;
    }
    let gap = (p - q).norm();
    let k = if gap > 0.0 {
        2.0 * r / gap
    } else {
        f64::INFINITY
    };
    let slack = 1.0 - r;
    let all_pole = (1.0 / (slack * slack)).min(k / slack);
    (c.b0.abs() + c.b1.abs() + c.b2.abs()) * all_pole
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The response is the rational function and the poles are its roots.
    ///
    /// Mutations (run): `a1` and `a2` swapped in `response` fails the first
    /// assertion; `+ disc` on both poles fails the second.
    #[test]
    fn the_response_is_the_rational_function_and_the_poles_its_roots() {
        let c = Biquad {
            b0: 0.2,
            b1: 0.1,
            b2: -0.05,
            a1: -1.2,
            a2: 0.5,
        };
        let w: f64 = 0.7;
        let z = Complex64::from_polar(1.0, w);
        let want = (c.b0 + c.b1 / z + c.b2 / (z * z)) / (1.0 + c.a1 / z + c.a2 / (z * z));
        assert!((c.response(w) - want).norm() < 1e-12);
        for p in c.poles() {
            assert!((p * p + c.a1 * p + c.a2).norm() < 1e-12, "{p}");
        }
        assert!((c.spectral_radius() - 0.5f64.sqrt()).abs() < 1e-12);
    }

    /// `process` runs the difference equation, `hold` leaves the history of a
    /// pass-through, and `reset` clears it.
    ///
    /// Mutations (run, each failed): the two `y` taps swapped in `process`
    /// (the impulse); `hold` taking the first two samples instead of the last
    /// two, and `hold` leaving `y` alone (the hand-over).
    #[test]
    fn direct_form_i_runs_the_difference_equation_and_holds_a_pass_through() {
        let c = Biquad {
            b0: 0.5,
            b1: 0.25,
            b2: -0.125,
            a1: -0.5,
            a2: 0.25,
        };
        let mut f = DirectForm1::default();
        let mut out = [1.0f32, 0.0, 0.0, 0.0];
        f.process(&c, &mut out);
        // y0 = b0; y1 = b1 - a1 y0; y2 = b2 - a1 y1 - a2 y0; y3 = -a1 y2 - a2 y1.
        let y1 = 0.25 + 0.5 * 0.5;
        let y2 = -0.125 + 0.5 * y1 - 0.25 * 0.5;
        let y3 = 0.5 * y2 - 0.25 * y1;
        for (got, want) in out.iter().zip([0.5, y1, y2, y3]) {
            assert!((f64::from(*got) - want).abs() < 1e-7, "{got} vs {want}");
        }
        // Held over [9, 1, 2], then a section fed 0 answers as if 1 then 2 had
        // passed through it unchanged.
        let mut held = DirectForm1::default();
        held.hold(&[9.0, 1.0, 2.0]);
        let mut after = [0.0f32];
        held.process(&c, &mut after);
        let want = c.b1 * 2.0 + c.b2 * 1.0 - c.a1 * 2.0 - c.a2 * 1.0;
        assert!((f64::from(after[0]) - want).abs() < 1e-7);
        held.reset();
        assert_eq!(held, DirectForm1::default());
    }
}
