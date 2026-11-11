//! A real-input FFT whose arithmetic does not depend on the CPU.
//!
//! A real frame of `size` samples has `size / 2 + 1` bins (`DC..=Nyquist` for
//! an even size; an odd size has no Nyquist bin). An even size is one complex
//! FFT of `size / 2` points plus an `O(size)` pre- or post-pass, half the work
//! of a complex transform over the same signal. An odd size runs a complex FFT
//! of `size` points over the real signal.
//!
//! **The inner FFT is rustfft's scalar one, on purpose.** rustfft's auto
//! planner (what `realfft` always uses) picks AVX, SSE or NEON at run time, and
//! those round differently from scalar in most bins. `realfft` cannot be given
//! a planner, so the real-FFT passes are written out here over
//! [`FftPlannerScalar`]. Everything else is plain `f32` Rust, which the compiler
//! does not contract into FMAs. The transform's arithmetic is therefore
//! CPU-independent. Its twiddles come from the C library's `sin` and `cos`, so
//! the output is bit-identical across CPUs on the same OS, libm and rustfft
//! version, not across libms.
//!
//! **Licence.** The `RealToComplexEven` and `ComplexToRealEven` passes and the
//! twiddle construction are adapted from realfft 3.5.0 by Henrik Enquist (MIT);
//! the full notice and permission text is in `LICENSE-realfft`, beside this
//! file.
//!
//! Plans and scratch are built on the control thread. A [`RealFft`] is
//! immutable and cheap to clone (the plans are shared); the scratch is the
//! caller's, one per thread that transforms. A transform allocates nothing.

use rustfft::{Fft as InnerFft, FftDirection, FftPlannerScalar};
use std::f64::consts::PI;
use std::sync::Arc;

/// num-complex 0.4's `Complex<f32>`, re-exported from rustfft, so it follows
/// rustfft's semver: a rustfft bump that changes num-complex changes this type.
pub use rustfft::num_complex::Complex32;

/// The pass twiddles of an even `size`: `size / 4` rounded up, minus the DC
/// term. Forward carries `w^k / 2`, inverse the conjugate of `w^k`, with
/// `w = exp(-2 pi i / size)`.
pub(crate) fn pass_twiddles(size: usize, direction: FftDirection) -> Vec<Complex32> {
    (1..size.div_ceil(4))
        .map(|k| {
            // The angle in f64, narrowed once, as `realfft` does.
            let angle = -2.0 * PI / size as f64 * k as f64;
            let w = Complex32::new(angle.cos() as f32, angle.sin() as f32);
            match direction {
                FftDirection::Forward => w * 0.5,
                FftDirection::Inverse => w.conj(),
            }
        })
        .collect()
}

/// One direction of the even-size transform: the inner FFT (`size / 2` points)
/// and the pass twiddles.
struct EvenPass {
    inner: Arc<dyn InnerFft<f32>>,
    /// [`pass_twiddles`].
    twiddles: Vec<Complex32>,
}

impl EvenPass {
    fn new(size: usize, direction: FftDirection, planner: &mut FftPlannerScalar<f32>) -> Self {
        Self {
            inner: planner.plan_fft(size / 2, direction),
            twiddles: pass_twiddles(size, direction),
        }
    }

    fn scratch_len(&self) -> usize {
        self.inner.get_outofplace_scratch_len()
    }
}

enum Plans {
    Even {
        forward: EvenPass,
        inverse: EvenPass,
    },
    Odd {
        forward: Arc<dyn InnerFft<f32>>,
        inverse: Arc<dyn InnerFft<f32>>,
    },
}

/// Forward and inverse plans for one real frame length.
///
/// The module docs (above) state the determinism guarantee.
#[derive(Clone)]
pub struct RealFft {
    size: usize,
    plans: Arc<Plans>,
}

impl RealFft {
    /// Plans for a frame of `size` samples (`size >= 1`). Allocates.
    ///
    /// # Panics
    /// If `size` is zero.
    #[must_use]
    pub fn new(size: usize) -> Self {
        assert!(size >= 1, "BUG: an FFT frame has at least one sample");
        let mut planner = FftPlannerScalar::<f32>::new();
        let plans = if size.is_multiple_of(2) {
            Plans::Even {
                forward: EvenPass::new(size, FftDirection::Forward, &mut planner),
                inverse: EvenPass::new(size, FftDirection::Inverse, &mut planner),
            }
        } else {
            Plans::Odd {
                forward: planner.plan_fft(size, FftDirection::Forward),
                inverse: planner.plan_fft(size, FftDirection::Inverse),
            }
        };
        Self {
            size,
            plans: Arc::new(plans),
        }
    }

    /// The frame length in samples.
    pub fn size(&self) -> usize {
        self.size
    }

