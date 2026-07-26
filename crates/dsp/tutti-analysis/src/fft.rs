//! Reusable FFT working set.
//!
//! The one place this crate compromises on purity. A planner is expensive to
//! build — measured at **82 ns cached against 8.06 µs fresh, ~98x** — and the
//! old code constructed one at nine sites, six of them inside functions that
//! ran per call.
//!
//! Threading it explicitly keeps the kernels honest: they are pure *in their
//! inputs*, since the same scratch always yields the same answer, and the
//! mutation is confined to one visible parameter instead of being smeared
//! across nine hidden ones.

use rustfft::{Fft, FftPlanner};
use std::sync::Arc;

use crate::Complex;

/// Cached planner plus the buffers a forward or inverse transform needs.
pub struct FftScratch {
    planner: FftPlanner<f32>,
    forward: Option<(usize, Arc<dyn Fft<f32>>)>,
    inverse: Option<(usize, Arc<dyn Fft<f32>>)>,
    buffer: Vec<Complex>,
    scratch: Vec<Complex>,
}

impl FftScratch {
    pub fn new() -> Self {
        Self {
            planner: FftPlanner::new(),
            forward: None,
            inverse: None,
            buffer: Vec::new(),
            scratch: Vec::new(),
        }
    }

    /// Windowed real input → complex bins, `DC..=Nyquist`.
    ///
    /// `out` receives `size / 2 + 1` bins; the rest of the spectrum is
    /// conjugate-symmetric and carries no extra information.
    pub fn forward(&mut self, input: &[f32], window: &[f32], out: &mut [Complex]) {
        let size = input.len();
        debug_assert_eq!(window.len(), size);
        debug_assert_eq!(out.len(), size / 2 + 1);

        let fft = self.forward_plan(size);
        self.buffer.clear();
        self.buffer
            .extend(input.iter().zip(window).map(|(&s, &w)| Complex {
                re: s * w,
                im: 0.0,
            }));

        let needed = fft.get_inplace_scratch_len();
        if self.scratch.len() < needed {
            self.scratch.resize(needed, Complex::default());
        }
        fft.process_with_scratch(&mut self.buffer, &mut self.scratch[..needed]);

        out.copy_from_slice(&self.buffer[..out.len()]);
    }

    /// Complex bins `DC..=Nyquist` → real samples, conjugate half rebuilt.
    ///
    /// `out` receives `size` samples, scaled by `1/size` so a forward followed
    /// by an inverse is the identity.
    pub fn inverse(&mut self, bins: &[Complex], out: &mut [f32]) {
        let size = out.len();
        debug_assert_eq!(bins.len(), size / 2 + 1);

        let fft = self.inverse_plan(size);
        self.buffer.clear();
        self.buffer.resize(size, Complex::default());

        // Rebuild the full spectrum: the upper half mirrors the lower one,
        // conjugated. Callers only ever hold DC..=Nyquist.
        self.buffer[..bins.len()].copy_from_slice(bins);
        for i in 1..size.div_ceil(2) {
            self.buffer[size - i] = bins[i].conj();
        }

        let needed = fft.get_inplace_scratch_len();
        if self.scratch.len() < needed {
            self.scratch.resize(needed, Complex::default());
        }
        fft.process_with_scratch(&mut self.buffer, &mut self.scratch[..needed]);

        let norm = 1.0 / size as f32;
        for (slot, bin) in out.iter_mut().zip(&self.buffer) {
            *slot = bin.re * norm;
        }
    }

    fn forward_plan(&mut self, size: usize) -> Arc<dyn Fft<f32>> {
        match &self.forward {
            Some((cached, fft)) if *cached == size => Arc::clone(fft),
            _ => {
                let fft = self.planner.plan_fft_forward(size);
                self.forward = Some((size, Arc::clone(&fft)));
                fft
            }
        }
    }

