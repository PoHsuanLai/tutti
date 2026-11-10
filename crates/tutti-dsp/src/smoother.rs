//! The one-pole smoother: `y += c * (x - y)`, the recurrence every
//! de-zippering and envelope-following filter in the engine runs.

/// A one-pole smoother over an `f32` quantity: each step closes the fraction
/// `coeff` of the gap to the target (`1` is instant, `0` holds).
///
/// The coefficient comes in already derived, from the time constant and the
/// rate the smoother is stepped at (`Seconds::one_pole_coeff` in
/// `tutti-types`), so the smoother knows no time and no rate. A follower with
/// separate attack and release coefficients picks one per step with
/// [`toward`](Self::toward), which is the same step with the state held by
/// the caller.
///
/// The value and the coefficient are bare `f32`: the value is whichever
/// quantity the caller smooths (a gain, a degree of height), so no unit
/// describes it, and a coefficient is a fraction with no unit of its own.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Smoother {
    value: f32,
    coeff: f32,
}

impl Smoother {
    /// A smoother at `0.0` closing `coeff` of the gap per step.
    pub const fn new(coeff: f32) -> Self {
        Self { value: 0.0, coeff }
    }

    /// One step from `current` toward `target`, closing `coeff` of the gap:
    /// the recurrence itself, for a caller that keeps the state.
    ///
    /// At `coeff = 1` the result is `target` only to within half an ulp of
    /// the larger of the two (exactly when they are within 2x of each
    /// other). The retention form `(1 - c) * current + c * target` would be
    /// exact there, but in `f32` its `1 - c` rounds to 1 for a long time
    /// constant and the value then stops approaching a lower target.
    #[inline]
    pub fn toward(current: f32, target: f32, coeff: f32) -> f32 {
        current + coeff * (target - current)
    }

    /// One step toward `target`; returns the new value.
    #[inline]
    pub fn step(&mut self, target: f32) -> f32 {
        self.value = Self::toward(self.value, target, self.coeff);
        self.value
    }

    /// The fraction of the gap `steps` consecutive steps close:
    /// `1 - (1 - coeff)^steps`, in `f64` so the power does not lose a small
    /// coefficient. `steps == 0` closes none and an instant smoother
    /// (`coeff == 1`) all of it.
    #[inline]
    pub fn gap_closed(&self, steps: usize) -> f32 {
        let retained = 1.0 - f64::from(self.coeff);
        let steps = i32::try_from(steps).unwrap_or(i32::MAX);
        (1.0 - retained.powi(steps)) as f32
    }

    /// [`step`](Self::step) `steps` times toward the same target in one
    /// move: `v + (target - v) * gap_closed(steps)` instead of the loop.
    ///
    /// The loop injects an `f32` rounding error of about 1.5 ulp of the range
    /// per step that the next steps then shrink by `1 - coeff`, so it sits
    /// within `1.5 * 2^-24 * R * min(steps, 1/coeff)` of the exact recurrence
    /// (`R` the range of the quantity). This form carries one rounding of its
    /// own, so it is the closer of the two to the exact answer; the two
    /// differ by at most that bound.
    #[inline]
    pub fn advance(&mut self, target: f32, steps: usize) -> f32 {
        if steps > 0 {
            self.value += self.gap_closed(steps) * (target - self.value);
        }
        self.value
    }

    /// The running value.
    #[inline]
    pub fn value(&self) -> f32 {
        self.value
    }

    /// Seats the running value, discarding whatever ramp was in flight.
    #[inline]
    pub fn seed(&mut self, value: f32) {
        self.value = value;
    }

    /// The fraction of the gap one step closes.
    #[inline]
    pub fn coeff(&self) -> f32 {
        self.coeff
    }

