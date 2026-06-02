//! One-pole exponential smoother used to de-zipper atomic parameter changes.
//!
//! Originally lived inside `spatial::utils`; promoted here so filter and
//! modulation nodes can share it without depending on the `spatial` feature.

use tutti_core::{SampleRate, Seconds};

pub const DEFAULT_POSITION_SMOOTH_TIME: Seconds = Seconds(0.05);

pub struct ExponentialSmoother {
    value: f32,
    coeff: f32,
}

impl ExponentialSmoother {
    pub fn new(smooth_time: impl Into<Seconds>, sample_rate: impl Into<SampleRate>) -> Self {
        let smooth_secs = smooth_time.into().get();
        let sr = sample_rate.into().get();
        let coeff = 1.0 - (-1.0 / (smooth_secs as f64 * sr)).exp() as f32;
        Self {
            value: 0.0,
            coeff: coeff.clamp(0.0, 1.0),
        }
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
