#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    MidiInput,
    AudioInput,
    InternalAudio,
    Pattern,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Replace,
    Overdub,
    Loop,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub channel_index: usize,
    pub source: Source,
    pub mode: Mode,
    pub quantize: Option<QuantizeSettings>,
    pub metronome: bool,
    pub preroll_beats: f64,
    pub punch_in: Option<f64>,
    pub punch_out: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QuantizeSettings {
    pub resolution: f64,
    pub strength: f32,
    pub swing: f32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            channel_index: 0,
            source: Source::MidiInput,
            mode: Mode::Replace,
            quantize: None,
            metronome: false,
            preroll_beats: 0.0,
            punch_in: None,
            punch_out: None,
        }
    }
}

impl Config {
    pub fn builder() -> ConfigBuilder {
        ConfigBuilder::default()
    }
}

#[derive(Clone, Debug)]
pub struct ConfigBuilder {
    channel_index: usize,
    source: Source,
    mode: Mode,
    quantize: Option<QuantizeSettings>,
    metronome: bool,
    preroll_beats: f64,
    punch_in: Option<f64>,
    punch_out: Option<f64>,
}

impl Default for ConfigBuilder {
    fn default() -> Self {
        Self {
            channel_index: 0,
            source: Source::MidiInput,
            mode: Mode::Replace,
            quantize: None,
            metronome: false,
            preroll_beats: 0.0,
            punch_in: None,
            punch_out: None,
        }
    }
}

impl ConfigBuilder {
    pub fn channel(mut self, index: usize) -> Self {
        self.channel_index = index;
        self
    }

    pub fn source(mut self, source: Source) -> Self {
        self.source = source;
        self
    }

    pub fn mode(mut self, mode: Mode) -> Self {
        self.mode = mode;
        self
    }

    pub fn quantize(mut self, settings: QuantizeSettings) -> Self {
        self.quantize = Some(settings);
        self
    }

    pub fn metronome(mut self, enabled: bool) -> Self {
        self.metronome = enabled;
        self
    }

    pub fn preroll_beats(mut self, beats: f64) -> Self {
        self.preroll_beats = beats;
        self
    }

    pub fn punch_in(mut self, beat: f64) -> Self {
        self.punch_in = Some(beat);
        self
    }

    pub fn punch_out(mut self, beat: f64) -> Self {
        self.punch_out = Some(beat);
        self
    }

    pub fn punch_range(mut self, in_beat: f64, out_beat: f64) -> Self {
        self.punch_in = Some(in_beat);
        self.punch_out = Some(out_beat);
        self
    }

    pub fn build(self) -> Config {
        Config {
            channel_index: self.channel_index,
            source: self.source,
            mode: self.mode,
            quantize: self.quantize,
            metronome: self.metronome,
            preroll_beats: self.preroll_beats,
            punch_in: self.punch_in,
            punch_out: self.punch_out,
        }
    }
}

impl QuantizeSettings {
    pub fn new(resolution: f64) -> Self {
        Self {
            resolution,
            strength: 1.0,
            swing: 0.0,
        }
    }

    pub fn builder() -> QuantizeSettingsBuilder {
        QuantizeSettingsBuilder::default()
    }

    pub fn quantize(&self, beat: f64) -> f64 {
        if self.strength == 0.0 {
            return beat;
        }

        let grid_position = (beat / self.resolution).round();
        let quantized = grid_position * self.resolution;

        let swung = if self.swing > 0.0 && grid_position % 2.0 == 1.0 {
            quantized + (self.resolution * f64::from(self.swing) * 0.5)
        } else {
            quantized
        };

        beat + (swung - beat) * f64::from(self.strength)
    }
}

#[derive(Clone, Debug)]
pub struct QuantizeSettingsBuilder {
    resolution: f64,
    strength: f32,
    swing: f32,
}

impl Default for QuantizeSettingsBuilder {
    fn default() -> Self {
        Self {
            resolution: 0.25, // 16th notes
            strength: 1.0,
            swing: 0.0,
        }
    }
}

impl QuantizeSettingsBuilder {
    pub fn resolution(mut self, beats: f64) -> Self {
        self.resolution = beats.max(0.0);
        self
    }

    pub fn strength(mut self, strength: f32) -> Self {
        self.strength = strength.clamp(0.0, 1.0);
        self
    }

    pub fn swing(mut self, swing: f32) -> Self {
        self.swing = swing.clamp(0.0, 1.0);
        self
    }

    pub fn build(self) -> QuantizeSettings {
        QuantizeSettings {
            resolution: self.resolution,
            strength: self.strength,
            swing: self.swing,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_quantize_settings() {
        let settings = QuantizeSettings::new(0.25); // 16th notes

        // Test full quantization
        assert!((settings.quantize(0.13) - 0.25).abs() < 0.001);
        assert!((settings.quantize(0.87) - 0.75).abs() < 0.001);

        // Test partial quantization
        let partial = QuantizeSettings {
            resolution: 0.25,
            strength: 0.5,
            swing: 0.0,
        };
        let result = partial.quantize(0.13);
        assert!(result > 0.13 && result < 0.25); // Between original and quantized
    }

    #[test]
    fn test_swing() {
        let settings = QuantizeSettings {
            resolution: 0.25,
            strength: 1.0,
            swing: 0.5,
        };

        // First beat: no swing
        assert!((settings.quantize(0.0) - 0.0).abs() < 0.001);

        // Second beat: swung
        let swung = settings.quantize(0.25);
        assert!(swung > 0.25); // Shifted forward
    }

    #[test]
    fn test_quantize_builder() {
        let settings = QuantizeSettings::builder()
            .resolution(0.5)
            .strength(0.75)
            .swing(0.25)
            .build();

        assert_eq!(settings.resolution, 0.5);
        assert_eq!(settings.strength, 0.75);
        assert_eq!(settings.swing, 0.25);
    }

    #[test]
    fn test_quantize_builder_clamps_values() {
        let settings = QuantizeSettings::builder()
            .strength(1.5) // Should clamp to 1.0
            .swing(-0.5) // Should clamp to 0.0
            .build();

        assert_eq!(settings.strength, 1.0);
        assert_eq!(settings.swing, 0.0);
    }

    #[test]
    fn test_recording_config_builder() {
        let config = Config::builder()
            .channel(2)
            .source(Source::AudioInput)
            .mode(Mode::Overdub)
            .quantize(QuantizeSettings::new(0.25))
            .metronome(true)
            .preroll_beats(1.0)
            .punch_in(4.0)
            .punch_out(8.0)
            .build();

        assert_eq!(config.channel_index, 2);
        assert_eq!(config.source, Source::AudioInput);
        assert_eq!(config.mode, Mode::Overdub);
        assert!(config.quantize.is_some());
        assert!(config.metronome);
        assert_eq!(config.preroll_beats, 1.0);
        assert_eq!(config.punch_in, Some(4.0));
        assert_eq!(config.punch_out, Some(8.0));
    }

    #[test]
    fn test_recording_config_builder_punch_range() {
        let config = Config::builder().punch_range(4.0, 8.0).build();

        assert_eq!(config.punch_in, Some(4.0));
        assert_eq!(config.punch_out, Some(8.0));
    }
}
