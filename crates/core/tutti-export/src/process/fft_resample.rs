//! A fixed-input FFT resampler over [`tutti_types::RealFft`].
//!
//! Overlap-add: each `fft_size_in`-frame block is zero-padded to twice its
//! length, transformed, multiplied by a windowed-sinc low-pass, truncated or
//! zero-extended to the output's bins, inverse-transformed at twice
//! `fft_size_out`, and its first half added to the previous block's second
//! half. The block sizes come from `gcd(source, target)`, so one block is a
//! whole number of the rate pair's smallest repeating unit and the ratio is
//! exact, not approximated.
//!
//! **Why this is not a dependency.** The crate this is adapted from runs
//! rustfft's auto planner, which picks AVX, SSE or NEON at run time and so
//! rounds differently on different CPUs. This runs [`RealFft`], whose inner
//! FFT is rustfft's scalar one, so the output is bit-identical across CPUs on
//! the same OS, libm and rustfft version (the window and the twiddles call the
//! C library's `sin` and `cos`, so not across libms).
//!
//! **Licence.** The block algorithm, the block-size derivation and the filter
//! design are adapted from rubato 0.16.2 by Henrik Enquist (MIT); the notice is
//! `LICENSE-rubato`, beside this file.

use crate::error::{Error, Result};
use std::f32::consts::PI;
use tutti_types::{Complex32, RealFft};

/// One block's worth of resampling: the filter, the plans and the scratch.
struct Unit {
    fft_size_in: usize,
    fft_size_out: usize,
    /// The low-pass, already transformed and scaled by `1 / (2 * fft_size_in)`,
    /// which is the inverse transform's normalisation.
    filter_f: Vec<Complex32>,
    forward: RealFft,
    inverse: RealFft,
    scratch_fw: Vec<Complex32>,
    scratch_inv: Vec<Complex32>,
    input_buf: Vec<f32>,
    input_f: Vec<Complex32>,
    output_f: Vec<Complex32>,
    output_buf: Vec<f32>,
}

/// Squared Blackman-Harris, periodic: `npoints` samples.
fn window(npoints: usize) -> Vec<f32> {
    let (a, b, c, d) = (0.35875f32, 0.48829f32, 0.14128f32, 0.01168f32);
    let n = npoints as f32;
    (0..npoints)
        .map(|x| {
            let x = x as f32;
            let w = a - b * (2.0 * PI * x / n).cos() + c * (4.0 * PI * x / n).cos()
                - d * (6.0 * PI * x / n).cos();
            w * w
        })
        .collect()
}

/// The relative cutoff (1.0 is Nyquist) at which a squared Blackman-Harris
/// windowed sinc of `npoints` taps rolls off to -6 dB at the band edge: a cubic
/// fit in `1 / npoints`, good for 32 to 2048 taps.
fn cutoff(npoints: usize) -> f32 {
    let (k1, k2, k3) = (13.745_203f32, 121.735_33f32, 5_964.163f32);
    let n = npoints as f32;
    1.0 / (k1 / n + k2 / (n * n) + k3 / (n * n * n) + 1.0)
}

/// A windowed sinc of `npoints` taps at relative cutoff `f_cutoff`, summing to 1
/// (unity gain at DC).
fn sinc_filter(npoints: usize, f_cutoff: f32) -> Vec<f32> {
    let w = window(npoints);
    let mid = (npoints / 2) as f32;
    let mut taps: Vec<f32> = w
        .iter()
        .enumerate()
        .map(|(x, &w)| {
            let v = (x as f32 - mid) * f_cutoff;
            let s = if v == 0.0 {
                1.0
            } else {
                (v * PI).sin() / (v * PI)
            };
            w * s
        })
        .collect();
    let sum: f32 = taps.iter().sum();
    taps.iter_mut().for_each(|t| *t /= sum);
    taps
}

