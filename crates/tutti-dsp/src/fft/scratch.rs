//! Reusable FFT working set.
//!
//! The one place this crate compromises on purity. A planner is expensive to
//! build — measured at **82 ns cached against 8.06 µs fresh, ~98x** (complex
//! planner, before the move to real-input plans) — so one is
//! threaded through the kernels rather than constructed per call.
//!
//! Threading it explicitly keeps the kernels honest: they stay pure *in their
//! inputs*, since the same scratch always yields the same answer, and the
//! mutation is confined to one visible parameter rather than hidden inside each
//! call site.

use super::RealFft;

use crate::Complex32;

/// The cached real-input plan plus the buffers a transform needs.
///
/// Both directions run on [`RealFft`], the engine's CPU-independent real FFT
/// (rustfft's scalar planner), so the spectra are the same bits on every CPU.
/// It does about half the work of a complex transform over a real signal at an
/// even size; an odd size runs a full-length complex transform.
pub struct FftScratch {
    /// The plan for the last size transformed; a different size re-plans.
    plan: Option<(usize, RealFft)>,
    /// The windowed real input of a forward transform.
    real: Vec<f32>,
    /// The `size / 2 + 1` input bins of an inverse transform (the plan
    /// consumes its input).
    half: Vec<Complex32>,
    scratch: Vec<Complex32>,
}

impl FftScratch {
    /// An empty working set. The plan and buffers are built lazily on the
    /// first transform and reused while the size stays the same.
    #[must_use]
    pub fn new() -> Self {
        Self {
            plan: None,
            real: Vec::new(),
            half: Vec::new(),
            scratch: Vec::new(),
        }
    }

    /// Windowed real input → complex bins, `DC..=Nyquist`.
    ///
    /// `out` receives `size / 2 + 1` bins; the rest of the spectrum is
    /// conjugate-symmetric and carries no extra information. The imaginary
    /// parts of the DC bin (and, for an even `size`, the Nyquist bin) are
    /// exactly zero.
    pub fn forward(&mut self, input: &[f32], window: &[f32], out: &mut [Complex32]) {
        let size = input.len();
        debug_assert_eq!(window.len(), size);
        debug_assert_eq!(out.len(), size / 2 + 1);

        let fft = plan_for(&mut self.plan, size);
        self.real.clear();
        self.real
            .extend(input.iter().zip(window).map(|(&s, &w)| s * w));

        let needed = fft.scratch_len();
        if self.scratch.len() < needed {
            self.scratch.resize(needed, Complex32::default());
        }
        fft.forward(&self.real, out, &mut self.scratch);
    }

    /// Complex bins `DC..=Nyquist` → real samples, conjugate half rebuilt.
    ///
    /// `out` receives `size` samples, scaled by `1/size` so a forward followed
    /// by an inverse is the identity. The imaginary parts of the DC bin (and,
    /// for an even `size`, the Nyquist bin) do not contribute to a real signal
    /// and are ignored.
    pub fn inverse(&mut self, bins: &[Complex32], out: &mut [f32]) {
        let size = out.len();
        debug_assert_eq!(bins.len(), size / 2 + 1);

        let fft = plan_for(&mut self.plan, size);
        self.half.clear();
        self.half.extend_from_slice(bins);

        let needed = fft.scratch_len();
        if self.scratch.len() < needed {
            self.scratch.resize(needed, Complex32::default());
        }
        fft.inverse(&mut self.half, out, &mut self.scratch);

        let norm = 1.0 / size as f32;
        for slot in out.iter_mut() {
            *slot *= norm;
        }
    }
}

/// The plan for `size`, built on a change of size. A free function over the
/// `plan` field alone, so the caller's other fields stay borrowable.
fn plan_for(plan: &mut Option<(usize, RealFft)>, size: usize) -> &RealFft {
    match plan {
        Some((cached, _)) if *cached == size => {}
        _ => *plan = Some((size, RealFft::new(size))),
    }
    &plan.as_ref().expect("just set").1
}

impl Default for FftScratch {
    fn default() -> Self {
        Self::new()
    }
}

impl core::fmt::Debug for FftScratch {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let size = self.plan.as_ref().map(|(size, _)| *size);
        f.debug_struct("FftScratch").field("size", &size).finish()
    }
}

#[cfg(test)]
mod tests;
