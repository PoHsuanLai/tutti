//! FFT magnitude spectrum for live visualization.

/// FFT magnitude spectrum result.
#[derive(Debug, Clone)]
pub struct SpectrumResult {
    /// Magnitude bins (linear scale, 0.0..1.0 normalized to peak).
    pub magnitudes: Vec<f32>,
    /// Number of bins (= fft_size / 2).
    pub num_bins: usize,
    /// Frequency resolution in Hz per bin (= sample_rate / fft_size).
    pub freq_resolution: f64,
}

impl Default for SpectrumResult {
    fn default() -> Self {
        Self {
            magnitudes: Vec::new(),
            num_bins: 0,
            freq_resolution: 0.0,
        }
    }
}

impl SpectrumResult {
    /// Frequency in Hz for a given bin index.
    pub fn bin_frequency(&self, bin: usize) -> f64 {
        bin as f64 * self.freq_resolution
    }
}
