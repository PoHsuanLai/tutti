//! Inverse Short-Time Fourier Transform (ISTFT) for spectral resynthesis.
//!
//! Reconstructs a time-domain audio signal from magnitude and phase spectra
//! using IFFT + overlap-add with a Hann synthesis window.

use rustfft::{num_complex::Complex, FftPlanner};

/// Reconstruct audio from magnitude and phase spectra via IFFT + overlap-add.
///
/// # Arguments
/// - `magnitudes`: Flat row-major magnitudes (`time_frames * freq_bins`), unnormalized.
/// - `phases`: Flat row-major phases (`time_frames * freq_bins`), in radians.
/// - `freq_bins`: Number of frequency bins per frame (= window_size / 2).
/// - `time_frames`: Number of time frames.
/// - `window_size`: FFT window size (must match the forward STFT).
/// - `hop_size`: Hop size between frames (must match the forward STFT).
///
/// # Returns
/// Reconstructed mono audio samples.
pub fn istft(
    magnitudes: &[f32],
    phases: &[f32],
    freq_bins: usize,
    time_frames: usize,
    window_size: usize,
    hop_size: usize,
) -> Vec<f32> {
    assert_eq!(magnitudes.len(), time_frames * freq_bins);
    assert_eq!(phases.len(), time_frames * freq_bins);
    assert!(window_size > 0 && hop_size > 0);
    assert_eq!(freq_bins, window_size / 2);

    if time_frames == 0 {
        return vec![];
    }

    let output_len = (time_frames - 1) * hop_size + window_size;
    let mut output = vec![0.0f32; output_len];
    let mut window_sum = vec![0.0f32; output_len];

    // Hann synthesis window
    let hann: Vec<f32> = crate::window::hann(window_size);

    let mut planner = FftPlanner::<f32>::new();
    let ifft = planner.plan_fft_inverse(window_size);
    let mut scratch = vec![Complex::default(); ifft.get_inplace_scratch_len()];
    let mut fft_buf = vec![Complex::default(); window_size];

    let scale = 1.0 / window_size as f32;

    for frame in 0..time_frames {
        let src = frame * freq_bins;

        // Build full complex spectrum from positive frequencies.
        // We have bins 0..freq_bins (= 0..N/2) from the forward STFT.
        // Bin 0 = DC, bins 1..N/2-1 = positive frequencies.
        // Bin N/2 (Nyquist) is not stored — set to zero.
        for bin in 0..freq_bins {
            let mag = magnitudes[src + bin];
            let phase = phases[src + bin];
            fft_buf[bin] = Complex {
                re: mag * phase.cos(),
                im: mag * phase.sin(),
            };
        }

        // Nyquist bin (index N/2) — not stored, set to zero
        fft_buf[freq_bins] = Complex { re: 0.0, im: 0.0 };

        // Conjugate symmetry: X[N-k] = conj(X[k]) for k = 1..N/2-1
        for bin in 1..freq_bins {
            let mirror = window_size - bin;
            fft_buf[mirror] = Complex {
                re: fft_buf[bin].re,
                im: -fft_buf[bin].im,
            };
        }

        ifft.process_with_scratch(&mut fft_buf, &mut scratch);

        // Overlap-add with synthesis window
        let offset = frame * hop_size;
        for i in 0..window_size {
            let sample = fft_buf[i].re * scale * hann[i];
            output[offset + i] += sample;
            window_sum[offset + i] += hann[i] * hann[i];
        }
    }

    // Normalize by the accumulated window energy to avoid amplitude modulation.
    // Where window_sum is very small, the signal is near zero anyway.
    for i in 0..output_len {
        if window_sum[i] > 1e-8 {
            output[i] /= window_sum[i];
        }
    }

    output
}

