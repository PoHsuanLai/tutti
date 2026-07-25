//! Time-stretching types and parameters.

use tutti_core::{Cents, StretchFactor};

/// Flush a subnormal (denormal) float to zero.
///
/// The phase-vocoder overlap-add FIFO is an IIR-like accumulator:
/// on a silent tail it can decay into the subnormal range, where x86 FPUs
/// trap into microcode and cause large CPU spikes. Snapping subnormals to zero
/// avoids that. It never changes audible output — subnormals are below
/// `~1.2e-38`, far under any perceptible level and under the noise floor of
/// 32-bit audio. Pure arithmetic branch, zero-alloc, safe on the audio thread.
#[inline(always)]
pub(super) fn flush_denormal(x: f32) -> f32 {
    if x.is_subnormal() {
        0.0
    } else {
        x
    }
}

/// Time-stretch and pitch-shift parameters
///
/// ## Range Limits
///
/// - `stretch_factor`: 0.25 - 4.0 (quarter speed to 4x speed)
/// - `pitch_cents`: -2400 to +2400 (±2 octaves)
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Params {
    /// Playback speed factor (1.0 = normal, 0.5 = half speed, 2.0 = double speed)
    /// Range: 0.25 to 4.0
    pub stretch_factor: StretchFactor,

    /// Pitch shift in cents (100 cents = 1 semitone)
    /// Range: -2400 to +2400 (±2 octaves)
    pub pitch_cents: Cents,

    /// Whether to preserve formants when pitch-shifting
    /// Important for vocal/speech content
    pub preserve_formants: bool,
}

impl Params {
    /// Minimum stretch factor (1/4 speed). Mirrors [`StretchFactor::MIN`],
    /// which is where the bound is actually enforced.
    pub const MIN_STRETCH: f32 = StretchFactor::MIN.get();
    /// Maximum stretch factor (4x speed). Mirrors [`StretchFactor::MAX`].
    pub const MAX_STRETCH: f32 = StretchFactor::MAX.get();
    /// Minimum pitch shift (-2 octaves)
    pub const MIN_PITCH_CENTS: f32 = -2400.0;
    /// Maximum pitch shift (+2 octaves)
    pub const MAX_PITCH_CENTS: f32 = 2400.0;

    pub fn new() -> Self {
        Self {
            stretch_factor: StretchFactor::UNITY,
            pitch_cents: Cents::new(0.0),
            preserve_formants: false,
        }
    }

    pub fn stretch_factor(mut self, factor: StretchFactor) -> Self {
        self.stretch_factor = StretchFactor::new_clamped(factor.get());
        self
    }

    pub fn pitch_cents(mut self, cents: Cents) -> Self {
        self.pitch_cents = Cents::new(
            cents
                .get()
                .clamp(Self::MIN_PITCH_CENTS, Self::MAX_PITCH_CENTS),
        );
        self
    }

    pub fn preserve_formants(mut self, preserve: bool) -> Self {
        self.preserve_formants = preserve;
        self
    }

    /// Check if any time-stretching/pitch-shifting is active
    pub fn is_active(&self) -> bool {
        (self.stretch_factor.get() - 1.0).abs() > 0.001 || self.pitch_cents.get().abs() > 0.5
    }

    /// Calculate the effective playback rate
    ///
    /// When pitch-shifting without formant preservation, we need to
    /// adjust playback speed to compensate for the pitch change.
    pub fn effective_stretch_factor(&self) -> f32 {
        // Convert cents to frequency ratio: 2^(cents/1200)
        let pitch_ratio = 2.0_f32.powf(self.pitch_cents.get() / 1200.0);

        if self.preserve_formants {
            // Formant preservation: stretch factor is independent of pitch
            self.stretch_factor.get()
        } else {
            // Standard pitch-shift: combine stretch and pitch factors
            self.stretch_factor.get() / pitch_ratio
        }
    }

    /// Calculate the synthesis hop size ratio relative to analysis hop
    ///
    /// For time-stretching, we modify the synthesis hop while keeping
    /// analysis hop constant. This ratio determines the stretch.
    pub fn synthesis_hop_ratio(&self) -> f32 {
        // stretch_factor > 1 means slower playback (longer output)
        // So we need larger synthesis hop to spread frames out
        self.stretch_factor.get()
    }

    /// Calculate the phase increment factor for pitch shifting
    ///
    /// When pitch-shifting, we need to modify the phase accumulation
    /// to shift frequencies up or down.
    pub fn pitch_shift_ratio(&self) -> f32 {
        2.0_f32.powf(self.pitch_cents.get() / 1200.0)
    }
}

impl Default for Params {
    fn default() -> Self {
        Self::new()
    }
}

/// Time-stretching algorithm selection
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Algorithm {
    /// Phase vocoder (FFT-based) - best for melodic/pitched content
    #[default]
    PhaseVocoder,
}