    /// Changes the fraction of the gap one step closes, keeping the value.
    #[inline]
    pub fn set_coeff(&mut self, coeff: f32) {
        self.coeff = coeff;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A step closes `coeff` of the gap: a tenth up, a tenth down, all of it
    /// at 1, none at 0.
    ///
    /// Mutation (run): `toward` returning `current + coeff * target` fails.
    /// Also carries the height half of the deleted `tutti-nodes` smoother tests
    /// (the plain step of `ExponentialSmoother::process`): the same mutation
    /// failed the deleted `linear_smoothing_still_converges_for_elevation`.
    /// (The retention form is not caught here, where `1 - c` is exact; the
    /// nodes' render digests catch it.)
    #[test]
    fn a_step_closes_the_coefficient_of_the_gap() {
        assert!((Smoother::toward(0.0, 1.0, 0.1) - 0.1).abs() < 0.001);
        assert!((Smoother::toward(1.0, 0.0, 0.1) - 0.9).abs() < 0.001);
        assert_eq!(Smoother::toward(0.0, 1.0, 1.0), 1.0);
        assert_eq!(Smoother::toward(0.25, 1.0, 0.0), 0.25);
        let mut s = Smoother::new(0.5);
        s.seed(0.0);
        assert_eq!(s.step(1.0), 0.5);
        assert_eq!(s.step(1.0), 0.75);
        assert_eq!(s.value(), 0.75);
    }

    /// `advance` is `steps` single steps, within the rounding bound it
    /// documents (`1.5 * 2^-24 * R * min(steps, 1/coeff)`, with a factor of
    /// two for the closed form's own rounding), and exact for zero steps and
    /// for an instant smoother.
    ///
    /// Mutation (each run, each fails): the retention `1 - coeff` replaced by
    /// `coeff` in `gap_closed`; `gap_closed`'s `steps` replaced by 1.
    /// Also carries the height, zero-step and instant halves of the deleted
    /// `tutti-nodes` `advance_is_the_closed_form_of_repeated_steps` (run: the
    /// retention mutation fails both).
    #[test]
    fn advance_is_the_closed_form_of_repeated_steps() {
        for coeff in [0.0004_f32, 0.02, 0.3] {
            for steps in [1usize, 7, 64, 1024, 48_000] {
                let mut looped = Smoother::new(coeff);
                looped.seed(-30.0);
                for _ in 0..steps {
                    looped.step(90.0);
                }
                let mut closed = Smoother::new(coeff);
                closed.seed(-30.0);
                let got = closed.advance(90.0, steps);
                let range = 120.0_f64;
                let bound =
                    2.0 * 1.5 * 2f64.powi(-24) * range * (steps as f64).min(1.0 / f64::from(coeff));
                let exact = 90.0 - 120.0 * (1.0 - f64::from(coeff)).powi(steps as i32);
                assert!(
                    (f64::from(got) - exact).abs() <= bound + 1e-6,
                    "{coeff} x {steps}: closed {got} vs exact {exact}"
                );
                assert!(
                    (f64::from(looped.value()) - f64::from(got)).abs() <= 2.0 * bound + 1e-6,
                    "{coeff} x {steps}: loop {} vs closed {got}",
                    looped.value()
                );
            }
        }
        let mut s = Smoother::new(0.1);
        s.seed(10.0);
        assert_eq!(s.advance(70.0, 0), 10.0);
        let mut instant = Smoother::new(1.0);
        instant.seed(10.0);
        assert_eq!(instant.advance(70.0, 5), 70.0);
    }

    /// The fraction closed is `1 - (1 - c)^n`: nothing at zero steps, `c` at
    /// one, all of it at an instant coefficient, and a time constant's worth
    /// of steps (`1 / c`) closes 63%.
    ///
    /// Mutation (run): `gap_closed` returning `coeff * steps` fails the
    /// time-constant case.
    #[test]
    fn the_gap_closed_is_one_minus_the_retention_to_the_steps() {
        let s = Smoother::new(0.01);
        assert_eq!(s.gap_closed(0), 0.0);
        assert!((s.gap_closed(1) - 0.01).abs() < 1e-9);
        assert!((s.gap_closed(100) - (1.0 - (-1.0_f32).exp())).abs() < 3e-3);
        assert_eq!(Smoother::new(1.0).gap_closed(3), 1.0);
    }
}
