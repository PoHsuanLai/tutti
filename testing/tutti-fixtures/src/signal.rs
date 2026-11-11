//! The signals a test feeds a node or checks one against, defined once: a sine,
//! an impulse, the level readings [`peak`] and [`rms`], the bit patterns of a
//! render ([`bits`]) and a float WAV on disk ([`write_wav_f32`],
//! [`read_wav_f32`]).
//!
//! Noise is [`noise`](crate::noise) and the digest of a render is
//! [`Fnv`](crate::Fnv), both in this crate's root. Every figure here is plain
//! `f32` samples; only the arguments that name a quantity are units.
//!
//! These are definitions, not references: a test pins an analytic figure with
//! them (a sine's amplitude, an impulse's position), never a render against
//! another render.

use std::path::Path;
use tutti_types::{Amplitude, Hz, SampleRate, Samples};

/// `len` samples of a sine: `amplitude * sin(2 pi * hz * i / rate)` for sample
/// `i`, with phase 0 at sample 0 (the first sample is exactly `0.0`).
///
/// The phase is computed in `f64` and the result narrowed once, so the figure
/// does not drift with the length (an `f32` phase accumulates an error that
/// grows with `i`).
pub fn sine(hz: Hz, amplitude: Amplitude, rate: SampleRate, len: Samples) -> Vec<f32> {
    let step = std::f64::consts::TAU * f64::from(hz.get()) / rate.get();
    let amp = f64::from(amplitude.get());
    (0..len.get())
        .map(|i| (amp * (step * i as f64).sin()) as f32)
        .collect()
}

/// `len` samples of silence but for a single `1.0` at sample 0.
///
/// An empty `len` gives an empty vector.
pub fn impulse(len: Samples) -> Vec<f32> {
    impulse_at(len, Samples(0))
}

/// `len` samples of silence but for a single `1.0` at sample `at`.
///
/// # Panics
///
/// If `at` is not inside `len`.
pub fn impulse_at(len: Samples, at: Samples) -> Vec<f32> {
    let mut x = vec![0.0; len.get()];
    if len.get() > 0 || at.get() > 0 {
        x[at.get()] = 1.0;
    }
    x
}

/// The largest absolute sample of `x` (`0.0` for an empty slice).
///
/// A `NaN` sample is skipped, as `f32::max` skips it.
pub fn peak(x: &[f32]) -> f32 {
    x.iter().fold(0.0, |m, s| m.max(s.abs()))
}

/// The root mean square of **every** sample of `x`: the square root of the
/// mean of the squares, summed in `f64` and narrowed once. `0.0` for an empty
/// slice.
///
/// The denominator is `x.len()`, so a stereo reading is taken over each plane
/// on its own (or over an interleaved slice as one signal), never over frames.
pub fn rms(x: &[f32]) -> f32 {
    if x.is_empty() {
        return 0.0;
    }
    let sum: f64 = x.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    (sum / x.len() as f64).sqrt() as f32
}

/// The IEEE bit pattern of each sample of `x`: what a bit-exact comparison
/// reads, so that `0.0` and `-0.0` differ and every `NaN` payload is its own.
pub fn bits(x: &[f32]) -> Vec<u32> {
    x.iter().map(|s| s.to_bits()).collect()
}

/// [`bits`] of each plane of `planes`.
pub fn plane_bits(planes: &[Vec<f32>]) -> Vec<Vec<u32>> {
    planes.iter().map(|p| bits(p)).collect()
}

/// Writes `interleaved` (frames of `channels` samples each) to `path` as a
/// 32-bit float WAV at `rate`.
///
/// # Panics
///
/// If the file cannot be written, or `rate` is not a whole number of hertz.
pub fn write_wav_f32(path: &Path, rate: SampleRate, channels: u16, interleaved: &[f32]) {
    assert!(rate.get().fract() == 0.0, "a WAV rate is whole hertz");
    let spec = hound::WavSpec {
        channels,
        sample_rate: rate.get() as u32,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(path, spec).expect("create the wav");
    for &s in interleaved {
        w.write_sample(s).expect("write the wav");
    }
    w.finalize().expect("finalize the wav");
}

/// Reads a 32-bit float WAV from `path` into one plane per channel.
///
/// # Panics
///
/// If the file is not a readable 32-bit float WAV.
pub fn read_wav_f32(path: &Path) -> Vec<Vec<f32>> {
    let mut reader = hound::WavReader::open(path).expect("the file is a WAV");
    let channels = usize::from(reader.spec().channels);
    let samples: Vec<f32> = reader
        .samples::<f32>()
        .map(|s| s.expect("a float sample"))
        .collect();
    (0..channels)
        .map(|c| samples.iter().skip(c).step_by(channels).copied().collect())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sine starts at 0, peaks at the amplitude a quarter period in, and its
    /// RMS is the amplitude over root two across whole periods.
    #[test]
    fn a_sine_is_the_analytic_one() {
        // 12 kHz at 48 kHz: four samples a period, 0, A, 0, -A.
        let x = sine(
            Hz(12_000.0),
            Amplitude(0.5),
            SampleRate(48_000.0),
            Samples(8),
        );
        assert_eq!(x[0], 0.0);
        assert!((x[1] - 0.5).abs() < 1e-6 && (x[3] + 0.5).abs() < 1e-6);
        let y = sine(
            Hz(1_000.0),
            Amplitude(0.5),
            SampleRate(48_000.0),
            Samples(48_000),
        );
        assert!((rms(&y) - 0.5 / 2f32.sqrt()).abs() < 1e-6);
        assert!((peak(&y) - 0.5).abs() < 1e-6);
    }

    /// An impulse is one `1.0` at the asked frame and nothing else.
    #[test]
    fn an_impulse_is_one_sample() {
        assert_eq!(impulse(Samples(4)), [1.0, 0.0, 0.0, 0.0]);
        assert_eq!(impulse_at(Samples(4), Samples(2)), [0.0, 0.0, 1.0, 0.0]);
        assert!(impulse(Samples(0)).is_empty());
    }

    /// The readings of a known buffer, the empty one included.
    #[test]
    fn the_levels_of_a_known_buffer() {
        assert_eq!(peak(&[0.25, -0.75, 0.5]), 0.75);
        assert_eq!(rms(&[3.0, -3.0, 3.0, -3.0]), 3.0);
        assert_eq!((peak(&[]), rms(&[])), (0.0, 0.0));
    }

    /// Negative zero has its own bits.
    #[test]
    fn bits_tell_the_signed_zeros_apart() {
        assert_ne!(bits(&[0.0]), bits(&[-0.0]));
        assert_eq!(
            plane_bits(&[vec![1.0], vec![2.0]]),
            [[1.0f32.to_bits()], [2.0f32.to_bits()]]
        );
    }

    /// A WAV written is the WAV read back, plane by plane.
    #[test]
    fn a_wav_reads_back_what_was_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.wav");
        write_wav_f32(&path, SampleRate(48_000.0), 2, &[0.1, 0.2, 0.3, 0.4]);
        assert_eq!(read_wav_f32(&path), [vec![0.1, 0.3], vec![0.2, 0.4]]);
    }
}
