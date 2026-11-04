//! The vocoder's real-input FFT pair, planned once per window length.
//!
//! A frame is real, so its spectrum is `size / 2 + 1` bins (`DC..=Nyquist`):
//! each direction is one complex FFT of `size / 2` points plus an `O(size)`
//! pre- or post-pass, half the work of a complex transform over the same
//! signal.
//!
//! **The inner FFT is rustfft's scalar one, on purpose.** Its auto planner
//! (what `realfft` always uses) picks AVX, SSE or NEON at run time, and those
//! round differently from scalar in most bins. `realfft` cannot be given a
//! planner, so the even-length real-FFT passes are written out here over
//! [`FftPlannerScalar`](rustfft::FftPlannerScalar). Everything else is plain
//! `f32` Rust, which the compiler does not contract into FMAs. The FFT
//! arithmetic is therefore CPU-independent. The twiddles and the vocoder's
//! phase math still call the C library's sin/cos/atan2/hypot, so a render is
//! bit-identical across CPUs on the same OS and libm, not across libms. The
//! tutti-nodes convolver, tutti-analysis and the tutti-spatial HRTF still use
//! SIMD-dispatching FFTs.
//!
//! **Licence.** The `RealToComplexEven` and `ComplexToRealEven` passes and the
//! twiddle construction are adapted from realfft 3.5.0 by Henrik Enquist
//! (MIT); the full notice and permission text is in `LICENSE-realfft`.
//!
//! The plans and the scratch they need are built in [`Fft::new`], on the
//! control thread. The plans are immutable and shared between clones; the
//! scratch is per vocoder. A transform allocates nothing.

pub(crate) use rustfft::num_complex::Complex32;
use rustfft::{Fft as InnerFft, FftDirection, FftPlannerScalar};
use std::f64::consts::PI;
use std::sync::Arc;

/// One direction's inner FFT (`size / 2` points) and the pass twiddles.
struct Plan {
    inner: Arc<dyn InnerFft<f32>>,
    /// `size / 4` rounded up, minus the DC term: forward carries `w^k / 2`,
    /// inverse the conjugate of `w^k`, with `w = exp(-2 pi i / size)`.
    twiddles: Vec<Complex32>,
}

impl Plan {
    fn new(size: usize, direction: FftDirection, planner: &mut FftPlannerScalar<f32>) -> Self {
        assert!(
            size >= 2 && size.is_multiple_of(2),
            "BUG: window is even, got {size}"
        );
        let count = size.div_ceil(4);
        let twiddles = (1..count)
            .map(|k| {
                // The angle in f64, narrowed once, as `realfft` does.
                let angle = -2.0 * PI / size as f64 * k as f64;
                let w = Complex32::new(angle.cos() as f32, angle.sin() as f32);
                match direction {
                    FftDirection::Forward => w * 0.5,
                    FftDirection::Inverse => w.conj(),
                }
            })
            .collect();
        Self {
            inner: planner.plan_fft(size / 2, direction),
            twiddles,
        }
    }

    fn scratch_len(&self) -> usize {
        self.inner.get_outofplace_scratch_len()
    }
}

/// Forward and inverse plans for one window length, and the scratch both use.
pub(super) struct Fft {
    size: usize,
    forward: Arc<Plan>,
    inverse: Arc<Plan>,
    /// The inner FFT's scratch: at least both plans' needs.
    scratch: Vec<Complex32>,
    /// `size / 2` complex points: the real frame packed in pairs on the way
    /// in, the inner FFT's output on the way out.
    work: Vec<Complex32>,
}

impl Fft {
    /// Plans for a window of `size` samples (an even length, which every
    /// [`FftSize`](super::FftSize) is).
    pub(super) fn new(size: usize) -> Self {
        let mut planner = FftPlannerScalar::<f32>::new();
        let forward = Arc::new(Plan::new(size, FftDirection::Forward, &mut planner));
        let inverse = Arc::new(Plan::new(size, FftDirection::Inverse, &mut planner));
        Self::from_plans(size, forward, inverse)
    }

