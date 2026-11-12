//! The one-pole envelope follower of the dynamics processors that still hold their
//! state in a kernel (the limiter, the multiband's bands). The families
//! (`CompressorFamily`, `GateFamily`) run their follower in their recurrence.

use crate::smoother::Smoother;
use tutti_types::{SampleRate, Seconds};

/// A time that moved by less than this (seconds) keeps its coefficient.
const EPSILON: f64 = 0.00001;

/// One-pole envelope follower with attack/release smoothing and lazy coefficient caching.
///
/// Encapsulates the stateful envelope tracking that all dynamics processors share:
/// attack/release coefficients, lazy recalculation, and one-pole IIR smoothing.
#[derive(Clone, Debug)]
pub struct EnvelopeFollower {
    value: f32,
    attack_coeff: f32,
    release_coeff: f32,
    last_attack: Seconds,
    last_release: Seconds,
    sample_rate: SampleRate,
}

impl EnvelopeFollower {
    /// A follower at rest with the given times at `sample_rate`.
    pub fn new(
        attack: impl Into<Seconds>,
        release: impl Into<Seconds>,
        sample_rate: impl Into<SampleRate>,
    ) -> Self {
        let attack = attack.into();
        let release = release.into();
        let sample_rate = sample_rate.into();
        Self {
            value: 0.0,
            attack_coeff: attack.one_pole_coeff(sample_rate),
            release_coeff: release.one_pole_coeff(sample_rate),
            last_attack: attack,
            last_release: release,
            sample_rate,
        }
    }

    /// Lazily update coefficients if attack/release params changed.
    #[inline]
    pub fn update_coefficients(&mut self, attack: impl Into<Seconds>, release: impl Into<Seconds>) {
        let attack = attack.into();
        let release = release.into();
        if (attack.get() - self.last_attack.get()).abs() > EPSILON {
            self.attack_coeff = attack.one_pole_coeff(self.sample_rate);
            self.last_attack = attack;
        }
        if (release.get() - self.last_release.get()).abs() > EPSILON {
            self.release_coeff = release.one_pole_coeff(self.sample_rate);
            self.last_release = release;
        }
    }

    /// Smooths toward target using attack (target > current) or release (target < current).
    #[inline]
    pub fn smooth(&mut self, target: f32) -> f32 {
        let coeff = if target > self.value {
            self.attack_coeff
        } else {
            self.release_coeff
        };
        self.value = Smoother::toward(self.value, target, coeff);
        self.value
    }

    /// Smooths with explicit attack coeff (for cases where direction logic differs).
    #[inline]
    pub fn smooth_with_coeff(&mut self, target: f32, coeff: f32) -> f32 {
        self.value = Smoother::toward(self.value, target, coeff);
        self.value
    }

    /// The one-pole coefficient used while the target rises.
    #[inline]
    pub fn attack_coeff(&self) -> f32 {
        self.attack_coeff
    }

    /// The one-pole coefficient used while the target falls.
    #[inline]
    pub fn release_coeff(&self) -> f32 {
        self.release_coeff
    }

    /// The smoothed value now.
    #[inline]
    pub fn value(&self) -> f32 {
        self.value
    }

    /// Sets the value outright.
    #[cfg(test)]
    pub fn set_value(&mut self, value: f32) {
        self.value = value;
    }

    /// Returns the follower to rest: value 0, hold cleared.
    pub fn reset(&mut self) {
        self.value = 0.0;
    }

    /// Re-derives the coefficients (and the hold count) for `sample_rate` at the given times.
    pub fn set_sample_rate(
        &mut self,
        sample_rate: impl Into<SampleRate>,
        attack: impl Into<Seconds>,
        release: impl Into<Seconds>,
    ) {
        let sample_rate = sample_rate.into();
        let attack = attack.into();
        let release = release.into();
        self.sample_rate = sample_rate;
        self.attack_coeff = attack.one_pole_coeff(sample_rate);
        self.release_coeff = release.one_pole_coeff(sample_rate);
        self.last_attack = attack;
        self.last_release = release;
    }
}

#[cfg(test)]
mod tests;
