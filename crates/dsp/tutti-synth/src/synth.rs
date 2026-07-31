//! [`SynthConfig`] and the value types that configure a synth (oscillator /
//! filter / envelope). The synth itself ([`PolySynth`](crate::PolySynth)) and
//! its per-voice engine live in `crate::polysynth` / `crate::synth_voice`.

use core::fmt;

use crate::{AllocationStrategy, PortamentoConfig, Tuning, UnisonConfig, VoiceMode};
use tutti_core::{Hz, Resonance, Semitones, Q};

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum OscillatorType {
    Sine,
    #[default]
    Saw,
    Square {
        pulse_width: f32,
    },
    Triangle,
    Noise,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SvfMode {
    #[default]
    Lowpass,
    Highpass,
    Bandpass,
    Notch,
}

/// Which filter a voice runs, and its settings.
///
/// The two variants deliberately carry *different* resonance types.
/// `Resonance` and `Q` both answer "how resonant", but a ladder at 1.0
/// self-oscillates while a Q of 1.0 is a mild bell — and as bare `f32`s in
/// adjacent variants of one public enum, swapping them was silent and sounded
/// like a mistuned filter. The nodes downstream already take `Resonance` and
/// `Q` respectively; these fields simply stop erasing that on the way in.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum FilterType {
    Moog {
        cutoff: Hz,
        resonance: Resonance,
    },
    Svf {
        cutoff: Hz,
        q: Q,
        mode: SvfMode,
    },
    #[default]
    None,
}

impl fmt::Display for OscillatorType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OscillatorType::Sine => write!(f, "Sine"),
            OscillatorType::Saw => write!(f, "Saw"),
            OscillatorType::Square { .. } => write!(f, "Square"),
            OscillatorType::Triangle => write!(f, "Triangle"),
            OscillatorType::Noise => write!(f, "Pink Noise"),
        }
    }
}

impl fmt::Display for SvfMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SvfMode::Lowpass => write!(f, "Lowpass"),
            SvfMode::Highpass => write!(f, "Highpass"),
            SvfMode::Bandpass => write!(f, "Bandpass"),
            SvfMode::Notch => write!(f, "Notch"),
        }
    }
}

impl fmt::Display for FilterType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FilterType::Moog { .. } => write!(f, "Moog Ladder"),
            FilterType::Svf { mode, .. } => write!(f, "SVF ({})", mode),
            FilterType::None => write!(f, "None"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EnvelopeConfig {
    pub attack: f32,
    pub decay: f32,
    /// 0.0 - 1.0
    pub sustain: f32,
    pub release: f32,
}

impl Default for EnvelopeConfig {
    fn default() -> Self {
        Self {
            attack: 0.01,
            decay: 0.1,
            sustain: 0.7,
            release: 0.2,
        }
    }
}

impl EnvelopeConfig {
    pub fn new(attack: f32, decay: f32, sustain: f32, release: f32) -> Self {
        Self {
            attack,
            decay,
            sustain,
            release,
        }
    }

    pub fn organ() -> Self {
        Self::new(0.001, 0.0, 1.0, 0.01)
    }

    pub fn pluck() -> Self {
        Self::new(0.001, 0.3, 0.0, 0.1)
    }

    pub fn pad() -> Self {
        Self::new(0.5, 0.2, 0.8, 1.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct FilterModConfig {
    /// Mod wheel (CC1) to filter cutoff depth (0.0-1.0, default: 0.0)
    /// At 1.0, mod wheel fully open doubles the cutoff frequency.
    pub mod_wheel_depth: f32,
    /// Velocity to filter cutoff depth (0.0-1.0, default: 0.0)
    /// At 1.0, velocity 0 halves cutoff, velocity 127 uses full cutoff.
    pub velocity_depth: f32,
    /// LFO rate in Hz (default: 0.0 = disabled)
    pub lfo_rate: f32,
    /// LFO to filter cutoff depth (0.0-1.0, default: 0.0)
    /// At 1.0, LFO sweeps cutoff by ±50%.
    pub lfo_depth: f32,
}

#[derive(Debug, Clone)]
pub struct SynthConfig {
    pub sample_rate: tutti_core::SampleRate,
    pub max_voices: usize,
    pub voice_mode: VoiceMode,
    pub oscillator: OscillatorType,
    pub filter: FilterType,
    pub envelope: EnvelopeConfig,
    pub portamento: Option<PortamentoConfig>,
    pub unison: Option<UnisonConfig>,
    pub allocation_strategy: AllocationStrategy,
    pub tuning: Tuning,
    /// Default: 2.0
    pub pitch_bend_range: Semitones,
    pub filter_mod: FilterModConfig,
    pub mpe_enabled: bool,
    /// MPE pitch bend range per-note (default: 48.0).
    pub mpe_pitch_bend_range: Semitones,
}

impl Default for SynthConfig {
    fn default() -> Self {
        Self {
            sample_rate: tutti_core::SampleRate::SR_44K1,
            max_voices: 8,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::default(),
            filter: FilterType::default(),
            envelope: EnvelopeConfig::default(),
            portamento: None,
            unison: None,
            allocation_strategy: AllocationStrategy::Oldest,
            tuning: Tuning::equal_temperament(),
            pitch_bend_range: Semitones(2.0),
            filter_mod: FilterModConfig::default(),
            mpe_enabled: false,
            mpe_pitch_bend_range: Semitones(48.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_envelope_presets() {
        let organ = EnvelopeConfig::organ();
        assert!(organ.attack < 0.01);
        assert_eq!(organ.sustain, 1.0);

        let pluck = EnvelopeConfig::pluck();
        assert!(pluck.attack < 0.01);
        assert_eq!(pluck.sustain, 0.0);

        let pad = EnvelopeConfig::pad();
        assert!(pad.attack > 0.1);
        assert!(pad.release > 0.5);
    }
}