    /// The same plans over fresh scratch.
    pub(super) fn clone_fresh(&self) -> Self {
        Self::from_plans(
            self.size,
            Arc::clone(&self.forward),
            Arc::clone(&self.inverse),
        )
    }

    fn from_plans(size: usize, forward: Arc<Plan>, inverse: Arc<Plan>) -> Self {
        let len = forward.scratch_len().max(inverse.scratch_len());
        Self {
            size,
            forward,
            inverse,
            scratch: vec![Complex32::new(0.0, 0.0); len],
            work: vec![Complex32::new(0.0, 0.0); size / 2],
        }
    }

    /// The scratch length the forward inner FFT needs.
    #[cfg(test)]
    pub(super) fn forward_scratch_len(&self) -> usize {
        self.forward.scratch_len()
    }

    /// The scratch length the inverse inner FFT needs.
    #[cfg(test)]
    pub(super) fn inverse_scratch_len(&self) -> usize {
        self.inverse.scratch_len()
    }

    /// The scratch length held, which must cover both of the above.
    #[cfg(test)]
    pub(super) fn scratch_len(&self) -> usize {
        self.scratch.len()
    }

    /// `real` (`size` samples) to `bins` (`size / 2 + 1`), `DC..=Nyquist`.
    /// DC's and Nyquist's imaginary parts are zero.
    pub(super) fn forward(&mut self, real: &[f32], bins: &mut [Complex32]) {
        let half = self.size / 2;
        assert_eq!(real.len(), self.size, "BUG: frame is one window");
        assert_eq!(bins.len(), half + 1, "BUG: spectrum is size / 2 + 1 bins");

        for (point, pair) in self.work.iter_mut().zip(real.as_chunks::<2>().0) {
            *point = Complex32::new(pair[0], pair[1]);
        }
        let plan = &*self.forward;
        let needed = plan.scratch_len();
        plan.inner.process_outofplace_with_scratch(
            &mut self.work,
            &mut bins[..half],
            &mut self.scratch[..needed],
        );

        // DC and Nyquist are the sum and difference of the first point's parts.
        let first = bins[0];
        bins[0] = Complex32::new(first.re + first.im, 0.0);
        bins[half] = Complex32::new(first.re - first.im, 0.0);

        // The rest, two mirrored bins at a time. The mirror bin's twiddle is
        // this one's with the real part negated, which halves the multiplies.
        let bins_len = bins.len();
        let (left, right) = bins.split_at_mut(bins_len / 2);
        let right_len = right.len();
        let pairs = plan
            .twiddles
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

    /// `bins` (`size / 2 + 1`) to `real` (`size` samples), **unnormalized**
    /// (the output is `size` times the signal; the caller scales). The plan
    /// uses `bins` as scratch, so its contents are unspecified after.
    ///
    /// The imaginary parts of DC and Nyquist do not belong to a real signal and
    /// are ignored (zeroed first, so a NaN there cannot reach the output).
    pub(super) fn inverse(&mut self, bins: &mut [Complex32], real: &mut [f32]) {
        let half = self.size / 2;
        assert_eq!(bins.len(), half + 1, "BUG: spectrum is size / 2 + 1 bins");
        assert_eq!(real.len(), self.size, "BUG: frame is one window");

        bins[0].im = 0.0;
        bins[half].im = 0.0;

        // Fold DC and Nyquist into the first inner point.
        let first = bins[0];
        let last = bins[half];
        let sum = first + last;
        let diff = first - last;
        bins[0] = Complex32::new(sum.re - sum.im, diff.re - diff.im);

        let plan = &*self.inverse;
        let bins_len = bins.len();
        let (left, right) = bins.split_at_mut(bins_len / 2);
        let right_len = right.len();
        let pairs = plan
            .twiddles
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

        let needed = plan.scratch_len();
        plan.inner.process_outofplace_with_scratch(
            &mut bins[..half],
            &mut self.work,
            &mut self.scratch[..needed],
        );
        for (pair, point) in real.as_chunks_mut::<2>().0.iter_mut().zip(&self.work) {
            pair[0] = point.re;
            pair[1] = point.im;
        }
    }
}