    fn inverse_plan(&mut self, size: usize) -> Arc<dyn Fft<f32>> {
        match &self.inverse {
            Some((cached, fft)) if *cached == size => Arc::clone(fft),
            _ => {
                let fft = self.planner.plan_fft_inverse(size);
                self.inverse = Some((size, Arc::clone(&fft)));
                fft
            }
        }
    }
}

impl Default for FftScratch {
    fn default() -> Self {
        Self::new()
    }
}

impl core::fmt::Debug for FftScratch {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FftScratch")
            .field("forward_size", &self.forward.as_ref().map(|(n, _)| n))
            .field("inverse_size", &self.inverse.as_ref().map(|(n, _)| n))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::window::hann;

    #[test]
    fn forward_finds_a_tone_in_the_right_bin() {
        let size = 1024;
        let bin = 64usize;
        let input: Vec<f32> = (0..size)
            .map(|i| (2.0 * core::f32::consts::PI * bin as f32 * i as f32 / size as f32).sin())
            .collect();
        let window = vec![1.0f32; size]; // rectangular, so the peak is sharp

        let mut fft = FftScratch::new();
        let mut out = vec![Complex::default(); size / 2 + 1];
        fft.forward(&input, &window, &mut out);

        let peak = out
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.norm().partial_cmp(&b.norm()).unwrap())
            .map(|(i, _)| i)
            .unwrap();
        assert_eq!(peak, bin);
    }

    /// Forward then inverse is the identity, up to float precision.
    #[test]
    fn forward_and_inverse_round_trip() {
        let size = 256;
        let input: Vec<f32> = (0..size).map(|i| (i as f32 / 7.0).sin() * 0.5).collect();
        let window = vec![1.0f32; size];

        let mut fft = FftScratch::new();
        let mut bins = vec![Complex::default(); size / 2 + 1];
        fft.forward(&input, &window, &mut bins);

        let mut out = vec![0.0f32; size];
        fft.inverse(&bins, &mut out);

        for (i, (a, b)) in input.iter().zip(&out).enumerate() {
            assert!((a - b).abs() < 1e-4, "sample {i}: {a} != {b}");
        }
    }

    /// The property that makes "pure in its inputs" true despite the `&mut`.
    #[test]
    fn reused_scratch_gives_identical_results() {
        let size = 512;
        let a: Vec<f32> = (0..size).map(|i| (i as f32 / 11.0).sin()).collect();
        let b: Vec<f32> = (0..size).map(|i| (i as f32 / 3.0).cos()).collect();
        let window = hann(size);

        let mut shared = FftScratch::new();
        let mut first = vec![Complex::default(); size / 2 + 1];
        shared.forward(&a, &window, &mut first);
        // Run something else through the same scratch, then repeat.
        let mut other = vec![Complex::default(); size / 2 + 1];
        shared.forward(&b, &window, &mut other);
        let mut again = vec![Complex::default(); size / 2 + 1];
        shared.forward(&a, &window, &mut again);

        assert_eq!(first, again);

        // And a fresh scratch agrees with the reused one.
        let mut fresh = FftScratch::new();
        let mut clean = vec![Complex::default(); size / 2 + 1];
        fresh.forward(&a, &window, &mut clean);
        assert_eq!(first, clean);
    }

    /// Alternating sizes must not return a stale plan.
    #[test]
    fn switching_sizes_replans() {
        let mut fft = FftScratch::new();
        for size in [64usize, 128, 64, 256, 64] {
            let input: Vec<f32> = (0..size).map(|i| i as f32 / size as f32).collect();
            let window = vec![1.0f32; size];
            let mut out = vec![Complex::default(); size / 2 + 1];
            fft.forward(&input, &window, &mut out);

            let mut back = vec![0.0f32; size];
            fft.inverse(&out, &mut back);
            for (a, b) in input.iter().zip(&back) {
                assert!((a - b).abs() < 1e-4, "round trip failed at size {size}");
            }
        }
    }
}
