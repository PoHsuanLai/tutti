//! Unison: the detune ratios, pans, phases and gains for the stack of
//! sub-voices that sound one note.
//!
//! Pure derivation — this module computes per-sub-voice parameters and never
//! touches a sample. `crate::synth_voice` reads them when it renders. Detune
//! and spread are recomputed only when they change, so the audio thread does an
//! array index rather than the trigonometry.

use std::sync::Arc;
use tutti_types::{Amplitude, AtomicF32, Cents, Pan, Param, Phase, Spread};

pub(crate) const MAX_UNISON_VOICES: usize = 16;

/// Stacked, detuned copies of a single note — the "supersaw" thickener.
///
/// Every field here multiplies the cost of *each* voice: a synth with
/// `max_voices: 8` and `voice_count: 7` runs 56 oscillator lanes. The
/// sub-voices are summed to a constant-power total, so raising
/// [`voice_count`](Self::voice_count) thickens the tone without raising the
/// level.
#[derive(Debug, Clone)]
pub struct UnisonConfig {
    /// Sub-voices per note, 1..=16 (clamped, not rejected). `1` is plain
    /// unison-off and the default. Even counts have no centre voice, so the
    /// written pitch is not itself sounded.
    pub voice_count: u8,
    /// Detune amount in [`Cents`], applied symmetrically: sub-voices are spaced
    /// evenly over ±this value, so the outermost pair sits `2 * detune_cents`
    /// apart and the centre (odd counts only) stays at the written pitch.
    /// Negative values are clamped to zero. A few cents is a chorus shimmer;
    /// tens of cents is an overtly out-of-tune stack.
    pub detune_cents: Cents,
    /// How wide the sub-voices are panned, as a [`Spread`] in 0.0..1.0.
    /// `0.0` stacks them all at centre, `1.0` puts the outermost pair hard
    /// left and hard right, with the rest spaced evenly between. Pans track
    /// detune position, so the lowest sub-voice is always the leftmost.
    pub stereo_spread: Spread,
    /// Whether each note-on starts its sub-voices at random phases.
    ///
    /// `false` (the default) starts them all in phase, so every note begins
    /// with a brief transient spike as the copies reinforce before the detune
    /// pulls them apart. `true` trades that consistent attack for a softer,
    /// note-to-note-varying one. The generator is a seeded xorshift, so a
    /// render is reproducible after `seed_unison_rng`.
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

/// One unison sub-voice's derived parameters, recomputed by every unison
/// setter.
#[derive(Debug, Clone, Copy, Default)]
pub struct UnisonVoiceParams {
    /// Pitch ratio against the note's pitch; 1.0 is the center pitch. A bare
    /// ratio: it is the output of `Semitones::to_pitch_ratio`, and a
    /// multiplier is not one of the units.
    pub freq_ratio: f32,
    /// Stereo position of this sub-voice.
    pub pan: Pan,
    /// Start phase from the last random draw; `Phase::START` when
    /// `phase_randomize` is off.
    pub phase_offset: Phase,
    /// Gain of this sub-voice, `1 / sqrt(count)`, so the stack's power does
    /// not grow with the voice count.
    pub amplitude: Amplitude,
}

#[derive(Debug, Clone)]
pub struct UnisonEngine {
    config: UnisonConfig,
    voices: [UnisonVoiceParams; MAX_UNISON_VOICES],
    rng_state: u32,
    /// Control-rate-modulatable mirrors of `config.detune_cents` /
    /// `config.stereo_spread`. A modulator writes these (through the
    /// synth's `ParamSet`); [`sync_from_atomics`](Self::sync_from_atomics),
    /// called once per block, folds any change back into `config` + a
    /// recompute. Detune/spread only affect per-voice params on recompute (not
    /// per-sample), so a block-rate sync is exact.
    detune: Param<Cents>,
    spread: Param<Spread>,
}

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

    /// Stops sharing the detune and spread cells with the live engine and its
    /// mod targets, keeping their current values (see `Param::detach`): the
    /// half of `PolySynth`'s `fork_fresh` that makes a fork render the unison
    /// it was taken with.
    pub fn detach(&mut self) {
        self.detune.detach();
        self.spread.detach();
    }

