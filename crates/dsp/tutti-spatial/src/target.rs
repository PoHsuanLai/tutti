//! Where a source is aimed, and how it gets there without zippering.
//! Algorithm-neutral: [`SpatialTarget`] is the control→RT storage,
//! [`AngleSmoother`] the de-zipper both panners step per frame.

use tutti_core::{Azimuth, Elevation, Param, SampleRate};

use crate::smoothing::{ExponentialSmoother, DEFAULT_POSITION_SMOOTH_TIME};

/// Azimuth/elevation pair as lock-free params.
///
/// The two fields are different types on purpose: a bearing wraps (190° is 170°
/// to the right), a height saturates (past straight up, you stop).
#[derive(Clone)]
pub struct SpatialTarget {
    pub azimuth: Param<Azimuth>,
    pub elevation: Param<Elevation>,
}

impl SpatialTarget {
    pub fn new() -> Self {
        Self {
            azimuth: Param::new(Azimuth::FRONT),
            elevation: Param::new(Elevation::LEVEL),
        }
    }

    /// The typed pair. Typed rather than raw floats: both panners rebuild the
    /// newtypes immediately, and that round trip is where two bugs lived.
    #[inline]
    pub fn load(&self) -> (Azimuth, Elevation) {
        (self.azimuth.load(), self.elevation.load())
    }

    /// Store a pair, normalized on the way in: the bearing wraps, the height
    /// clamps. Taking newtypes is what makes the two unswappable.
    pub fn store(&self, azimuth: impl Into<Azimuth>, elevation: impl Into<Elevation>) {
        self.azimuth.store(azimuth.into().wrap());
        self.elevation
            .store(Elevation::new_clamped(elevation.into().get()));
    }

    pub fn reset_origin(&self) {
        self.store(Azimuth::FRONT, Elevation::LEVEL);
    }
}

impl Default for SpatialTarget {
    fn default() -> Self {
        Self::new()
    }
}

/// One smoother per coordinate, each with its own arithmetic: the bearing takes
/// the short arc, the height is a plain ramp.
///
/// Shared because that asymmetry is easy to get wrong twice. Both used the
/// linear form once, and a source crossing behind the listener (170° → -170°, a
/// 20° move) swept 340° the wrong way around the head.
pub(crate) struct AngleSmoother {
    azimuth: ExponentialSmoother,
    elevation: ExponentialSmoother,
}

impl AngleSmoother {
    pub(crate) fn new(sample_rate: impl Into<SampleRate>) -> Self {
        let sample_rate = sample_rate.into();
        Self {
            azimuth: ExponentialSmoother::new(DEFAULT_POSITION_SMOOTH_TIME, sample_rate),
            elevation: ExponentialSmoother::new(DEFAULT_POSITION_SMOOTH_TIME, sample_rate),
        }
    }

    /// Retune so the 50 ms ramp holds at any sample rate.
    pub(crate) fn set_sample_rate(&mut self, sample_rate: impl Into<SampleRate>) {
        let sample_rate = sample_rate.into();
        self.azimuth.set_sample_rate(sample_rate);
        self.elevation.set_sample_rate(sample_rate);
    }

    /// Advance one step toward the target and return the smoothed pair.
    #[inline]
    pub(crate) fn step(&mut self, azimuth: Azimuth, elevation: Elevation) -> (Azimuth, Elevation) {
        (
            self.azimuth.process_angle(azimuth),
            self.elevation.process(elevation),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bearing crossing the ±180 seam must take the short arc. Asserted as a
    /// direction of travel — the exact per-step position is the smoother's.
    #[test]
    fn bearing_crosses_the_seam_the_short_way() {
        let mut s = AngleSmoother::new(SampleRate(48_000.0));
        // Settle at 170 degrees.
        for _ in 0..48_000 {
            s.step(Azimuth(170.0), Elevation::LEVEL);
        }
        let (start, _) = s.step(Azimuth(170.0), Elevation::LEVEL);
        assert!(
            (start.get() - 170.0).abs() < 1.0,
            "should have settled near 170, got {}",
            start.get()
        );

        // Now aim at -170: 20 degrees away across the seam, NOT 340 back
        // through zero. One step must move the bearing UP past 180 (which
        // wraps to a negative value) rather than down toward 160.
        let (next, _) = s.step(Azimuth(-170.0), Elevation::LEVEL);
        let moved_down_through_zero = next.get() < start.get() && next.get() > 0.0;
        assert!(
            !moved_down_through_zero,
            "bearing took the long way: 170 -> {} (should cross the seam)",
            next.get()
        );
    }

    /// The counterpart: elevation saturates rather than wrapping.
    #[test]
    fn height_saturates_and_does_not_wrap() {
        let mut s = AngleSmoother::new(SampleRate(48_000.0));
        for _ in 0..48_000 {
            s.step(Azimuth::FRONT, Elevation(90.0));
        }
        let (_, el) = s.step(Azimuth::FRONT, Elevation(90.0));
        assert!(
            el.get() > 80.0,
            "elevation should settle near the pole, got {}",
            el.get()
        );
    }

    /// Otherwise every static source sits slightly off-axis forever.
    #[test]
    fn settles_on_a_stationary_target() {
        let mut s = AngleSmoother::new(SampleRate(48_000.0));
        for _ in 0..48_000 {
            s.step(Azimuth(45.0), Elevation(30.0));
        }
        let (az, el) = s.step(Azimuth(45.0), Elevation(30.0));
        assert!((az.get() - 45.0).abs() < 0.5, "azimuth {}", az.get());
        assert!((el.get() - 30.0).abs() < 0.5, "elevation {}", el.get());
    }
}
