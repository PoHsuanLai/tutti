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
mod tests;
