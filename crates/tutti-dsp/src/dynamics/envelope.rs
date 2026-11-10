//! One-pole envelope followers, shared by every dynamics processor.
//!
//! Two variants because the gain decision differs: `EnvelopeFollower` smooths a
//! continuous level for the compressor and limiter, while
//! `GateEnvelopeFollower` adds the hold stage a gate needs between opening and
//! releasing.

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

/// Gate envelope follower with hold phase.
///
/// Extends `EnvelopeFollower` with a hold counter that keeps the gate open
/// for a specified duration after the signal drops below threshold.
#[derive(Clone, Debug)]
pub struct GateEnvelopeFollower {
    inner: EnvelopeFollower,
    hold_counter: usize,
    hold_samples: usize,
    last_hold: Seconds,
}

impl GateEnvelopeFollower {
    /// A follower at rest with the given times at `sample_rate`.
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
            // a truncating `as usize` would agree, since truncation is floor.
            hold_samples: hold.to_samples_floor(sample_rate).get(),
            last_hold: hold,
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
        if (hold.get() - self.last_hold.get()).abs() > EPSILON {
            self.hold_samples = hold.to_samples_floor(self.inner.sample_rate).get();
            self.last_hold = hold;
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

    /// The smoothed value now.
    #[inline]
    pub fn value(&self) -> f32 {
        self.inner.value()
    }

    /// Returns the follower to rest: value 0, hold cleared.
    pub fn reset(&mut self) {
        self.inner.reset();
        self.hold_counter = 0;
    }

    /// Re-derives the coefficients (and the hold count) for `sample_rate` at the given times.
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
        self.last_hold = hold;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The coefficient is the one-pole formula on the exact time:
    /// `1 - exp(-1 / (0.01 * 48000))` in `f64`, rounded once to `f32`
    /// (0.0020811646). Rounding 0.01 through `f32` first gives
    /// 0.0020811649, a different `f32`.
    ///
    /// Mutation (run): `attack.to_f32()` then `Seconds::from_f32` before
    /// `one_pole_coeff` (the old detour) fails the attack assert.
    #[test]
    fn coefficients_come_from_the_exact_times() {
        let want = |t: f64| (1.0 - libm::exp(-1.0 / (t * 48_000.0))) as f32;
        let mut env = EnvelopeFollower::new(0.01, 0.007, 48_000.0);
        assert_eq!(env.attack_coeff(), want(0.01));
        assert_eq!(env.release_coeff(), want(0.007));
        env.update_coefficients(0.007, 0.01);
        assert_eq!(env.attack_coeff(), want(0.007));
        assert_eq!(env.release_coeff(), want(0.01));
    }

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

    /// **A zero attack or release is instant**: the follower lands on its
    /// target in one sample, rising and falling, and a gate opens and
    /// closes in one. (It used to freeze: the coefficient was the
    /// retention, 1 at zero time, so a zero-attack compressor never
    /// reduced.)
    ///
    /// Mutation (run): `Smoother::toward` with the retention form
    /// `coeff * current + (1 - coeff) * target` (the old convention, under
    /// which a coefficient of 1 holds) → fails.
    #[test]
    fn a_zero_attack_or_release_is_instant() {
        let mut env = EnvelopeFollower::new(0.0, 0.0, 48_000.0);
        assert_eq!(env.smooth(0.7), 0.7);
        assert!((env.smooth(0.2) - 0.2).abs() < 1e-7, "{}", env.value());

        let mut gate = GateEnvelopeFollower::new(0.0, 0.0, 0.0, 48_000.0);
        assert_eq!(gate.step(true), 1.0);
        assert_eq!(gate.step(false), 0.0);
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