/// Reconstruct audio from a full complex spectrum (DC … Nyquist inclusive)
/// via IFFT + overlap-add with a Hann synthesis window.
///
/// Counterpart to [`compute_stft_complex`](crate::stft::compute_stft_complex).
/// Unlike [`istft`], this carries the Nyquist bin and the true complex phase,
/// so a complex mask of `1+0i` over untouched bins reconstructs the input to
/// float precision under a COLA-satisfying window pair.
///
/// # Arguments
/// - `bins`: Flat row-major complex bins (`time_frames * bins_per_frame`).
/// - `bins_per_frame`: `window_size / 2 + 1`.
/// - `time_frames`: Number of time frames.
/// - `window_size`, `hop_size`: Must match the forward transform.
pub fn istft_complex(
    bins: &[Complex<f32>],
    bins_per_frame: usize,
    time_frames: usize,
    window_size: usize,
    hop_size: usize,
) -> Vec<f32> {
    assert_eq!(bins.len(), time_frames * bins_per_frame);
    assert!(window_size > 0 && hop_size > 0);
    assert_eq!(bins_per_frame, window_size / 2 + 1);

    if time_frames == 0 {
        return vec![];
    }

    let output_len = (time_frames - 1) * hop_size + window_size;
    let mut output = vec![0.0f32; output_len];
    let mut window_sum = vec![0.0f32; output_len];

    let hann: Vec<f32> = crate::window::hann(window_size);

    let mut planner = FftPlanner::<f32>::new();
    let ifft = planner.plan_fft_inverse(window_size);
    let mut scratch = vec![Complex::default(); ifft.get_inplace_scratch_len()];
    let mut fft_buf = vec![Complex::default(); window_size];

    let scale = 1.0 / window_size as f32;
    let nyquist = window_size / 2;

    for frame in 0..time_frames {
        let src = frame * bins_per_frame;

        // DC..=Nyquist directly from storage.
        fft_buf[..bins_per_frame].copy_from_slice(&bins[src..src + bins_per_frame]);

        // Conjugate symmetry: X[N-k] = conj(X[k]) for k = 1..N/2.
        // (DC and Nyquist are their own mirror and stay as stored.)
        for bin in 1..nyquist {
            let mirror = window_size - bin;
            fft_buf[mirror] = fft_buf[bin].conj();
        }

        ifft.process_with_scratch(&mut fft_buf, &mut scratch);

        let offset = frame * hop_size;
        for i in 0..window_size {
            output[offset + i] += fft_buf[i].re * scale * hann[i];
            window_sum[offset + i] += hann[i] * hann[i];
        }
    }

    for i in 0..output_len {
        if window_sum[i] > 1e-8 {
            output[i] /= window_sum[i];
        }
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stft::{compute_stft, compute_stft_complex};
    use rustfft::num_complex::Complex;

    #[test]
    fn test_roundtrip_sine() {
        let sample_rate = 44100.0;
        let freq = 440.0;
        let duration = 0.5;
        let num_samples = (sample_rate * duration) as usize;

        let original: Vec<f32> = (0..num_samples)
            .map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / sample_rate as f32).sin())
            .collect();

        let result = compute_stft(&original, sample_rate, 2048, 512, None);

        let reconstructed = istft(
            &result.raw_magnitudes,
            &result.phases,
            result.freq_bins,
            result.time_frames,
            result.window_size,
            result.hop_size,
        );

        // Compare in the stable middle region (skip edges where windowing tapers)
        let skip = 2048; // skip one full window on each side
        let end = original.len().min(reconstructed.len()) - skip;
        assert!(end > skip, "Signal too short for roundtrip test");

        let mut max_error: f32 = 0.0;
        for i in skip..end {
            let err = (original[i] - reconstructed[i]).abs();
            if err > max_error {
                max_error = err;
            }
        }

        assert!(
            max_error < 0.05,
            "Roundtrip error too high: max_error = {max_error}"
        );
    }

    #[test]
    fn test_empty() {
        let result = istft(&[], &[], 1024, 0, 2048, 512);
        assert!(result.is_empty());
    }

    #[test]
    fn test_complex_roundtrip_near_exact() {
        // Complex STFT → unmodified iSTFT should reconstruct to float precision
        // in the COLA-stable interior (much tighter than the magnitude path).
        let sample_rate = 44100.0;
        let num_samples = (sample_rate * 0.5) as usize;
        // A mix of tones + a little noise-like detail to exercise many bins.
        let original: Vec<f32> = (0..num_samples)
            .map(|i| {
                let t = i as f32 / sample_rate as f32;
                0.6 * (2.0 * std::f32::consts::PI * 440.0 * t).sin()
                    + 0.3 * (2.0 * std::f32::consts::PI * 3500.0 * t).sin()
                    + 0.1 * (2.0 * std::f32::consts::PI * 11000.0 * t).cos()
            })
            .collect();

        let s = compute_stft_complex(&original, sample_rate, 2048, 512);
        let recon = istft_complex(
            &s.bins,
            s.bins_per_frame,
            s.time_frames,
            s.window_size,
            s.hop_size,
        );

        let skip = 2048;
        let end = original.len().min(recon.len()) - skip;
        let mut max_error = 0.0f32;
        for i in skip..end {
            max_error = max_error.max((original[i] - recon[i]).abs());
        }
        assert!(
            max_error < 1e-4,
            "complex roundtrip should be near-exact, got {max_error}"
        );
    }

    #[test]
    fn test_complex_neutral_mask_is_identity() {
        // Multiplying every bin by 1+0i must change nothing.
        let sample_rate = 44100.0;
        let original: Vec<f32> = (0..44100)
            .map(|i| (2.0 * std::f32::consts::PI * 660.0 * i as f32 / sample_rate as f32).sin())
            .collect();
        let s = compute_stft_complex(&original, sample_rate, 2048, 512);

        let masked: Vec<Complex<f32>> =
            s.bins.iter().map(|b| *b * Complex::new(1.0, 0.0)).collect();
        let a = istft_complex(
            &s.bins,
            s.bins_per_frame,
            s.time_frames,
            s.window_size,
            s.hop_size,
        );
        let b = istft_complex(
            &masked,
            s.bins_per_frame,
            s.time_frames,
            s.window_size,
            s.hop_size,
        );

        let mut max_error = 0.0f32;
        for (x, y) in a.iter().zip(b.iter()) {
            max_error = max_error.max((x - y).abs());
        }
        assert!(
            max_error < 1e-6,
            "neutral complex mask must be identity, got {max_error}"
        );
    }

    #[test]
    fn test_correction_delta_is_silent_for_neutral_and_audible_for_edit() {
        // The Stage-5 re-inject: correction = istft(S∘mask) − istft(S).
        // Neutral mask → correction is silence (so the live signal passes
        // through untouched). A real edit → correction is non-trivial.
        let sample_rate = 44100.0;
        let original: Vec<f32> = (0..44100)
            .map(|i| {
                let t = i as f32 / sample_rate as f32;
                0.5 * (2.0 * std::f32::consts::PI * 440.0 * t).sin()
                    + 0.5 * (2.0 * std::f32::consts::PI * 5000.0 * t).sin()
            })
            .collect();
        let s = compute_stft_complex(&original, sample_rate, 2048, 512);
        let base = istft_complex(
            &s.bins,
            s.bins_per_frame,
            s.time_frames,
            s.window_size,
            s.hop_size,
        );

        // Neutral mask: correction must be silent.
        let neutral: Vec<Complex<f32>> = s.bins.clone();
        let recon_n = istft_complex(
            &neutral,
            s.bins_per_frame,
            s.time_frames,
            s.window_size,
            s.hop_size,
        );
        let max_corr_neutral = base
            .iter()
            .zip(recon_n.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(
            max_corr_neutral < 1e-6,
            "neutral correction not silent: {max_corr_neutral}"
        );

        // Edit: zero out the high band (top half of bins). Correction should
        // carry real energy.
        let mut edited = s.bins.clone();
        for f in 0..s.time_frames {
            for b in (s.bins_per_frame / 2)..s.bins_per_frame {
                edited[f * s.bins_per_frame + b] = Complex::new(0.0, 0.0);
            }
        }
        let recon_e = istft_complex(
            &edited,
            s.bins_per_frame,
            s.time_frames,
            s.window_size,
            s.hop_size,
        );
        let max_corr_edit = base
            .iter()
            .zip(recon_e.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        // Orders of magnitude above the neutral silence floor (~1e-6): the
        // edit genuinely changes the signal, while neutral does not.
        assert!(
            max_corr_edit > 1e-3,
            "edit correction should carry real energy, got {max_corr_edit}"
        );
        assert!(
            max_corr_edit > max_corr_neutral * 1000.0,
            "edit correction ({max_corr_edit}) should dwarf neutral ({max_corr_neutral})"
        );
    }

    #[test]
    fn test_complex_erase_produces_silence() {
        let sample_rate = 44100.0;
        let original: Vec<f32> = (0..44100)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sample_rate as f32).sin())
            .collect();
        let s = compute_stft_complex(&original, sample_rate, 2048, 512);

        let zeroed = vec![Complex::new(0.0, 0.0); s.bins.len()];
        let recon = istft_complex(
            &zeroed,
            s.bins_per_frame,
            s.time_frames,
            s.window_size,
            s.hop_size,
        );

        let max_val = recon.iter().copied().fold(0.0f32, |a, b| a.max(b.abs()));
        assert!(
            max_val < 1e-6,
            "erased complex spectrum should be silent, got {max_val}"
        );
    }

    #[test]
    fn test_erase_produces_silence() {
        let sample_rate = 44100.0;
        let num_samples = 44100;
        let original: Vec<f32> = (0..num_samples)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sample_rate as f32).sin())
            .collect();

        let result = compute_stft(&original, sample_rate, 2048, 512, None);

        // Zero out all magnitudes (simulate erasing everything)
        let zeroed = vec![0.0f32; result.raw_magnitudes.len()];

        let reconstructed = istft(
            &zeroed,
            &result.phases,
            result.freq_bins,
            result.time_frames,
            result.window_size,
            result.hop_size,
        );

        let max_val = reconstructed
            .iter()
            .copied()
            .fold(0.0f32, |a, b| a.max(b.abs()));
        assert!(
            max_val < 1e-6,
            "Erased signal should be silent, got max = {max_val}"
        );
    }
}
