//! One-pole exponential smoother used to de-zipper the panners' atomic
//! position changes. Private to `spatial` — the only consumer.

use tutti_core::{SampleRate, Seconds};

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

    #[inline]
    pub fn process(&mut self, target: f32) -> f32 {
        self.value += self.coeff * (target - self.value);
        self.value
    }

    #[allow(dead_code)]
    pub fn reset(&mut self, value: f32) {
        self.value = value;
    }
}
