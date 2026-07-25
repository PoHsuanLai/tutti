//! One-pole exponential smoother used to de-zipper the panners' atomic
//! position changes. Private to `spatial` — the only consumer.

use tutti_core::{Azimuth, SampleRate, Seconds};

pub const DEFAULT_POSITION_SMOOTH_TIME: Seconds = Seconds(0.05);

pub struct ExponentialSmoother {
    value: f32,
    coeff: f32,
    smooth_secs: f32,
}

impl ExponentialSmoother {
    pub fn new(smooth_time: impl Into<Seconds>, sample_rate: impl Into<SampleRate>) -> Self {
        let smooth_secs = smooth_time.into().get();
        Self {
            value: 0.0,
            coeff: Self::coeff(smooth_secs, sample_rate.into().get()),
            smooth_secs,
        }
    }

    fn coeff(smooth_secs: f32, sr: f64) -> f32 {
        let coeff = 1.0 - (-1.0 / (smooth_secs as f64 * sr)).exp() as f32;
        coeff.clamp(0.0, 1.0)
    }

    /// Recompute the smoothing coefficient for a new sample rate, holding the
    /// original smoothing time constant. Without this the de-zipper ramp runs
    /// at whatever rate the smoother was built with.
    pub fn set_sample_rate(&mut self, sample_rate: impl Into<SampleRate>) {
        self.coeff = Self::coeff(self.smooth_secs, sample_rate.into().get());
    }

    /// Step toward `target` along the number line.
    ///
    /// Correct for any quantity with two ends and no seam — elevation, gain,
    /// width. **Wrong for a bearing**: see [`process_angle`](Self::process_angle).
    #[inline]
    pub fn process(&mut self, target: f32) -> f32 {
        self.value += self.coeff * (target - self.value);
        self.value
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

    #[allow(dead_code)]
    pub fn reset(&mut self, value: f32) {
        self.value = value;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A smoother that moves the whole way in one step, so the *path* is
    /// visible without waiting out an exponential ramp.
    fn instant() -> ExponentialSmoother {
        let mut s = ExponentialSmoother::new(Seconds(0.05), SampleRate(48_000.0));
        s.coeff = 1.0;
        s
    }

    #[test]
    fn angular_smoothing_crosses_the_seam_instead_of_going_around() {
        let mut s = instant();
        s.reset(170.0);
        // 170 -> -170 is +20 degrees. The linear form would travel -340.
        assert_eq!(s.process_angle(Azimuth(-170.0)), Azimuth(-170.0));
    }

    #[test]
    fn angular_smoothing_takes_the_short_arc_at_partial_coefficient() {
        let mut s = ExponentialSmoother::new(Seconds(0.05), SampleRate(48_000.0));
        s.coeff = 0.5;
        s.reset(170.0);
        // Half of the +20 degree arc lands at 180, not back down near 0.
        let next = s.process_angle(Azimuth(-170.0));
        assert_eq!(next, Azimuth(-180.0));

        // The linear smoother, for contrast, heads the wrong way entirely.
        let mut linear = ExponentialSmoother::new(Seconds(0.05), SampleRate(48_000.0));
        linear.coeff = 0.5;
        linear.reset(170.0);
        assert_eq!(linear.process(-170.0), 0.0);
    }

    #[test]
    fn angular_smoothing_stays_on_the_circle_over_a_long_sweep() {
        let mut s = ExponentialSmoother::new(Seconds(0.05), SampleRate(48_000.0));
        s.coeff = 0.3;
        s.reset(0.0);
        for _ in 0..2_000 {
            let v = s.process_angle(Azimuth(179.0));
            assert!(
                (-180.0..=180.0).contains(&v.get()),
                "bearing {v:?} left the circle"
            );
        }
    }

    #[test]
    fn linear_smoothing_still_converges_for_elevation() {
        // Elevation has no seam, so the plain form remains correct there.
        let mut s = ExponentialSmoother::new(Seconds(0.05), SampleRate(48_000.0));
        s.coeff = 0.5;
        s.reset(0.0);
        assert_eq!(s.process(90.0), 45.0);
        assert_eq!(s.process(90.0), 67.5);
    }
}