impl Unit {
    fn new(fft_size_in: usize, fft_size_out: usize) -> Self {
        // The filter is cut at the lower of the two Nyquists: the output's
        // when downsampling (anti-alias), the input's when upsampling
        // (anti-image).
        let f_cutoff = if fft_size_in > fft_size_out {
            cutoff(fft_size_out) * fft_size_out as f32 / fft_size_in as f32
        } else {
            cutoff(fft_size_in)
        };
        let taps = sinc_filter(fft_size_in, f_cutoff);
        let scale = (2 * fft_size_in) as f32;
        let mut filter_t = vec![0.0f32; 2 * fft_size_in];
        for (f, t) in filter_t.iter_mut().zip(&taps) {
            *f = t / scale;
        }
        let forward = RealFft::new(2 * fft_size_in);
        let inverse = RealFft::new(2 * fft_size_out);
        let mut scratch_fw = forward.make_scratch();
        let scratch_inv = inverse.make_scratch();
        let mut filter_f = vec![Complex32::new(0.0, 0.0); fft_size_in + 1];
        forward.forward(&filter_t, &mut filter_f, &mut scratch_fw);
        Self {
            fft_size_in,
            fft_size_out,
            filter_f,
            forward,
            inverse,
            scratch_fw,
            scratch_inv,
            input_buf: vec![0.0; 2 * fft_size_in],
            input_f: vec![Complex32::new(0.0, 0.0); fft_size_in + 1],
            output_f: vec![Complex32::new(0.0, 0.0); fft_size_out + 1],
            output_buf: vec![0.0; 2 * fft_size_out],
        }
    }

    /// Resample one block: `wave_in` is `fft_size_in` frames, `wave_out` gets
    /// `fft_size_out`, and `overlap` carries the tail between blocks.
    fn resample_unit(&mut self, wave_in: &[f32], wave_out: &mut [f32], overlap: &mut [f32]) {
        self.input_buf[..self.fft_size_in].copy_from_slice(wave_in);
        self.input_buf[self.fft_size_in..].fill(0.0);
        self.forward
            .forward(&self.input_buf, &mut self.input_f, &mut self.scratch_fw);

        // The bins both rates share; the rest of the output spectrum is zero.
        let shared = if self.fft_size_in < self.fft_size_out {
            self.fft_size_in + 1
        } else {
            self.fft_size_out
        };
        for ((out, spec), filt) in self
            .output_f
            .iter_mut()
            .zip(&self.input_f)
            .zip(&self.filter_f)
            .take(shared)
        {
            *out = *spec * *filt;
        }
        self.output_f[shared..].fill(Complex32::new(0.0, 0.0));

        self.inverse.inverse(
            &mut self.output_f,
            &mut self.output_buf,
            &mut self.scratch_inv,
        );
        for ((out, &fresh), &carried) in wave_out
            .iter_mut()
            .zip(&self.output_buf)
            .zip(overlap.iter())
            .take(self.fft_size_out)
        {
            *out = fresh + carried;
        }
        overlap.copy_from_slice(&self.output_buf[self.fft_size_out..]);
    }
}

/// A resampler that takes a fixed number of input frames per call and returns
/// a variable number of output frames (whole blocks of `fft_size_out`).
pub(super) struct FftFixedIn {
    channels: usize,
    chunk_size_in: usize,
    fft_size_in: usize,
    fft_size_out: usize,
    overlaps: Vec<Vec<f32>>,
    input_buffers: Vec<Vec<f32>>,
    saved_frames: usize,
    unit: Unit,
}

impl FftFixedIn {
    /// `chunk_size_in` input frames per call, split into about `sub_chunks`
    /// blocks (the real count is whatever makes a block a whole number of the
    /// rate pair's repeating unit).
    pub(super) fn new(
        source_hz: usize,
        target_hz: usize,
        chunk_size_in: usize,
        sub_chunks: usize,
        channels: usize,
    ) -> Result<Self> {
        if source_hz == 0 || target_hz == 0 {
            return Err(Error::Resample(format!(
                "invalid sample rates, input: {source_hz}, output: {target_hz}"
            )));
        }
        if channels == 0 || chunk_size_in == 0 || sub_chunks == 0 {
            return Err(Error::Resample(
                "a resampler needs channels, a chunk and sub-chunks".into(),
            ));
        }
        let gcd = gcd(source_hz, target_hz);
        let min_chunk_in = source_hz / gcd;
        let fft_chunks = (chunk_size_in / sub_chunks).div_ceil(min_chunk_in).max(1);
        let fft_size_out = fft_chunks * target_hz / gcd;
        let fft_size_in = fft_chunks * source_hz / gcd;
        Ok(Self {
            channels,
            chunk_size_in,
            fft_size_in,
            fft_size_out,
            overlaps: vec![vec![0.0; fft_size_out]; channels],
            input_buffers: vec![vec![0.0; chunk_size_in + fft_size_in]; channels],
            saved_frames: 0,
            unit: Unit::new(fft_size_in, fft_size_out),
        })
    }

    /// Input frames the next call takes, per channel.
    pub(super) fn input_frames_next(&self) -> usize {
        self.chunk_size_in
    }

    /// The most output frames a call can produce, per channel.
    pub(super) fn output_frames_max(&self) -> usize {
        self.chunk_size_in.div_ceil(self.fft_size_in) * self.fft_size_out
    }