    /// The spectrum length, `size / 2 + 1`: `DC..=Nyquist` for an even size,
    /// `DC` and the positive bins for an odd one.
    pub fn bins(&self) -> usize {
        self.size / 2 + 1
    }

    /// Complex elements of scratch either direction needs. Hand
    /// [`forward`](Self::forward) and [`inverse`](Self::inverse) a slice at
    /// least this long.
    pub fn scratch_len(&self) -> usize {
        self.forward_scratch_len().max(self.inverse_scratch_len())
    }

    /// A zeroed scratch of [`scratch_len`](Self::scratch_len) elements.
    pub fn new_scratch(&self) -> Vec<Complex32> {
        vec![Complex32::new(0.0, 0.0); self.scratch_len()]
    }

    fn forward_scratch_len(&self) -> usize {
        match &*self.plans {
            Plans::Even { forward, .. } => self.size / 2 + forward.scratch_len(),
            Plans::Odd { forward, .. } => self.size + forward.get_inplace_scratch_len(),
        }
    }

    fn inverse_scratch_len(&self) -> usize {
        match &*self.plans {
            Plans::Even { inverse, .. } => self.size / 2 + inverse.scratch_len(),
            Plans::Odd { inverse, .. } => self.size + inverse.get_inplace_scratch_len(),
        }
    }

    /// `real` (`size` samples) to `bins` ([`bins`](Self::bins)). The imaginary
    /// parts of DC (and of Nyquist, for an even size) are exactly zero.
    ///
    /// # Panics
    /// If a slice length is wrong or `scratch` is shorter than
    /// [`scratch_len`](Self::scratch_len).
    pub fn forward(&self, real: &[f32], bins: &mut [Complex32], scratch: &mut [Complex32]) {
        assert_eq!(real.len(), self.size, "BUG: frame is one window");
        assert_eq!(
            bins.len(),
            self.bins(),
            "BUG: spectrum is size / 2 + 1 bins"
        );
        assert!(scratch.len() >= self.forward_scratch_len(), "BUG: scratch");
        match &*self.plans {
            Plans::Even { forward, .. } => self.forward_even(forward, real, bins, scratch),
            Plans::Odd { forward, .. } => {
                let (work, inner) = scratch.split_at_mut(self.size);
                for (point, &x) in work.iter_mut().zip(real) {
                    *point = Complex32::new(x, 0.0);
                }
                forward.process_with_scratch(work, &mut inner[..forward.get_inplace_scratch_len()]);
                bins.copy_from_slice(&work[..bins.len()]);
                bins[0].im = 0.0;
            }
        }
    }

    /// `bins` ([`bins`](Self::bins)) to `real` (`size` samples),
    /// **unnormalized**: the output is `size` times the signal, so the caller
    /// scales by `1 / size`. An even size uses `bins` as scratch, so its
    /// contents are unspecified after.
    ///
    /// The imaginary parts of DC (and of Nyquist, for an even size) do not
    /// belong to a real signal and are ignored: zeroed first, so a NaN there
    /// cannot reach the output.
    ///
    /// # Panics
    /// If a slice length is wrong or `scratch` is shorter than
    /// [`scratch_len`](Self::scratch_len).
    pub fn inverse(&self, bins: &mut [Complex32], real: &mut [f32], scratch: &mut [Complex32]) {
        assert_eq!(
            bins.len(),
            self.bins(),
            "BUG: spectrum is size / 2 + 1 bins"
        );
        assert_eq!(real.len(), self.size, "BUG: frame is one window");
        assert!(scratch.len() >= self.inverse_scratch_len(), "BUG: scratch");
        match &*self.plans {
            Plans::Even { inverse, .. } => self.inverse_even(inverse, bins, real, scratch),
            Plans::Odd { inverse, .. } => {
                let (work, inner) = scratch.split_at_mut(self.size);
                // The conjugate-symmetric spectrum, rebuilt from the half,
                // with DC's imaginary part dropped.
                work[0] = Complex32::new(bins[0].re, 0.0);
                for (k, &bin) in bins.iter().enumerate().skip(1) {
                    work[k] = bin;
                    work[self.size - k] = bin.conj();
                }
                inverse.process_with_scratch(work, &mut inner[..inverse.get_inplace_scratch_len()]);
                for (x, point) in real.iter_mut().zip(work.iter()) {
                    *x = point.re;
                }
            }
        }
    }

    fn forward_even(
        &self,
        pass: &EvenPass,
        real: &[f32],
        bins: &mut [Complex32],
        scratch: &mut [Complex32],
    ) {
        let half = self.size / 2;
        let (work, inner) = scratch.split_at_mut(half);
        for (point, pair) in work.iter_mut().zip(real.as_chunks::<2>().0) {
            *point = Complex32::new(pair[0], pair[1]);
        }
        pass.inner.process_outofplace_with_scratch(
            work,
            &mut bins[..half],
            &mut inner[..pass.scratch_len()],
        );

        forward_post_pass(&pass.twiddles, bins);
    }