/// FFT size presets for latency/quality trade-off
///
/// Larger FFT sizes provide better frequency resolution and quality
/// but introduce more latency. Choose based on your use case:
///
/// - **Small (1024)**: Live performance, minimal latency (~12ms @ 44.1kHz)
/// - **Medium (2048)**: Default, balanced latency/quality (~23ms @ 44.1kHz)
/// - **Large (4096)**: Mixing/mastering, high quality (~46ms @ 44.1kHz)
/// - **XLarge (8192)**: Extreme stretching (Paulstretch), excellent quality (~93ms @ 44.1kHz)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FftSize {
    /// 1024-point FFT (~12ms latency @ 44.1kHz)
    Small = 1024,

    /// 2048-point FFT (~23ms latency @ 44.1kHz) - Default
    #[default]
    Medium = 2048,

    /// 4096-point FFT (~46ms latency @ 44.1kHz)
    Large = 4096,

    /// 8192-point FFT (~93ms latency @ 44.1kHz)
    XLarge = 8192,
}

impl FftSize {
    pub fn size(&self) -> usize {
        *self as usize
    }

    /// FFT size / 4 = 75% overlap.
    pub fn hop_size(&self) -> usize {
        self.size() / 4
    }

    /// Get the approximate latency in seconds at a given sample rate
    pub fn latency_seconds(&self, sample_rate: f64) -> f64 {
        self.size() as f64 / sample_rate
    }

    /// Get the approximate latency in milliseconds at a given sample rate
    pub fn latency_ms(&self, sample_rate: f64) -> f64 {
        self.latency_seconds(sample_rate) * 1000.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_params_default() {
        let params = Params::new();
        assert!((params.stretch_factor.get() - 1.0).abs() < 0.001);
        assert!(params.pitch_cents.get().abs() < 0.001);
        assert!(!params.preserve_formants);
        assert!(!params.is_active());
    }

    #[test]
    fn test_params_builder() {
        let params = Params::new()
            .stretch_factor(StretchFactor::new(2.0))
            .pitch_cents(Cents::new(1200.0))
            .preserve_formants(true);

        assert!((params.stretch_factor.get() - 2.0).abs() < 0.001);
        assert!((params.pitch_cents.get() - 1200.0).abs() < 0.001);
        assert!(params.preserve_formants);
        assert!(params.is_active());
    }

    #[test]
    fn test_params_clamping() {
        let params = Params::new()
            .stretch_factor(StretchFactor::new(10.0)) // Should clamp to 4.0
            .pitch_cents(Cents::new(5000.0)); // Should clamp to 2400.0

        assert!((params.stretch_factor.get() - 4.0).abs() < 0.001);
        assert!((params.pitch_cents.get() - 2400.0).abs() < 0.001);

        let params2 = Params::new()
            .stretch_factor(StretchFactor::new(0.1)) // Should clamp to 0.25
            .pitch_cents(Cents::new(-5000.0)); // Should clamp to -2400.0

        assert!((params2.stretch_factor.get() - 0.25).abs() < 0.001);
        assert!((params2.pitch_cents.get() - (-2400.0)).abs() < 0.001);
    }

    #[test]
    fn test_effective_stretch_no_pitch() {
        let params = Params::new().stretch_factor(StretchFactor::new(1.5));
        assert!((params.effective_stretch_factor() - 1.5).abs() < 0.001);
    }

    #[test]
    fn test_effective_stretch_with_pitch() {
        // Pitch up by 1 octave (1200 cents) = 2x frequency
        // Without formant preservation, effective stretch = stretch / pitch_ratio
        let params = Params::new()
            .stretch_factor(StretchFactor::new(1.0))
            .pitch_cents(Cents::new(1200.0));

        let effective = params.effective_stretch_factor();
        // 1.0 / 2.0 = 0.5
        assert!(
            (effective - 0.5).abs() < 0.01,
            "Expected ~0.5, got {}",
            effective
        );
    }

    #[test]
    fn test_effective_stretch_with_formant_preservation() {
        let params = Params::new()
            .stretch_factor(StretchFactor::new(1.5))
            .pitch_cents(Cents::new(1200.0))
            .preserve_formants(true);

        // With formant preservation, stretch factor is independent
        assert!((params.effective_stretch_factor() - 1.5).abs() < 0.001);
    }

    #[test]
    fn test_pitch_shift_ratio() {
        let params = Params::new().pitch_cents(Cents::new(1200.0)); // +1 octave
        assert!((params.pitch_shift_ratio() - 2.0).abs() < 0.01);

        let params2 = Params::new().pitch_cents(Cents::new(-1200.0)); // -1 octave
        assert!((params2.pitch_shift_ratio() - 0.5).abs() < 0.01);

        let params3 = Params::new().pitch_cents(Cents::new(0.0)); // No shift
        assert!((params3.pitch_shift_ratio() - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_fft_size() {
        assert_eq!(FftSize::Small.size(), 1024);
        assert_eq!(FftSize::Medium.size(), 2048);
        assert_eq!(FftSize::Large.size(), 4096);
        assert_eq!(FftSize::XLarge.size(), 8192);

        assert_eq!(FftSize::Medium.hop_size(), 512);

        // Check latency at 44100 Hz
        let latency_ms = FftSize::Medium.latency_ms(44100.0);
        assert!((latency_ms - 46.44).abs() < 1.0); // ~46ms for 2048 samples
    }
}