    /// Output frames at the head that are filter warm-up, not signal.
    pub(super) fn output_delay(&self) -> usize {
        self.fft_size_out / 2
    }

    /// Resample one chunk (`input_frames_next()` frames per plane) into
    /// `wave_out`, which must hold `output_frames_max()` per plane. Returns the
    /// frames produced per plane.
    pub(super) fn process_into_buffer(
        &mut self,
        wave_in: &[Vec<f32>],
        wave_out: &mut [Vec<f32>],
    ) -> Result<usize> {
        let next_saved = self.saved_frames + self.chunk_size_in;
        let ready = next_saved / self.fft_size_in;
        let needed = ready * self.fft_size_out;
        if wave_in.len() != self.channels
            || wave_out.len() != self.channels
            || wave_in.iter().any(|p| p.len() < self.chunk_size_in)
            || wave_out.iter().any(|p| p.len() < needed)
        {
            return Err(Error::Resample(format!(
                "buffers do not fit: {} channels of {} frames in, room for {needed} out",
                self.channels, self.chunk_size_in
            )));
        }

        for (buffer, input) in self.input_buffers.iter_mut().zip(wave_in) {
            buffer[self.saved_frames..next_saved].copy_from_slice(&input[..self.chunk_size_in]);
        }
        self.saved_frames = next_saved;

        for ((buffer, out), overlap) in self
            .input_buffers
            .iter()
            .zip(wave_out.iter_mut())
            .zip(self.overlaps.iter_mut())
        {
            for (in_block, out_block) in buffer
                .chunks(self.fft_size_in)
                .take(ready)
                .zip(out.chunks_mut(self.fft_size_out))
            {
                self.unit.resample_unit(in_block, out_block, overlap);
            }
        }

        // Keep the frames short of a whole block for the next call.
        let used = ready * self.fft_size_in;
        if self.saved_frames > used {
            for buffer in &mut self.input_buffers {
                buffer.copy_within(used..self.saved_frames, 0);
            }
        }
        self.saved_frames -= used;
        Ok(needed)
    }
}

fn gcd(mut a: usize, mut b: usize) -> usize {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cutoff fit at the tap counts its source documents (128 taps: 0.894,
    /// 256: 0.947, which the fit's source quotes to 3 digits), so a transcription slip in one of the three
    /// coefficients moves it.
    ///
    /// Mutation (run): `k1` 13.745_203 -> 12.0 → 128 taps gives 0.9058.
    #[test]
    fn the_cutoff_fit_matches_its_documented_values() {
        assert!((cutoff(128) - 0.8947).abs() < 0.0002, "{}", cutoff(128));
        assert!((cutoff(256) - 0.9470).abs() < 0.0005, "{}", cutoff(256));
    }

    /// The filter sums to 1 (DC gain 1), is symmetric about tap `n / 2`, and
    /// peaks there.
    ///
    /// Mutation (run): drop the `/= sum` normalisation → the sum is ~1/cutoff.
    #[test]
    fn the_sinc_has_unity_dc_gain_and_peaks_at_its_centre() {
        let taps = sinc_filter(441, 0.9);
        let sum: f32 = taps.iter().sum();
        assert!((sum - 1.0).abs() < 1e-5, "sum {sum}");
        let peak = taps
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .unwrap()
            .0;
        assert_eq!(peak, 441 / 2);
    }

    /// The block sizes are the rate pair's repeating unit times a count, so the
    /// ratio is exact: 44.1 -> 48 k is 147 -> 160 per unit, and 1024 in 2
    /// sub-chunks asks for 512 -> 4 units = 588 -> 640.
    ///
    /// Mutation (run): derive `fft_size_out` from `source_hz` → 588 -> 588.
    #[test]
    fn block_sizes_are_whole_units_of_the_rate_pair() {
        let r = FftFixedIn::new(44_100, 48_000, 1024, 2, 1).unwrap();
        assert_eq!((r.fft_size_in, r.fft_size_out), (588, 640));
        assert_eq!(r.output_delay(), 320);
        let r = FftFixedIn::new(48_000, 44_100, 1024, 2, 1).unwrap();
        assert_eq!((r.fft_size_in, r.fft_size_out), (640, 588));
    }

    /// A zero rate is refused, not divided by.
    #[test]
    fn a_zero_rate_is_an_error() {
        assert!(FftFixedIn::new(0, 48_000, 1024, 2, 1).is_err());
        assert!(FftFixedIn::new(44_100, 0, 1024, 2, 1).is_err());
    }
}
