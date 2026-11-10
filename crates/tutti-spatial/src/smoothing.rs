//! One-pole exponential smoother that de-zippers the panners' position
//! changes.

use tutti_types::{Azimuth, Elevation, SampleRate, Samples, Seconds};

pub const DEFAULT_POSITION_SMOOTH_TIME: Seconds = Seconds(0.05);

#[derive(Clone, Debug)]
pub struct ExponentialSmoother {
    value: f32,
    coeff: f32,
    /// The time constant this smoother was built with, kept so
    /// [`set_sample_rate`](Self::set_sample_rate) can re-derive `coeff` without
    /// the caller re-supplying it.
    smooth_time: Seconds,
    /// Samples of audio each step stands for: 1 for a smoother stepped every
    /// sample, the frame length for one stepped once per frame.
    stride: Samples,
}

impl ExponentialSmoother {
    /// A smoother stepped once per `samples_per_step` samples of audio at
    /// `sample_rate`: each step covers that many samples of the time
    /// constant, so `smooth_time` is audio time whatever the stride.
    pub fn new(
        smooth_time: impl Into<Seconds>,
        sample_rate: impl Into<SampleRate>,
        samples_per_step: Samples,
    ) -> Self {
        // The coefficient is `f32`: the time is read at `f32` precision.
        let smooth_time = smooth_time.into().narrowed();
        Self {
            value: 0.0,
            coeff: Self::coeff(smooth_time, sample_rate.into(), samples_per_step),
            smooth_time,
            stride: samples_per_step,
        }
    }

    /// The fraction of the gap one step of `stride` samples closes: the
    /// one-pole coefficient at the rate the smoother is stepped,
    /// `sr / stride` ([`Seconds::one_pole_coeff`]; a zero time is instant).
    fn coeff(smooth_time: Seconds, sr: SampleRate, stride: Samples) -> f32 {
        smooth_time.one_pole_coeff(SampleRate(sr.get() / stride.get() as f64))
    }

    /// Recomputes the smoothing coefficient for a new sample rate, holding the
    /// original smoothing time constant and stride. Without this the
    /// de-zipper ramp runs at whatever rate the smoother was built with.
    pub fn set_sample_rate(&mut self, sample_rate: impl Into<SampleRate>) {
        self.coeff = Self::coeff(self.smooth_time, sample_rate.into(), self.stride);
    }

    /// Step toward a target *height* along the number line.
    ///
    /// Correct for a quantity with two ends and no seam. **Wrong for a
    /// bearing**: see `process_angle`. The pair is the
    /// point — one entry per space, each typed, so a bearing cannot reach the
    /// linear form by accident.
    #[cfg(any(feature = "hrtf", test))]
    #[inline]
    pub fn process(&mut self, target: Elevation) -> Elevation {
        self.value += self.coeff * (target.get() - self.value);
        Elevation(self.value)
    }

    /// Step toward a target *bearing*, taking the short way around.
    ///
    /// `process` computes `target - self.value`, which on a circle can be the
    /// long way: moving from 170 degrees to -170 degrees is 20 degrees to the
    /// right, but the plain subtraction reports -340 and the smoother sweeps
    /// almost the whole circle to get there. That is audible — the source
    /// travels the wrong direction past the listener.
    ///
    /// [`Azimuth::shortest_arc_to`] is the correct difference, and the running
    /// value is re-wrapped each step so it never drifts out of `-180..180`.
    #[cfg(any(feature = "hrtf", test))]
    #[inline]
    pub fn process_angle(&mut self, target: Azimuth) -> Azimuth {
        let current = Azimuth(self.value).wrap();
        let arc = current.shortest_arc_to(target.wrap());
        let next = current.rotate_by(arc * self.coeff);
        self.value = next.get();
        next
    }

    /// The fraction of the gap `steps` consecutive steps close:
    /// `1 - (1 - coeff)^steps`, in f64 so the power does not lose the small
    /// coefficient. `steps == 0` closes none and an instant smoother
    /// (`coeff == 1`) all of it.
    #[inline]
    fn gap_closed(&self, steps: usize) -> f32 {
        let retained = 1.0 - f64::from(self.coeff);
        let steps = i32::try_from(steps).unwrap_or(i32::MAX);
        (1.0 - retained.powi(steps)) as f32
    }

    /// `process` `steps` times toward the same target, in
    /// one move: the analytic `v + (target - v)(1 - (1 - coeff)^steps)`
    /// instead of the loop.
    ///
    /// The loop injects an f32 rounding error of about 1.5 ulp of the range
    /// per step that the next steps then shrink by `1 - coeff`, so it sits
    /// within `1.5 * 2^-24 * R * min(steps, 1/coeff)` of the exact recurrence
    /// (`R` the range of the quantity: 0.04 degrees of bearing at 48 kHz and
    /// a 50 ms time constant, where `1/coeff` is 2400 steps). This form
    /// carries one rounding of its own, so it is the closer of the two to the
    /// exact answer; the two differ by at most that bound.
    #[inline]
    pub fn advance(&mut self, target: Elevation, steps: usize) -> Elevation {
        if steps > 0 {
            self.value += self.gap_closed(steps) * (target.get() - self.value);
        }
        Elevation(self.value)
    }

    /// `process_angle` `steps` times toward the same
    /// target, in one move along the short arc (see
    /// [`advance`](Self::advance) for the error bound). The short arc to a
    /// target does not change direction as the value closes on it, so one
    /// arc computation stands for all the steps.
    #[inline]
    pub fn advance_angle(&mut self, target: Azimuth, steps: usize) -> Azimuth {
        if steps == 0 {
            return Azimuth(self.value);
        }
        let current = Azimuth(self.value).wrap();
        let arc = current.shortest_arc_to(target.wrap());
        let next = current.rotate_by(arc * self.gap_closed(steps));
        self.value = next.get();
        next
    }

    /// The running value, in whichever space the caller steps it in (see
    /// [`seed_at`](Self::seed_at) for why it is a bare `f32`). Already
    /// wrapped when stepped with `process_angle`.
    #[inline]
    pub fn value(&self) -> f32 {
        self.value
    }

    /// Seeds the running value directly, discarding whatever ramp was in
    /// flight.
    ///
    /// This is the whole of a smoother's runtime state, so seeding it at the
    /// current target is what "has not processed any data" means here — see
    /// [`AngleSmoother::reset_to_target`](crate::AngleSmoother::reset_to_target).
    /// Seeding at `0.0` instead would leave the next block ramping in from
    /// front-centre.
    ///
    /// Bare `f32` on purpose: `value` is the state *both* entry points share —
    /// degrees of bearing under `process_angle`, degrees
    /// of height under `process` — so no single unit describes
    /// it. Callers seed it in whichever space they then step in.
    pub fn seed_at(&mut self, value: f32) {
        self.value = value;
    }
}

#[cfg(test)]
mod tests;
