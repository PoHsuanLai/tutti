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
    pub fn make_scratch(&self) -> Vec<Complex32> {
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
mod tests {
    use super::*;

    fn frame(n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| ((i * 7 + 3) % 11) as f32 * 0.25 - 1.0)
            .collect()
    }

    /// The textbook DFT of bin `k`, in f64.
    fn dft(frame: &[f32], k: usize) -> (f64, f64) {
        let n = frame.len();
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (t, &x) in frame.iter().enumerate() {
            let angle = -2.0 * PI * (k * t) as f64 / n as f64;
            re += x as f64 * angle.cos();
            im += x as f64 * angle.sin();
        }
        (re, im)
    }

    /// The transform pair against the textbook DFT, so the passes are checked
    /// against an analytic figure and not against another FFT. Every size up
    /// to 64, even and odd: the even ones include those that are not a
    /// multiple of four (an odd bin count, a lone centre bin).
    ///
    /// Mutation (run): drop the `bins[bins_len / 2].im = -...` line in
    /// `forward_even` -> the size-6 case fails; swap `w * 0.5` for `w` in
    /// `EvenPass::new` -> every even size fails; in the odd inverse, write
    /// `work[self.size - k] = bin` (no conjugate) -> every odd size fails.
    #[test]
    fn the_transform_is_the_dft_for_every_size() {
        for n in 1..=64usize {
            let frame = frame(n);
            let fft = RealFft::new(n);
            let mut scratch = fft.make_scratch();
            let mut bins = vec![Complex32::new(0.0, 0.0); fft.bins()];
            fft.forward(&frame, &mut bins, &mut scratch);
            for (k, bin) in bins.iter().enumerate() {
                let (re, im) = dft(&frame, k);
                assert!(
                    (bin.re as f64 - re).abs() < 1e-4 && (bin.im as f64 - im).abs() < 1e-4,
                    "size {n} bin {k}: {bin} != {re} + {im}i"
                );
            }
            assert_eq!(bins[0].im, 0.0, "size {n}: DC is real");
            if n % 2 == 0 {
                assert_eq!(bins[n / 2].im, 0.0, "size {n}: Nyquist is real");
            }

            let mut back = vec![0.0f32; n];
            fft.inverse(&mut bins, &mut back, &mut scratch);
            for (i, (&got, &want)) in back.iter().zip(&frame).enumerate() {
                assert!(
                    (got / n as f32 - want).abs() < 1e-5,
                    "size {n} sample {i}: {got} / {n} != {want}"
                );
            }
        }
    }

    /// Odd sizes the analysis kernels run (7, 255, 301) and a prime large
    /// enough to leave the small butterflies, against the DFT.
    ///
    /// Mutation (run): take `bins.len() - 1` bins from the odd forward (copy
    /// `work[1..=bins.len()]`) -> fails at every size.
    #[test]
    fn larger_odd_sizes_match_the_dft() {
        for n in [127usize, 255, 301] {
            let frame: Vec<f32> = (0..n).map(|i| (i as f32 * 0.37).sin() + 0.1).collect();
            let fft = RealFft::new(n);
            let mut scratch = fft.make_scratch();
            let mut bins = vec![Complex32::new(0.0, 0.0); fft.bins()];
            fft.forward(&frame, &mut bins, &mut scratch);
            for (k, bin) in bins.iter().enumerate() {
                let (re, im) = dft(&frame, k);
                assert!(
                    (bin.re as f64 - re).abs() < 2e-3 && (bin.im as f64 - im).abs() < 2e-3,
                    "size {n} bin {k}: {bin} != {re} + {im}i"
                );
            }
        }
    }

    /// A non-zero imaginary DC or Nyquist is not part of a real signal: the
    /// inverse must neither panic nor let those parts reach the signal. Any
    /// non-zero value does this; NaN is the worst of them. Even and odd.
    ///
    /// Mutation (run): delete the `bins[..].im = 0.0` lines in `inverse_even`
    /// -> the `0.25`/`-0.75` frame no longer equals the clean one (and the NaN
    /// frame's output is not finite) and this test fails. The odd arm's equivalent is not mutation-covered:
    /// DC is added to every output point untwiddled, so its imaginary part
    /// only ever reaches the discarded imaginary output.
    #[test]
    fn the_inverse_ignores_dc_and_nyquist_imaginary_parts() {
        for n in [64usize, 63] {
            let fft = RealFft::new(n);
            let mut scratch = fft.make_scratch();
            let mut tone = |im_dc: f32, im_ny: f32| {
                let mut bins = vec![Complex32::new(0.0, 0.0); fft.bins()];
                bins[0] = Complex32::new(1.0, im_dc);
                bins[3] = Complex32::new(2.0, -1.0);
                if n % 2 == 0 {
                    bins[n / 2] = Complex32::new(0.5, im_ny);
                }
                let mut out = vec![0.0f32; n];
                fft.inverse(&mut bins, &mut out, &mut scratch);
                out
            };
            let clean = tone(0.0, 0.0);
            assert_eq!(clean, tone(0.25, -0.75), "size {n}");
            // The DC bin alone contributes 1.0 to every sample's mean.
            let mean = clean.iter().sum::<f32>() / n as f32;
            assert!((mean - 1.0).abs() < 1e-5, "size {n}: {mean}");
            assert!(tone(f32::NAN, f32::INFINITY).iter().all(|x| x.is_finite()));
        }
    }

    /// The scratch is shared by both directions, so it must cover the larger
    /// of the two plans' needs, at every size.
    ///
    /// Not mutation-covered: rustfft 6.4's scalar out-of-place scratch length
    /// is 0 at every size, so sizing it from one direction alone still passes.
    /// This is a cheap guard for a future rustfft whose scratch is nonzero or
    /// differs by direction; it cannot fail today.
    #[test]
    fn the_scratch_covers_both_directions() {
        // Under miri the powers of two stop at 2^10: planning a 32k transform
        // there takes tens of seconds and adds no new scratch rule.
        let top = if cfg!(miri) { 10 } else { 15 };
        for n in (1..=33usize).chain((6..=top).map(|p| 1usize << p)) {
            let fft = RealFft::new(n);
            assert!(fft.scratch_len() >= fft.forward_scratch_len(), "size {n}");
            assert!(fft.scratch_len() >= fft.inverse_scratch_len(), "size {n}");
            assert_eq!(fft.make_scratch().len(), fft.scratch_len());
        }
    }

    /// Pins that the FFT is rustfft's *scalar* one, which is what makes a
    /// result bit-identical on every CPU. The auto planner (AVX, SSE or NEON)
    /// differs from scalar bitwise in nearly every bin of this frame, so a swap
    /// to it moves the digest on any machine with SIMD.
    ///
    /// The digest is a self-recorded golden, asserted only where it was
    /// recorded: the twiddles come from libm (`sin`, `cos` in f64), which can
    /// differ in the last ulp between CRTs. The input is integer-valued, with
    /// no libm in it. (It is the digest the time-stretch vocoder's copy of
    /// this code recorded, so moving the code here changed no bit.)
    ///
    /// Only the x86_64 Linux/glibc golden exists: aarch64 and the other
    /// platforms are covered by the analytic DFT tests alone, which pin
    /// correctness but not the scalar choice.
    ///
    /// Mutation (run): build the plans on `rustfft::FftPlanner` instead of
    /// `FftPlannerScalar` in `RealFft::new` -> the digest moves on an AVX
    /// machine.
    #[cfg(all(target_os = "linux", target_env = "gnu", target_arch = "x86_64"))]
    #[test]
    fn the_fft_is_scalar_so_results_match_across_cpus() {
        let n = 1024usize;
        let mut state = 12345u32;
        let frame: Vec<f32> = (0..n)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                ((state >> 16) % 2001) as f32 / 1000.0 - 1.0
            })
            .collect();
        let fft = RealFft::new(n);
        let mut scratch = fft.make_scratch();
        let mut bins = vec![Complex32::new(0.0, 0.0); n / 2 + 1];
        fft.forward(&frame, &mut bins, &mut scratch);
        let mut back = vec![0.0f32; n];
        fft.inverse(&mut bins.clone(), &mut back, &mut scratch);

        let mut digest = 0xcbf2_9ce4_8422_2325u64;
        let mut eat = |x: f32| {
            digest = (digest ^ x.to_bits() as u64).wrapping_mul(0x0000_0100_0000_01b3);
        };
        for b in &bins {
            eat(b.re);
            eat(b.im);
        }
        back.iter().copied().for_each(&mut eat);
        assert_eq!(digest, 1_168_166_466_409_112_443);
    }

    /// The odd path is scalar too: a digest of an odd-size round trip, gated
    /// like the even one.
    ///
    /// Like the even one, it exists only on x86_64 Linux/glibc; aarch64 and the
    /// other platforms are covered by the analytic DFT tests alone.
    ///
    /// Mutation (run): as above, in the odd arm of `RealFft::new`.
    #[cfg(all(target_os = "linux", target_env = "gnu", target_arch = "x86_64"))]
    #[test]
    fn the_odd_fft_is_scalar_too() {
        let n = 255usize;
        let mut state = 777u32;
        let frame: Vec<f32> = (0..n)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                ((state >> 16) % 2001) as f32 / 1000.0 - 1.0
            })
            .collect();
        let fft = RealFft::new(n);
        let mut scratch = fft.make_scratch();
        let mut bins = vec![Complex32::new(0.0, 0.0); fft.bins()];
        fft.forward(&frame, &mut bins, &mut scratch);
        let mut back = vec![0.0f32; n];
        fft.inverse(&mut bins.clone(), &mut back, &mut scratch);

        let mut digest = 0xcbf2_9ce4_8422_2325u64;
        let mut eat = |x: f32| {
            digest = (digest ^ x.to_bits() as u64).wrapping_mul(0x0000_0100_0000_01b3);
        };
        for b in &bins {
            eat(b.re);
            eat(b.im);
        }
        back.iter().copied().for_each(&mut eat);
        assert_eq!(digest, 9_419_405_582_622_979_040);
    }
}
