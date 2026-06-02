//! Fluent builder API for creating synthesizers.

#[cfg(feature = "midi")]
mod polysynth;
#[cfg(feature = "midi")]
mod voice;

#[cfg(feature = "midi")]
pub use polysynth::PolySynth;

use core::fmt;

use crate::{AllocationStrategy, PortamentoConfig, Tuning, UnisonConfig, VoiceMode};
use tutti_core::Semitones;

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

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum FilterType {
    Moog {
        cutoff: f32,
        resonance: f32,
    },
    Svf {
        cutoff: f32,
        q: f32,
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
    pub sample_rate: f64,
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
            sample_rate: 44100.0,
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

#[derive(Debug, Clone)]
pub struct SynthBuilder {
    config: SynthConfig,
}

impl SynthBuilder {
    pub fn new(sample_rate: f64) -> Self {
        Self {
            config: SynthConfig {
                sample_rate,
                ..Default::default()
            },
        }
    }

    pub fn poly(mut self, voices: usize) -> Self {
        self.config.max_voices = voices;
        self.config.voice_mode = VoiceMode::Poly;
        self
    }

    pub fn mono(mut self) -> Self {
        self.config.max_voices = 1;
        self.config.voice_mode = VoiceMode::Mono;
        self
    }

    pub fn legato(mut self) -> Self {
        self.config.max_voices = 1;
        self.config.voice_mode = VoiceMode::Legato;
        self
    }

    pub fn oscillator(mut self, osc: OscillatorType) -> Self {
        self.config.oscillator = osc;
        self
    }

    pub fn filter(mut self, filter: FilterType) -> Self {
        self.config.filter = filter;
        self
    }

    pub fn envelope(mut self, attack: f32, decay: f32, sustain: f32, release: f32) -> Self {
        self.config.envelope = EnvelopeConfig::new(attack, decay, sustain, release);
        self
    }

    pub fn envelope_config(mut self, config: EnvelopeConfig) -> Self {
        self.config.envelope = config;
        self
    }

    pub fn portamento(mut self, config: PortamentoConfig) -> Self {
        self.config.portamento = Some(config);
        self
    }

    pub fn unison(mut self, config: UnisonConfig) -> Self {
        self.config.unison = Some(config);
        self
    }

    pub fn voice_stealing(mut self, strategy: AllocationStrategy) -> Self {
        self.config.allocation_strategy = strategy;
        self
    }

    pub fn tuning(mut self, tuning: Tuning) -> Self {
        self.config.tuning = tuning;
        self
    }

    pub fn pitch_bend_range(mut self, semitones: impl Into<Semitones>) -> Self {
        self.config.pitch_bend_range = semitones.into();
        self
    }

    pub fn mod_wheel_to_filter(mut self, depth: f32) -> Self {
        self.config.filter_mod.mod_wheel_depth = depth.clamp(0.0, 1.0);
        self
    }

    pub fn velocity_to_filter(mut self, depth: f32) -> Self {
        self.config.filter_mod.velocity_depth = depth.clamp(0.0, 1.0);
        self
    }

    pub fn lfo_to_filter(mut self, rate: f32, depth: f32) -> Self {
        self.config.filter_mod.lfo_rate = rate.max(0.0);
        self.config.filter_mod.lfo_depth = depth.clamp(0.0, 1.0);
        self
    }

    pub fn mpe(mut self, enabled: bool) -> Self {
        self.config.mpe_enabled = enabled;
        self
    }

    pub fn mpe_pitch_bend_range(mut self, semitones: impl Into<Semitones>) -> Self {
        self.config.mpe_pitch_bend_range = Semitones(semitones.into().get().clamp(0.0, 96.0));
        self
    }

    #[cfg(feature = "midi")]
    pub fn build(self) -> crate::Result<PolySynth> {
        PolySynth::from_config(self.config)
    }

    #[cfg(test)]
    pub fn config(&self) -> &SynthConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builder_defaults() {
        let builder = SynthBuilder::new(44100.0);
        let config = builder.config();

        assert_eq!(config.sample_rate, 44100.0);
        assert_eq!(config.max_voices, 8);
        assert_eq!(config.voice_mode, VoiceMode::Poly);
    }

    #[test]
    fn test_builder_chain() {
        let builder = SynthBuilder::new(48000.0)
            .poly(16)
            .oscillator(OscillatorType::Square { pulse_width: 0.5 })
            .filter(FilterType::Moog {
                cutoff: 2000.0,
                resonance: 0.7,
            })
            .envelope(0.01, 0.2, 0.6, 0.3)
            .voice_stealing(AllocationStrategy::Quietest);

        let config = builder.config();
        assert_eq!(config.sample_rate, 48000.0);
        assert_eq!(config.max_voices, 16);
        assert!(matches!(
            config.oscillator,
            OscillatorType::Square { pulse_width: 0.5 }
        ));
        assert!(matches!(
            config.filter,
            FilterType::Moog {
                cutoff: 2000.0,
                resonance: 0.7
            }
        ));
    }

    #[test]
    fn test_mono_mode() {
        let builder = SynthBuilder::new(44100.0).mono();
        assert_eq!(builder.config().max_voices, 1);
        assert_eq!(builder.config().voice_mode, VoiceMode::Mono);
    }

    #[test]
    fn test_legato_mode() {
        let builder = SynthBuilder::new(44100.0).legato();
        assert_eq!(builder.config().max_voices, 1);
        assert_eq!(builder.config().voice_mode, VoiceMode::Legato);
    }

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