    /// The detune cell (cents), for the synth's declared params.
    pub(crate) fn detune(&self) -> &Param<Cents> {
        &self.detune
    }

    /// The stereo-spread cell (0..1), for the synth's declared params.
    pub(crate) fn spread(&self) -> &Param<Spread> {
        &self.spread
    }

    /// The shared detune atomic (cents), for control-rate modulation.
    pub fn detune_atomic(&self) -> Arc<AtomicF32> {
        self.detune.as_atomic()
    }

    /// The shared stereo-spread atomic (0..1), for control-rate modulation.
    pub fn spread_atomic(&self) -> Arc<AtomicF32> {
        self.spread.as_atomic()
    }

    /// Folds any change to the detune/spread atomics back into
    /// `config` and recompute per-voice params. Called once per block. Cheap
    /// when nothing moved (compares against the current config first).
    ///
    /// `driven` is the block's detune and spread from the synth's param
    /// ports where a lane drives them; `None` reads the atomic.
    /// A detune in cents as [`set_detune`](Self::set_detune) stores it:
    /// floored at 0. Shared by the setter and the block's read.
    #[inline]
    fn clamp_detune(cents: f32) -> f32 {
        cents.max(0.0)
    }

    pub fn sync_from_atomics(&mut self, [detune, spread]: [Option<f32>; 2]) {
        // `set_detune`'s floor at the point of use, driven or not: a
        // negative detune is no detune, and comparing it unfloored against
        // the stored config would recompute every block.
        let detune = Cents(Self::clamp_detune(
            detune.unwrap_or_else(|| self.detune.load().get()),
        ));
        let spread = Spread::new_clamped(spread.unwrap_or_else(|| self.spread.load().get()));
        let changed = (detune.get() - self.config.detune_cents.get()).abs() > f32::EPSILON
            || (spread.get() - self.config.stereo_spread.get()).abs() > f32::EPSILON;
        if changed {
            self.config.detune_cents = detune;
            self.config.stereo_spread = spread;
            self.recompute_params();
        }
    }

    pub fn recompute_params(&mut self) {
        let count = usize::from(self.config.voice_count).clamp(1, MAX_UNISON_VOICES);

        let amplitude = Amplitude(1.0 / (count as f32).sqrt());
        // A converter, not a divide: `Cents` is `Scalable`, so
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
            let pan = Pan(position * self.config.stereo_spread.get());

            // The phase offset is kept: it is the last `randomize_phases`
            // draw, not a function of detune or spread, and a detune moving
            // under modulation must not wipe it.
            self.voices[i] = UnisonVoiceParams {
                freq_ratio,
                pan,
                phase_offset: self.voices[i].phase_offset,
                amplitude,
            };
        }

        for i in count..MAX_UNISON_VOICES {
            self.voices[i] = UnisonVoiceParams::default();
        }
    }

    /// Draws a fresh set of start phases, one per sub-voice, into the
    /// params table, and return them.
    ///
    /// Every call is a new draw, so each voice that starts calls this for its
    /// own stack: two notes of a chord struck together get different phases.
    /// Without `phase_randomize` every phase is [`Phase::START`].
    pub fn randomize_phases(&mut self) -> &[UnisonVoiceParams] {
        if !self.config.phase_randomize {
            for v in &mut self.voices {
                v.phase_offset = Phase::START;
            }
            return self.all_params();
        }

        let count = usize::from(self.config.voice_count).clamp(1, MAX_UNISON_VOICES);

        for i in 0..count {
            self.rng_state ^= self.rng_state << 13;
            self.rng_state ^= self.rng_state >> 17;
            self.rng_state ^= self.rng_state << 5;

            self.voices[i].phase_offset = Phase((self.rng_state as f32) / (u32::MAX as f32));
        }
        self.all_params()
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
        self.config.detune_cents = Cents(Self::clamp_detune(cents.into().get()));
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
mod tests;
