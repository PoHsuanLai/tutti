//! One-pole exponential smoother that de-zippers the panners' position
//! changes.

use tutti_core::{Azimuth, Elevation, SampleRate, Samples, Seconds};

pub const DEFAULT_POSITION_SMOOTH_TIME: Seconds = Seconds(0.05);

#[derive(Clone)]
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
        let smooth_time = smooth_time.into();
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
    /// bearing**: see [`process_angle`](Self::process_angle). The pair is the
    /// point — one entry per space, each typed, so a bearing cannot reach the
    /// linear form by accident.
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

    /// [`process`](Self::process) `steps` times toward the same target, in
    /// one move: the analytic `v + (target - v)(1 - (1 - coeff)^steps)`
    /// instead of the loop.
    ///
    /// The loop injects an f32 rounding error of about 1.5 ulp of the range
    /// per step that the next steps then shrink by `1 - coeff`, so it sits
    /// within `1.5 * 2^-24 * R * min(steps, 1/coeff)` of the exact recurrence
    /// (`R` the range of the quantity: 0.03 degrees of bearing at 48 kHz and
    /// a 50 ms time constant, where `1/coeff` is 2400 steps). This form carries one rounding of its own,
    /// so it is the closer of the two to the exact answer; the two differ by
    /// at most that bound.
    #[inline]
    pub fn advance(&mut self, target: Elevation, steps: usize) -> Elevation {
        if steps > 0 {
            self.value += self.gap_closed(steps) * (target.get() - self.value);
        }
        Elevation(self.value)
    }

    /// [`process_angle`](Self::process_angle) `steps` times toward the same
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
    /// wrapped when stepped with [`process_angle`](Self::process_angle).
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
    /// degrees of bearing under [`process_angle`](Self::process_angle), degrees
    /// of height under [`process`](Self::process) — so no single unit describes
    /// it. Callers seed it in whichever space they then step in.
    pub fn seed_at(&mut self, value: f32) {
        self.value = value;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A smoother that moves the whole way in one step, so the *path* is
    /// visible without waiting out an exponential ramp.
    fn instant() -> ExponentialSmoother {
        let mut s = ExponentialSmoother::new(Seconds(0.05), SampleRate(48_000.0), Samples(1));
        s.coeff = 1.0;
        s
    }

    #[test]
    fn angular_smoothing_crosses_the_seam_instead_of_going_around() {
        let mut s = instant();
        s.seed_at(170.0);
        // 170 -> -170 is +20 degrees. The linear form would travel -340.
        assert_eq!(s.process_angle(Azimuth(-170.0)), Azimuth(-170.0));
    }

    #[test]
    fn angular_smoothing_takes_the_short_arc_at_partial_coefficient() {
        let mut s = ExponentialSmoother::new(Seconds(0.05), SampleRate(48_000.0), Samples(1));
        s.coeff = 0.5;
        s.seed_at(170.0);
        // Half of the +20 degree arc lands at 180, not back down near 0.
        let next = s.process_angle(Azimuth(-170.0));
        assert_eq!(next, Azimuth(-180.0));

        // The linear smoother, for contrast, heads the wrong way entirely.
        let mut linear = ExponentialSmoother::new(Seconds(0.05), SampleRate(48_000.0), Samples(1));
        linear.coeff = 0.5;
        linear.seed_at(170.0);
        assert_eq!(linear.process(Elevation(-170.0)), Elevation(0.0));
    }

    #[test]
    fn angular_smoothing_stays_on_the_circle_over_a_long_sweep() {
        let mut s = ExponentialSmoother::new(Seconds(0.05), SampleRate(48_000.0), Samples(1));
        s.coeff = 0.3;
        s.seed_at(0.0);
        for _ in 0..2_000 {
            let v = s.process_angle(Azimuth(179.0));
            assert!(
                (-180.0..=180.0).contains(&v.get()),
                "bearing {v:?} left the circle"
            );
        }
    }

    /// **A zero smoothing time is instant**, stepped per sample or per
    /// frame: the first step lands on the target, in either space. A
    /// negative time is instant too (it used to clamp to a frozen 0).
    ///
    /// Mutation (run): `coeff` returning 0 for a non-positive time (the
    /// freeze) → fails.
    #[test]
    fn a_zero_smoothing_time_is_instant() {
        for tau in [0.0, -0.05] {
            for stride in [Samples(1), Samples(64)] {
                let mut s = ExponentialSmoother::new(Seconds(tau), SampleRate(48_000.0), stride);
                s.seed_at(10.0);
                assert_eq!(s.process(Elevation(40.0)), Elevation(40.0));
                s.seed_at(170.0);
                assert_eq!(s.process_angle(Azimuth(-170.0)), Azimuth(-170.0));
            }
        }
    }

    #[test]
    fn linear_smoothing_still_converges_for_elevation() {
        // Elevation has no seam, so the plain form remains correct there.
        let mut s = ExponentialSmoother::new(Seconds(0.05), SampleRate(48_000.0), Samples(1));
        s.coeff = 0.5;
        s.seed_at(0.0);
        assert_eq!(s.process(Elevation(90.0)), Elevation(45.0));
        assert_eq!(s.process(Elevation(90.0)), Elevation(67.5));
    }

    /// **`advance` is the closed form of `steps` steps**: from a gap `g`
    /// the remainder is `g * (1 - coeff)^steps`, for a height and for a
    /// bearing across the seam, within the bound `advance` documents; zero
    /// steps move nothing and an instant smoother lands on the target.
    ///
    /// Mutations (each run, each fails): the retention `1 - coeff`
    /// replaced by `coeff` in `gap_closed`; `advance_angle` taking the arc
    /// as `target - current`.
    #[test]
    fn advance_is_the_closed_form_of_repeated_steps() {
        for steps in [1usize, 64, 1024, 48_000] {
            let mut s = ExponentialSmoother::new(Seconds(0.05), SampleRate(48_000.0), Samples(1));
            let coeff = f64::from(s.coeff);
            let left = (1.0 - coeff).powi(steps as i32);
            s.seed_at(10.0);
            let h = s.advance(Elevation(70.0), steps).get();
            assert!(
                (f64::from(h) - (70.0 - 60.0 * left)).abs() < 1e-3,
                "{steps}: {h}"
            );
            // 170 toward -170: a +20 degree arc, remainder `20 * left`.
            s.seed_at(170.0);
            let b = s.advance_angle(Azimuth(-170.0), steps).get();
            let want = Azimuth((170.0 + 20.0 * (1.0 - left)) as f32).wrap().get();
            let err = Azimuth(b).shortest_arc_to(Azimuth(want)).get().abs();
            assert!(err < 1e-3, "{steps}: {b} vs {want}");
        }
        let mut s = ExponentialSmoother::new(Seconds(0.05), SampleRate(48_000.0), Samples(1));
        s.seed_at(10.0);
        assert_eq!(s.advance(Elevation(70.0), 0), Elevation(10.0));
        assert_eq!(s.advance_angle(Azimuth(-170.0), 0), Azimuth(10.0));
        let mut i = instant();
        i.seed_at(10.0);
        assert_eq!(i.advance(Elevation(70.0), 1), Elevation(70.0));
        i.seed_at(170.0);
        assert_eq!(i.advance_angle(Azimuth(-170.0), 5), Azimuth(-170.0));
    }
}
