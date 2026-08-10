//! One-pole envelope followers, shared by every dynamics processor.
//!
//! Two variants because the gain decision differs: `EnvelopeFollower` smooths a
//! continuous level for the compressor and limiter, while
//! `GateEnvelopeFollower` adds the hold stage a gate needs between opening and
//! releasing.

use super::utils::{smooth_envelope, time_to_coeff};
use tutti_core::{SampleRate, Seconds};

const EPSILON: f32 = 0.00001;

/// One-pole envelope follower with attack/release smoothing and lazy coefficient caching.
///
/// Encapsulates the stateful envelope tracking that all dynamics processors share:
/// attack/release coefficients, lazy recalculation, and one-pole IIR smoothing.
#[derive(Clone)]
pub(crate) struct EnvelopeFollower {
    value: f32,
    attack_coeff: f32,
    release_coeff: f32,
    last_attack: f32,
    last_release: f32,
    sample_rate: SampleRate,
}

impl EnvelopeFollower {
    pub fn new(
        attack: impl Into<Seconds>,
        release: impl Into<Seconds>,
        sample_rate: impl Into<SampleRate>,
    ) -> Self {
        let attack = attack.into().get();
        let release = release.into().get();
        let sample_rate = sample_rate.into();
        Self {
            value: 0.0,
            attack_coeff: time_to_coeff(attack, sample_rate),
            release_coeff: time_to_coeff(release, sample_rate),
            last_attack: attack,
            last_release: release,
            sample_rate,
        }
    }

    /// Lazily update coefficients if attack/release params changed.
    #[inline]
    pub fn update_coefficients(&mut self, attack: impl Into<Seconds>, release: impl Into<Seconds>) {
        let attack = attack.into().get();
        let release = release.into().get();
        if (attack - self.last_attack).abs() > EPSILON {
            self.attack_coeff = time_to_coeff(attack, self.sample_rate);
            self.last_attack = attack;
        }
        if (release - self.last_release).abs() > EPSILON {
            self.release_coeff = time_to_coeff(release, self.sample_rate);
            self.last_release = release;
        }
    }

    /// Smooth toward target using attack (target > current) or release (target < current).
    #[inline]
    pub fn smooth(&mut self, target: f32) -> f32 {
        let coeff = if target > self.value {
            self.attack_coeff
        } else {
            self.release_coeff
        };
        self.value = smooth_envelope(self.value, target, coeff);
        self.value
    }

    /// Smooth with explicit attack coeff (for cases where direction logic differs).
    #[inline]
    pub fn smooth_with_coeff(&mut self, target: f32, coeff: f32) -> f32 {
        self.value = smooth_envelope(self.value, target, coeff);
        self.value
    }

    #[inline]
    pub fn attack_coeff(&self) -> f32 {
        self.attack_coeff
    }

    #[inline]
    pub fn release_coeff(&self) -> f32 {
        self.release_coeff
    }

    #[inline]
    pub fn value(&self) -> f32 {
        self.value
    }

    #[cfg(test)]
    pub fn set_value(&mut self, value: f32) {
        self.value = value;
    }

    pub fn reset(&mut self) {
        self.value = 0.0;
    }

    pub fn set_sample_rate(
        &mut self,
        sample_rate: impl Into<SampleRate>,
        attack: impl Into<Seconds>,
        release: impl Into<Seconds>,
    ) {
        let sample_rate = sample_rate.into();
        let attack = attack.into().get();
        let release = release.into().get();
        self.sample_rate = sample_rate;
        self.attack_coeff = time_to_coeff(attack, sample_rate);
        self.release_coeff = time_to_coeff(release, sample_rate);
        self.last_attack = attack;
        self.last_release = release;
    }
}

/// Gate envelope follower with hold phase.
///
/// Extends `EnvelopeFollower` with a hold counter that keeps the gate open
/// for a specified duration after the signal drops below threshold.
#[derive(Clone)]
pub(crate) struct GateEnvelopeFollower {
    inner: EnvelopeFollower,
    hold_counter: usize,
    hold_samples: usize,
    last_hold: f32,
}

impl GateEnvelopeFollower {
    pub fn new(
        attack: impl Into<Seconds>,
        hold: impl Into<Seconds>,
        release: impl Into<Seconds>,
        sample_rate: impl Into<SampleRate>,
    ) -> Self {
        let attack = attack.into();
        let hold = hold.into();
        let release = release.into();
        let sample_rate = sample_rate.into();
        Self {
            inner: EnvelopeFollower::new(attack, release, sample_rate),
            hold_counter: 0,
            // `_floor`: a hold is a countdown of whole elapsed frames, and
            // the old `as usize` truncated, which is floor.
            hold_samples: hold.to_samples_floor(sample_rate).get(),
            last_hold: hold.get(),
        }
    }