    fn inverse_even(
        &self,
        pass: &EvenPass,
        bins: &mut [Complex32],
        real: &mut [f32],
        scratch: &mut [Complex32],
    ) {
        let half = self.size / 2;
        inverse_pre_pass(&pass.twiddles, bins);

        let (work, inner) = scratch.split_at_mut(half);
        pass.inner.process_outofplace_with_scratch(
            &mut bins[..half],
            work,
            &mut inner[..pass.scratch_len()],
        );
        for (pair, point) in real.as_chunks_mut::<2>().0.iter_mut().zip(work.iter()) {
            pair[0] = point.re;
            pair[1] = point.im;
        }
    }
}

/// The even-size forward transform after its inner FFT: `bins[..size / 2]`
/// holds that FFT's output, and becomes the `size / 2 + 1` bins.
pub(crate) fn forward_post_pass(twiddles: &[Complex32], bins: &mut [Complex32]) {
    let half = bins.len() - 1;
    // DC and Nyquist are the sum and difference of the first point's parts.
    let first = bins[0];
    bins[0] = Complex32::new(first.re + first.im, 0.0);
    bins[half] = Complex32::new(first.re - first.im, 0.0);

    // The rest, two mirrored bins at a time. The mirror bin's twiddle is
    // this one's with the real part negated, which halves the multiplies.
    let bins_len = bins.len();
    let (left, right) = bins.split_at_mut(bins_len / 2);
    let right_len = right.len();
    let pairs = twiddles
        .iter()
        .zip(left[1..].iter_mut())
        .zip(right[..right_len - 1].iter_mut().rev());
    for ((twiddle, low), high) in pairs {
        let sum = *low + *high;
        let diff = *low - *high;
        let twiddled_re_sum = sum * twiddle.re;
        let twiddled_im_sum = sum * twiddle.im;
        let twiddled_re_diff = diff * twiddle.re;
        let twiddled_im_diff = diff * twiddle.im;
        let half_sum_re = 0.5 * sum.re;
        let half_diff_im = 0.5 * diff.im;

        let twiddled_real = twiddled_re_sum.im + twiddled_im_diff.re;
        let twiddled_im = twiddled_im_sum.im - twiddled_re_diff.re;

        *low = Complex32::new(half_sum_re + twiddled_real, half_diff_im + twiddled_im);
        *high = Complex32::new(half_sum_re - twiddled_real, twiddled_im - half_diff_im);
    }

    // An odd bin count leaves the centre bin unpaired.
    if bins_len % 2 == 1 {
        bins[bins_len / 2].im = -bins[bins_len / 2].im;
    }
}

/// The even-size inverse transform before its inner FFT: `bins` become the
/// `size / 2` points that FFT takes, in `bins[..size / 2]`.
pub(crate) fn inverse_pre_pass(twiddles: &[Complex32], bins: &mut [Complex32]) {
    let half = bins.len() - 1;
    bins[0].im = 0.0;
    bins[half].im = 0.0;

    // Fold DC and Nyquist into the first inner point.
    let first = bins[0];
    let last = bins[half];
    let sum = first + last;
    let diff = first - last;
    bins[0] = Complex32::new(sum.re - sum.im, diff.re - diff.im);

    let bins_len = bins.len();
    let (left, right) = bins.split_at_mut(bins_len / 2);
    let right_len = right.len();
    let pairs = twiddles
        .iter()
        .zip(left[1..].iter_mut())
        .zip(right[..right_len - 1].iter_mut().rev());
    for ((twiddle, low), high) in pairs {
        let sum = *low + *high;
        let diff = *low - *high;
        let twiddled_re_sum = sum * twiddle.re;
        let twiddled_im_sum = sum * twiddle.im;
        let twiddled_re_diff = diff * twiddle.re;
        let twiddled_im_diff = diff * twiddle.im;

        let twiddled_real = twiddled_re_sum.im + twiddled_im_diff.re;
        let twiddled_im = twiddled_im_sum.im - twiddled_re_diff.re;

        *low = Complex32::new(sum.re - twiddled_real, diff.im - twiddled_im);
        *high = Complex32::new(sum.re + twiddled_real, -twiddled_im - diff.im);
    }

    // An odd bin count leaves the centre bin unpaired.
    if bins_len % 2 == 1 {
        let centre = bins[bins_len / 2];
        bins[bins_len / 2] = (centre + centre).conj();
    }
}

impl std::fmt::Debug for RealFft {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RealFft").field("size", &self.size).finish()
    }
}

#[cfg(test)]
mod tests;
