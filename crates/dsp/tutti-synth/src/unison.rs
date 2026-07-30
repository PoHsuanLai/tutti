//! Unison engine: voice detuning and stereo spread.

use tutti_core::Cents;

// The modulatable-param plumbing (`Param`/atomics) is only used by
// `UnisonEngine`, which is itself gated on `midi`/`test`.
#[cfg(any(feature = "midi", test))]
use alloc::sync::Arc;
#[cfg(any(feature = "midi", test))]
use tutti_core::{AtomicF32, Param, Spread};

#[cfg(any(feature = "midi", test))]
extern crate alloc;

const MAX_UNISON_VOICES: usize = 16;

#[derive(Debug, Clone)]
pub struct UnisonConfig {
    /// 1-16
    pub voice_count: u8,
    /// Total spread (not per-voice)
    pub detune_cents: Cents,
    /// 0.0 = mono, 1.0 = full stereo
    pub stereo_spread: Spread,
    pub phase_randomize: bool,
}

impl Default for UnisonConfig {
    fn default() -> Self {
        Self {
            voice_count: 1,
            detune_cents: Cents(0.0),
            stereo_spread: Spread::POINT,
            phase_randomize: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct UnisonVoiceParams {
    /// 1.0 = center pitch
    pub freq_ratio: f32,
    /// -1.0 = left, 1.0 = right
    pub pan: f32,
    /// 0.0 to 1.0
    pub phase_offset: f32,
    pub amplitude: f32,
}

#[cfg(any(feature = "midi", test))]
#[derive(Debug, Clone)]
pub struct UnisonEngine {
    config: UnisonConfig,
    voices: [UnisonVoiceParams; MAX_UNISON_VOICES],
    rng_state: u32,
    /// Control-rate-modulatable mirrors of `config.detune_cents` /
    /// `config.stereo_spread`. A modulator writes these (via
    /// [`ModParams`](crate::ModParams)); [`sync_from_atomics`](Self::sync_from_atomics),
    /// called once per block, folds any change back into `config` + a
    /// recompute. Detune/spread only affect per-voice params on recompute (not
    /// per-sample), so a block-rate sync is exact.
    detune: Param<Cents>,
    spread: Param<Spread>,
}

#[cfg(any(feature = "midi", test))]
impl UnisonEngine {
    pub fn new(config: UnisonConfig) -> Self {
        let detune = Param::new(config.detune_cents);
        let spread = Param::new(config.stereo_spread);
        let mut engine = Self {
            config,
            voices: [UnisonVoiceParams::default(); MAX_UNISON_VOICES],
            rng_state: 12345,
            detune,
            spread,
        };
        engine.recompute_params();
        engine
    }

    /// The shared detune atomic (cents), for control-rate modulation.
    pub fn detune_atomic(&self) -> Arc<AtomicF32> {
        self.detune.as_atomic()
    }

    /// The shared stereo-spread atomic (0..1), for control-rate modulation.
    pub fn spread_atomic(&self) -> Arc<AtomicF32> {
        self.spread.as_atomic()
    }

    /// Fold any control-rate change to the detune/spread atomics back into
    /// `config` and recompute per-voice params. Called once per block. Cheap
    /// when nothing moved (compares against the current config first).
    pub fn sync_from_atomics(&mut self) {
        let detune = self.detune.load();
        let spread = Spread::new_clamped(self.spread.load().get());
        let changed = (detune.get() - self.config.detune_cents.get()).abs() > f32::EPSILON
            || (spread.get() - self.config.stereo_spread.get()).abs() > f32::EPSILON;
        if changed {
            self.config.detune_cents = Cents(detune.get().max(0.0));
            self.config.stereo_spread = spread;
            self.recompute_params();
        }
    }

    pub fn recompute_params(&mut self) {
        let count = usize::from(self.config.voice_count).clamp(1, MAX_UNISON_VOICES);

        let amplitude = 1.0 / (count as f32).sqrt();
        // A converter, not a divide: `Cents` is `unit_scalable!`, so
        // `detune_cents / 100.0` would compile and hand back `Cents` — wrong by
        // 100x, with a type that says it is fine.
        let detune_semitones = self.config.detune_cents.to_semitones();

        for i in 0..count {
            let position = if count == 1 {
                0.0
            } else {
                (i as f32 / (count - 1) as f32) * 2.0 - 1.0
            };

            // `Semitones * f32` is opted in, so spreading the detune across the
            // voice's position stays in the unit, and the exponent conversion
            // happens once at the end.
            let freq_ratio = (detune_semitones * position).to_pitch_ratio();
            let pan = position * self.config.stereo_spread.get();

            self.voices[i] = UnisonVoiceParams {
                freq_ratio,
                pan,
                phase_offset: 0.0,
                amplitude,
            };
        }

        for i in count..MAX_UNISON_VOICES {
            self.voices[i] = UnisonVoiceParams::default();
        }
    }

    pub fn randomize_phases(&mut self) {
        if !self.config.phase_randomize {
            return;
        }

        let count = usize::from(self.config.voice_count).clamp(1, MAX_UNISON_VOICES);

        for i in 0..count {
            self.rng_state ^= self.rng_state << 13;
            self.rng_state ^= self.rng_state >> 17;
            self.rng_state ^= self.rng_state << 5;

            self.voices[i].phase_offset = (self.rng_state as f32) / (u32::MAX as f32);
        }
    }

    #[inline]
    pub fn voice_count(&self) -> usize {
        usize::from(self.config.voice_count).clamp(1, MAX_UNISON_VOICES)
    }

    #[inline]
    pub fn voice_params(&self, index: usize) -> &UnisonVoiceParams {
        &self.voices[index.min(MAX_UNISON_VOICES - 1)]
    }

    #[inline]
    pub fn all_params(&self) -> &[UnisonVoiceParams] {
        &self.voices[..self.voice_count()]
    }

    pub fn set_config(&mut self, config: UnisonConfig) {
        self.detune.store(config.detune_cents);
        self.spread.store(config.stereo_spread);
        self.config = config;
        self.recompute_params();
    }

    pub fn config(&self) -> &UnisonConfig {
        &self.config
    }

    pub fn set_voice_count(&mut self, count: u8) {
        self.config.voice_count = count.clamp(1, MAX_UNISON_VOICES as u8);
        self.recompute_params();
    }

    pub fn set_detune(&mut self, cents: impl Into<Cents>) {
        self.config.detune_cents = Cents(cents.into().get().max(0.0));
        self.detune.store(self.config.detune_cents);
        self.recompute_params();
    }

    pub fn set_stereo_spread(&mut self, spread: f32) {
        // `new_clamped` rather than an open-coded `clamp(0.0, 1.0)` followed
        // by the unchecked `Spread(..)` — the constructor is the same bound,
        // and it was already being used twelve lines up.
        self.config.stereo_spread = Spread::new_clamped(spread);
        self.spread.store(self.config.stereo_spread);
        self.recompute_params();
    }

    pub fn seed_rng(&mut self, seed: u32) {
        self.rng_state = if seed == 0 { 1 } else { seed };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_single_voice() {
        let config = UnisonConfig {
            voice_count: 1,
            ..Default::default()
        };
        let unison = UnisonEngine::new(config);

        assert_eq!(unison.voice_count(), 1);

        let params = unison.voice_params(0);
        assert!((params.freq_ratio - 1.0).abs() < 0.001);
        assert!((params.pan - 0.0).abs() < 0.001);
        assert!((params.amplitude - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_detune_spread() {
        let config = UnisonConfig {
            voice_count: 3,
            detune_cents: Cents(12.0), // ~1/8 semitone spread
            stereo_spread: Spread::POINT,
            phase_randomize: false,
        };
        let unison = UnisonEngine::new(config);

        assert_eq!(unison.voice_count(), 3);

        // Center voice should be at ratio 1.0
        let center = unison.voice_params(1);
        assert!((center.freq_ratio - 1.0).abs() < 0.001);

        // First voice should be detuned down
        let low = unison.voice_params(0);
        assert!(low.freq_ratio < 1.0);

        // Last voice should be detuned up
        let high = unison.voice_params(2);
        assert!(high.freq_ratio > 1.0);

        // Symmetric detune
        let low_ratio = 1.0 / low.freq_ratio;
        let high_ratio = high.freq_ratio;
        assert!((low_ratio - high_ratio).abs() < 0.001);
    }

    #[test]
    fn test_stereo_spread() {
        let config = UnisonConfig {
            voice_count: 3,
            detune_cents: Cents(0.0),
            stereo_spread: Spread(1.0),
            phase_randomize: false,
        };
        let unison = UnisonEngine::new(config);

        let left = unison.voice_params(0);
        let center = unison.voice_params(1);
        let right = unison.voice_params(2);

        assert!((left.pan - (-1.0)).abs() < 0.001);
        assert!((center.pan - 0.0).abs() < 0.001);
        assert!((right.pan - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_equal_power_amplitude() {
        for count in 1..=8 {
            let config = UnisonConfig {
                voice_count: count,
                ..Default::default()
            };
            let unison = UnisonEngine::new(config);

            // Sum of squared amplitudes should equal 1.0 (equal power)
            let sum_sq: f32 = unison
                .all_params()
                .iter()
                .map(|p| p.amplitude * p.amplitude)
                .sum();

            assert!(
                (sum_sq - 1.0).abs() < 0.01,
                "Equal power failed for {} voices: {}",
                count,
                sum_sq
            );
        }
    }

    #[test]
    fn test_phase_randomization() {
        let config = UnisonConfig {
            voice_count: 4,
            phase_randomize: true,
            ..Default::default()
        };
        let mut unison = UnisonEngine::new(config);

        // Seed for reproducibility
        unison.seed_rng(42);
        unison.randomize_phases();

        // Phases should be different
        let phases: Vec<f32> = unison.all_params().iter().map(|p| p.phase_offset).collect();

        // Check phases are in valid range
        for phase in &phases {
            assert!(*phase >= 0.0 && *phase <= 1.0);
        }

        // Check they're not all the same
        let first = phases[0];
        let all_same = phases.iter().all(|p| (*p - first).abs() < 0.001);
        assert!(!all_same, "Phases should be randomized");
    }

    #[test]
    fn test_config_update() {
        let mut unison = UnisonEngine::new(UnisonConfig::default());

        assert_eq!(unison.voice_count(), 1);

        unison.set_voice_count(5);
        assert_eq!(unison.voice_count(), 5);

        unison.set_detune(20.0);
        assert!((unison.config().detune_cents.get() - 20.0).abs() < 0.001);

        unison.set_stereo_spread(0.5);
        assert!((unison.config().stereo_spread.get() - 0.5).abs() < 0.001);
    }

    #[test]
    fn test_clamp_voice_count() {
        let config = UnisonConfig {
            voice_count: 100, // Over max
            ..Default::default()
        };
        let unison = UnisonEngine::new(config);

        assert_eq!(unison.voice_count(), MAX_UNISON_VOICES);
    }
}
