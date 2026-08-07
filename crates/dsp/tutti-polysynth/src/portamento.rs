//! Portamento (pitch glide) for synthesizers.

use tutti_core::{Hz, SampleRate, Seconds, Semitones};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PortamentoMode {
    /// Always glide between notes
    Always,
    /// Only glide during legato (overlapping notes)
    LegatoOnly,
    /// Disabled (no glide)
    #[default]
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PortamentoCurve {
    /// Linear interpolation
    #[default]
    Linear,
    /// Exponential curve (slow start, fast finish)
    Exponential,
    /// Logarithmic curve (fast start, slow finish)
    Logarithmic,
}

#[derive(Debug, Clone)]
pub struct PortamentoConfig {
    pub mode: PortamentoMode,
    pub curve: PortamentoCurve,
    pub time: Seconds,
    /// If true, glide time is constant regardless of interval.
    /// If false, larger intervals take proportionally longer.
    pub constant_time: bool,
}

impl Default for PortamentoConfig {
    fn default() -> Self {
        Self {
            mode: PortamentoMode::Off,
            curve: PortamentoCurve::Linear,
            time: Seconds(0.1),
            constant_time: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Portamento {
    config: PortamentoConfig,
    /// The three glide endpoints. Typed because the public surface already is
    /// — `set_target(impl Into<Hz>)`, `tick() -> Hz`, `current() -> Hz` — so
    /// the `Hz` was unwrapped on the way in and re-wrapped on the way out
    /// purely to cross the struct, leaving three same-typed fields written in
    /// three separate branches.
    start_freq: Hz,
    target_freq: Hz,
    current_freq: Hz,
    /// 0.0 to 1.0
    progress: f32,
    /// Per sample
    rate: f32,
    sample_rate: SampleRate,
}

impl Portamento {
    pub fn new(config: PortamentoConfig, sample_rate: impl Into<SampleRate>) -> Self {
        Self {
            config,
            start_freq: Hz(440.0),
            target_freq: Hz(440.0),
            current_freq: Hz(440.0),
            progress: 1.0,
            rate: 0.0,
            sample_rate: sample_rate.into(),
        }
    }

    pub fn set_target(&mut self, freq: impl Into<Hz>, is_legato: bool) {
        let freq = freq.into();
        let should_glide = match self.config.mode {
            PortamentoMode::Off => false,
            PortamentoMode::Always => true,
            PortamentoMode::LegatoOnly => is_legato,
        };

        let time = self.config.time.get();
        if should_glide && time > 0.0 {
            self.start_freq = self.current_freq;
            self.target_freq = freq;

            let glide_time = if self.config.constant_time {
                time
            } else {
                // The named converter, in octaves: this was
                // `(freq / start).abs().log2().abs()` spelled out, and
                // `Semitones::from_pitch_ratio` guards the non-positive ratio
                // the bare `log2` would have turned into a NaN glide time.
                let interval =
                    (Semitones::from_pitch_ratio(freq.get() / self.start_freq.get()).get() / 12.0)
                        .abs();
                time * interval.max(0.1) // At least 10% of base time
            };

            // A fractional glide length feeding a reciprocal, not a frame
            // count: multiply in f64 and narrow once, rather than narrowing
            // the rate first as this did before.
            let glide_samples = (glide_time as f64 * self.sample_rate.get()) as f32;
            self.rate = if glide_samples > 0.0 {
                1.0 / glide_samples
            } else {
                1.0
            };
            self.progress = 0.0;
        } else {
            self.start_freq = freq;
            self.target_freq = freq;
            self.current_freq = freq;
            self.progress = 1.0;
        }
    }

    #[inline]
    pub fn tick(&mut self) -> Hz {
        if self.progress >= 1.0 {
            return self.target_freq;
        }

        self.progress += self.rate;
        self.progress = self.progress.min(1.0);

        let t = match self.config.curve {
            PortamentoCurve::Linear => self.progress,
            PortamentoCurve::Exponential => self.progress * self.progress,
            PortamentoCurve::Logarithmic => self.progress.sqrt(),
        };

        let log_start = self.start_freq.get().ln();
        let log_target = self.target_freq.get().ln();
        self.current_freq = Hz((log_start + (log_target - log_start) * t).exp());

        self.current_freq
    }

    #[inline]
    pub fn current(&self) -> Hz {
        self.current_freq
    }

    #[inline]
    pub fn is_gliding(&self) -> bool {
        self.progress < 1.0
    }

    pub fn reset(&mut self, freq: impl Into<Hz>) {
        let freq = freq.into();
        self.start_freq = freq;
        self.target_freq = freq;
        self.current_freq = freq;
        self.progress = 1.0;
    }

    pub fn set_sample_rate(&mut self, sample_rate: impl Into<SampleRate>) {
        self.sample_rate = sample_rate.into();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_glide() {
        let config = PortamentoConfig {
            mode: PortamentoMode::Off,
            ..Default::default()
        };
        let mut porta = Portamento::new(config, 44100.0);

        porta.set_target(880.0, false);
        assert!((porta.tick().get() - 880.0).abs() < 0.01);
        assert!(!porta.is_gliding());
    }

    #[test]
    fn test_always_glide() {
        let config = PortamentoConfig {
            mode: PortamentoMode::Always,
            time: Seconds(0.01), // 10ms
            ..Default::default()
        };
        let mut porta = Portamento::new(config, 44100.0);

        porta.reset(440.0);
        porta.set_target(880.0, false);

        // Should start near 440
        let first = porta.tick();
        assert!(first.get() < 500.0);

        // Run until glide finishes
        while porta.is_gliding() {
            porta.tick();
        }

        // Should end at 880
        assert!((porta.current().get() - 880.0).abs() < 1.0);
    }

    #[test]
    fn test_legato_only() {
        let config = PortamentoConfig {
            mode: PortamentoMode::LegatoOnly,
            time: Seconds(0.01),
            ..Default::default()
        };
        let mut porta = Portamento::new(config, 44100.0);

        porta.reset(440.0);

        // Non-legato should not glide
        porta.set_target(880.0, false);
        assert!(!porta.is_gliding());
        assert!((porta.current().get() - 880.0).abs() < 0.01);

        // Legato should glide
        porta.set_target(440.0, true);
        assert!(porta.is_gliding());
    }

    #[test]
    fn test_curve_shapes() {
        for curve in [
            PortamentoCurve::Linear,
            PortamentoCurve::Exponential,
            PortamentoCurve::Logarithmic,
        ] {
            let config = PortamentoConfig {
                mode: PortamentoMode::Always,
                curve,
                time: Seconds(0.01),
                ..Default::default()
            };
            let mut porta = Portamento::new(config, 44100.0);

            porta.reset(440.0);
            porta.set_target(880.0, false);

            // Run to completion
            while porta.is_gliding() {
                porta.tick();
            }

            // All curves should reach the target
            assert!((porta.current().get() - 880.0).abs() < 1.0);
        }
    }
}
