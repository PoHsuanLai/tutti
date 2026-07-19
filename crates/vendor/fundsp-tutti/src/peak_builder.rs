//! Streaming peak builder for incremental waveform mipmap construction.
//!
//! Accumulates min/max peaks from decoded audio samples in fixed-size bins.
//! Used during wave decode to build waveform data without a second pass.

use alloc::vec::Vec;

/// Builds level-0 mipmap peaks incrementally from decoded audio chunks.
pub struct StreamingPeakBuilder {
    peaks: Vec<(f32, f32)>,
    current_min: f32,
    current_max: f32,
    current_count: usize,
    samples_per_peak: usize,
}

impl StreamingPeakBuilder {
    pub fn new(samples_per_peak: usize) -> Self {
        Self {
            peaks: Vec::new(),
            current_min: f32::MAX,
            current_max: f32::MIN,
            current_count: 0,
            samples_per_peak,
        }
    }

    /// Feed a chunk of decoded channel-0 samples.
    pub fn push_samples(&mut self, samples: &[f32]) {
        for &s in samples {
            self.current_min = self.current_min.min(s);
            self.current_max = self.current_max.max(s);
            self.current_count += 1;
            if self.current_count >= self.samples_per_peak {
                self.peaks.push((self.current_min, self.current_max));
                self.current_min = f32::MAX;
                self.current_max = f32::MIN;
                self.current_count = 0;
            }
        }
    }

    /// Clone current completed peaks (for sending to main thread).
    pub fn snapshot(&self) -> Vec<(f32, f32)> {
        self.peaks.clone()
    }

    /// Number of completed peaks.
    pub fn len(&self) -> usize {
        self.peaks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.peaks.is_empty()
    }

    /// Flush any partial bin and return all peaks.
    pub fn finish(mut self) -> Vec<(f32, f32)> {
        if self.current_count > 0 {
            self.peaks.push((self.current_min, self.current_max));
        }
        self.peaks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_bins() {
        let mut builder = StreamingPeakBuilder::new(4);
        builder.push_samples(&[0.1, 0.5, -0.3, 0.8]); // bin 1
        builder.push_samples(&[-1.0, 0.0, 0.2, 0.3]); // bin 2
        assert_eq!(builder.len(), 2);
        let peaks = builder.finish();
        assert_eq!(peaks.len(), 2);
        assert_eq!(peaks[0], (-0.3, 0.8));
        assert_eq!(peaks[1], (-1.0, 0.3));
    }

    #[test]
    fn partial_last_bin() {
        let mut builder = StreamingPeakBuilder::new(4);
        builder.push_samples(&[0.1, 0.5, -0.3, 0.8, 0.2]); // 1 full + 1 partial
        assert_eq!(builder.len(), 1);
        let peaks = builder.finish();
        assert_eq!(peaks.len(), 2); // partial flushed
        assert_eq!(peaks[1], (0.2, 0.2));
    }

    #[test]
    fn snapshot_does_not_consume() {
        let mut builder = StreamingPeakBuilder::new(2);
        builder.push_samples(&[0.1, 0.5, -0.3, 0.8]);
        let snap = builder.snapshot();
        assert_eq!(snap.len(), 2);
        // Can still finish
        let peaks = builder.finish();
        assert_eq!(peaks.len(), 2);
    }

    #[test]
    fn chunked_input() {
        let mut builder = StreamingPeakBuilder::new(4);
        // Feed in small chunks that cross bin boundaries
        builder.push_samples(&[0.1, 0.2]);
        builder.push_samples(&[0.3, 0.4, -0.5]);
        builder.push_samples(&[0.6, 0.7, 0.8]);
        assert_eq!(builder.len(), 2);
        let peaks = builder.finish();
        assert_eq!(peaks[0], (0.1, 0.4));
        assert_eq!(peaks[1], (-0.5, 0.8));
    }
}