    /// Lazily update coefficients if attack/hold/release params changed.
    #[inline]
    pub fn update_coefficients(
        &mut self,
        attack: impl Into<Seconds>,
        hold: impl Into<Seconds>,
        release: impl Into<Seconds>,
    ) {
        let hold = hold.into();
        self.inner.update_coefficients(attack, release);
        if (hold.get() - self.last_hold).abs() > EPSILON {
            self.hold_samples = hold.to_samples_floor(self.inner.sample_rate).get();
            self.last_hold = hold.get();
        }
    }

    /// Step the gate envelope: open (attack toward 1.0), hold, or release (toward 0.0).
    #[inline]
    pub fn step(&mut self, gate_open: bool) -> f32 {
        if gate_open {
            self.hold_counter = self.hold_samples;
            self.inner.smooth_with_coeff(1.0, self.inner.attack_coeff())
        } else if self.hold_counter > 0 {
            self.hold_counter -= 1;
            self.inner.value()
        } else {
            self.inner
                .smooth_with_coeff(0.0, self.inner.release_coeff())
        }
    }

    #[inline]
    pub fn value(&self) -> f32 {
        self.inner.value()
    }

    pub fn reset(&mut self) {
        self.inner.reset();
        self.hold_counter = 0;
    }

    pub fn set_sample_rate(
        &mut self,
        sample_rate: impl Into<SampleRate>,
        attack: impl Into<Seconds>,
        hold: impl Into<Seconds>,
        release: impl Into<Seconds>,
    ) {
        let sample_rate = sample_rate.into();
        let attack = attack.into();
        let hold = hold.into();
        let release = release.into();
        self.inner.set_sample_rate(sample_rate, attack, release);
        self.hold_samples = hold.to_samples_floor(sample_rate).get();
        self.last_hold = hold.get();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_envelope_follower_attack() {
        let mut env = EnvelopeFollower::new(0.001, 0.1, 44100.0);
        for _ in 0..200 {
            env.smooth(1.0);
        }
        assert!(
            env.value() > 0.9,
            "Should converge toward 1.0: {}",
            env.value()
        );
    }

    #[test]
    fn test_envelope_follower_release() {
        let mut env = EnvelopeFollower::new(0.001, 0.01, 44100.0);
        env.set_value(1.0);
        for _ in 0..3000 {
            env.smooth(0.0);
        }
        assert!(
            env.value() < 0.1,
            "Should converge toward 0.0: {}",
            env.value()
        );
    }

    #[test]
    fn test_envelope_follower_reset() {
        let mut env = EnvelopeFollower::new(0.001, 0.1, 44100.0);
        env.set_value(0.8);
        env.reset();
        assert_eq!(env.value(), 0.0);
    }

    #[test]
    fn test_envelope_follower_lazy_coefficients() {
        let mut env = EnvelopeFollower::new(0.001, 0.1, 44100.0);
        let orig_attack = env.attack_coeff();
        // Same values: no change
        env.update_coefficients(0.001, 0.1);
        assert_eq!(env.attack_coeff(), orig_attack);
        // Different values: recalculate
        env.update_coefficients(0.01, 0.1);
        assert_ne!(env.attack_coeff(), orig_attack);
    }

    #[test]
    fn test_gate_envelope_hold_phase() {
        let mut gate = GateEnvelopeFollower::new(0.0001, 0.01, 0.001, 44100.0);
        // Open the gate
        for _ in 0..100 {
            gate.step(true);
        }
        assert!(gate.value() > 0.9);

        // Close the gate — should hold first
        gate.step(false);
        assert!(gate.value() > 0.9, "During hold, level should stay high");

        // Exhaust hold, then release
        for _ in 0..1000 {
            gate.step(false);
        }
        assert!(
            gate.value() < 0.1,
            "After hold+release, level should be low: {}",
            gate.value()
        );
    }

    #[test]
    fn test_gate_envelope_reset() {
        let mut gate = GateEnvelopeFollower::new(0.001, 0.01, 0.1, 44100.0);
        for _ in 0..100 {
            gate.step(true);
        }
        gate.reset();
        assert_eq!(gate.value(), 0.0);
    }
}
