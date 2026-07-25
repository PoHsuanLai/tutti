//! Audio resampling using rubato
//!
//! Provides high-quality sample rate conversion with SIMD optimization.

#[cfg(any(feature = "wav", feature = "flac"))]
use crate::error::{Error, Result};
#[cfg(any(feature = "wav", feature = "flac"))]
use rubato::{FftFixedIn, Resampler};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ResampleQuality {
    Fast,
    #[default]
    Medium,
    High,
    Best,
}

#[cfg(any(feature = "wav", feature = "flac"))]
impl ResampleQuality {
    fn chunk_size(&self) -> usize {
        match self {
            Self::Fast => 512,
            Self::Medium => 1024,
            Self::High => 2048,
            Self::Best => 4096,
        }
    }

    fn sub_chunks(&self) -> usize {
        match self {
            Self::Fast => 1,
            Self::Medium => 2,
            Self::High => 4,
            Self::Best => 8,
        }
    }
}

/// Resample `planes.len()` channel planes from `source_rate` to `target_rate` in
/// lockstep. rubato is natively multichannel — the resampler is built for
/// `planes.len()` channels and every plane is processed together each chunk, so
/// mono, stereo, and surround all take the same path. Every plane must have the
/// same length (they are the deinterleaved channels of one signal). Returns the
/// resampled planes in the same channel order.
#[cfg(any(feature = "wav", feature = "flac"))]
pub(crate) fn resample_planar(
    planes: &[Vec<f32>],
    source_rate: u32,
    target_rate: u32,
    quality: ResampleQuality,
) -> Result<Vec<Vec<f32>>> {
    let channels = planes.len();
    if channels == 0 {
        return Ok(Vec::new());
    }
    if source_rate == target_rate {
        return Ok(planes.to_vec());
    }

    let input_frames = planes[0].len();
    if planes.iter().any(|p| p.len() != input_frames) {
        return Err(Error::InvalidData(
            "Channel planes have different lengths".into(),
        ));
    }

    let chunk_size = quality.chunk_size();
    let sub_chunks = quality.sub_chunks();

    let mut resampler = FftFixedIn::<f32>::new(
        source_rate as usize,
        target_rate as usize,
        chunk_size,
        sub_chunks,
        channels,
    )?;

    let expected_output_frames =
        (input_frames as f64 * target_rate as f64 / source_rate as f64).ceil() as usize;

    let mut outputs: Vec<Vec<f32>> = (0..channels)
        .map(|_| Vec::with_capacity(expected_output_frames + chunk_size))
        .collect();

    let mut pos = 0;
    while pos < input_frames {
        let remaining = input_frames - pos;
        let frames_to_process = remaining.min(chunk_size);

        let input_frames_needed = resampler.input_frames_next();
        let actual_frames = if remaining < input_frames_needed {
            input_frames_needed
        } else {
            frames_to_process.max(input_frames_needed)
        };

        let copy_frames = frames_to_process.min(remaining);
        let input_channels: Vec<Vec<f32>> = planes
            .iter()
            .map(|plane| {
                let mut chunk = vec![0.0f32; actual_frames];
                chunk[..copy_frames].copy_from_slice(&plane[pos..pos + copy_frames]);
                chunk
            })
            .collect();

        let output = resampler.process(&input_channels, None)?;
        for (out, produced) in outputs.iter_mut().zip(&output) {
            out.extend_from_slice(produced);
        }

        pos += actual_frames;
    }

    let final_length = expected_output_frames.min(outputs[0].len());
    for out in outputs.iter_mut() {
        out.truncate(final_length);
    }

    Ok(outputs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_resample_needed() {
        let planes = vec![vec![1.0, 2.0, 3.0], vec![4.0, 5.0, 6.0]];

        let out = resample_planar(&planes, 44100, 44100, ResampleQuality::Fast).unwrap();

        assert_eq!(out, planes);
    }

    #[test]
    fn test_resample_upsample() {
        // Generate a simple sine wave at 1000 Hz
        let sample_rate = 44100;
        let target_rate = 48000;
        let duration_samples = 4410; // 0.1 seconds

        let left: Vec<f32> = (0..duration_samples)
            .map(|i| (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / sample_rate as f32).sin())
            .collect();
        let planes = vec![left.clone(), left];

        let out =
            resample_planar(&planes, sample_rate, target_rate, ResampleQuality::Medium).unwrap();

        // Check output length is approximately correct
        let expected_length =
            (duration_samples as f64 * target_rate as f64 / sample_rate as f64) as usize;
        assert!(
            (out[0].len() as i32 - expected_length as i32).abs() < 100,
            "Output length {} differs too much from expected {}",
            out[0].len(),
            expected_length
        );
        assert_eq!(out[0].len(), out[1].len());
    }

    #[test]
    fn test_resample_quad() {
        // Four distinct planes resample together and stay aligned.
        let sample_rate = 44100;
        let target_rate = 48000;
        let n = 4410;
        let planes: Vec<Vec<f32>> = (0..4)
            .map(|ch| {
                (0..n)
                    .map(|i| {
                        (2.0 * std::f32::consts::PI * (500.0 * (ch + 1) as f32) * i as f32
                            / sample_rate as f32)
                            .sin()
                    })
                    .collect()
            })
            .collect();

        let out =
            resample_planar(&planes, sample_rate, target_rate, ResampleQuality::Medium).unwrap();

        assert_eq!(out.len(), 4);
        let len = out[0].len();
        assert!(out.iter().all(|p| p.len() == len));
    }

    #[test]
    fn test_mismatched_channel_lengths() {
        let planes = vec![vec![1.0, 2.0, 3.0], vec![4.0, 5.0]];

        let result = resample_planar(&planes, 44100, 48000, ResampleQuality::Fast);
        assert!(result.is_err());
    }
}
