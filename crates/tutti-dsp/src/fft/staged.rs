//! A [`RealFft`] whose transform runs in steps, so that a caller can spread
//! one big transform over several host blocks.
//!
//! The inner complex FFT of `size / 2` points is cut by decimation in time
//! into `ways` interleaved sub-FFTs (sub-FFT `r` takes the points `r`,
//! `r + ways`, `r + 2 * ways`, ...), each `size / 2 / ways` points, followed by
//! `log2(ways)` radix-2 combine passes. A forward step is one sub-FFT or one
//! combine pass, the gather riding with the first sub-FFT and the real-input
//! pass with the last combine. An inverse transform starts with a step of its
//! own, the real-input pass and the gather, and ends with the last combine
//! pass and the interleave. `ways == 1` is one step: [`RealFft`] itself, bit
//! for bit.
//!
//! **The split changes the rounding, not the result.** The sub-FFTs are
//! rustfft's scalar ones and the combine is plain `f32` Rust, so the output
//! is as CPU-independent as [`RealFft`]'s, but it is not the unsplit
//! transform's bits: the butterflies add in another order. Both are the same
//! transform to a few ulp of the signal's norm.
//!
//! A step reads and writes the caller's scratch, which holds the transform's
//! state between steps: the steps of one transform must run in order on one
//! scratch, and nothing else may use that scratch between them. The first
//! forward step reads `real` and the last writes `bins`; the first inverse
//! step reads `bins` and the last writes `real`, so neither is touched in
//! between. A step allocates nothing.

use super::real::{Complex32, RealFft, forward_post_pass, inverse_pre_pass, pass_twiddles};
use rustfft::{Fft as InnerFft, FftDirection, FftPlannerScalar};
use std::f64::consts::PI;
use std::sync::Arc;

/// One direction of the split transform.
struct Split {
    /// The `size / 2 / ways`-point sub-FFT.
    inner: Arc<dyn InnerFft<f32>>,
    /// The real-input pass's twiddles (`size` points).
    pass: Vec<Complex32>,
    /// Combine pass `p` merges sequences of `sub << p` points: its twiddles
    /// are `exp(-+2 pi i k / (2 * sub << p))`, `k < sub << p`.
    combine: Vec<Vec<Complex32>>,
}

impl std::fmt::Debug for Split {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Split")
            .field("passes", &self.combine.len())
            .finish_non_exhaustive()
    }
}

impl Split {
    fn new(size: usize, ways: usize, direction: FftDirection) -> Self {
        let sub = size / 2 / ways;
        let mut planner = FftPlannerScalar::<f32>::new();
        let combine = (0..ways.trailing_zeros())
            .map(|p| {
                let l = sub << p;
                (0..l)
                    .map(|k| {
                        let angle = -2.0 * PI / (2 * l) as f64 * k as f64;
                        let w = Complex32::new(angle.cos() as f32, angle.sin() as f32);
                        match direction {
                            FftDirection::Forward => w,
                            FftDirection::Inverse => w.conj(),
                        }
                    })
                    .collect()
            })
            .collect();
        Self {
            inner: planner.plan_fft(sub, direction),
            pass: pass_twiddles(size, direction),
            combine,
        }
    }
}

/// Forward and inverse plans for one real frame length, in steps.
///
/// The module docs state the stepping contract.
#[derive(Clone, Debug)]
pub struct StagedRealFft {
    fft: RealFft,
    ways: usize,
    split: Option<Arc<(Split, Split)>>,
}

impl StagedRealFft {
    /// Plans for `size` samples run as `ways` sub-FFTs: `ways` is a power of
    /// two, and at one the transform is [`RealFft`]'s single step. Allocates.
    ///
    /// # Panics
    /// If `size` is zero, `ways` is not a power of two, or `ways` is above
    /// one with a `size` that is not a multiple of `2 * ways`.
    #[must_use]
    pub fn new(size: usize, ways: usize) -> Self {
        assert!(size > 0, "BUG: a transform has at least one sample");
        assert!(ways.is_power_of_two(), "BUG: ways is a power of two");
        let split = (ways > 1).then(|| {
            assert!(
                size.is_multiple_of(2 * ways),
                "BUG: each sub-FFT has a whole number of points"
            );
            Arc::new((
                Split::new(size, ways, FftDirection::Forward),
                Split::new(size, ways, FftDirection::Inverse),
            ))
        });
        Self {
            fft: RealFft::new(size),
            ways,
            split,
        }
    }

    /// The frame length in samples.
    pub fn size(&self) -> usize {
        self.fft.size()
    }

    /// The spectrum length, `size / 2 + 1`.
    pub fn bins(&self) -> usize {
        self.fft.bins()
    }

    /// Steps of a forward transform.
    pub fn forward_steps(&self) -> usize {
        match self.split {
            None => 1,
            Some(_) => self.ways + self.ways.trailing_zeros() as usize,
        }
    }

    /// Steps of an inverse transform.
    pub fn inverse_steps(&self) -> usize {
        match self.split {
            None => 1,
            Some(_) => 1 + self.ways + self.ways.trailing_zeros() as usize,
        }
    }

    /// Complex elements of scratch a step needs.
    pub fn scratch_len(&self) -> usize {
        match &self.split {
            None => self.fft.scratch_len(),
            Some(s) => {
                let half = self.size() / 2;
                let inner =
                    s.0.inner
                        .get_outofplace_scratch_len()
                        .max(s.1.inner.get_outofplace_scratch_len());
                2 * half + inner
            }
        }
    }

    /// A zeroed scratch of [`scratch_len`](Self::scratch_len) elements.
    pub fn new_scratch(&self) -> Vec<Complex32> {
        vec![Complex32::new(0.0, 0.0); self.scratch_len()]
    }

    /// Step `step` of the forward transform of `real` into `bins`, as
    /// [`RealFft::forward`] documents; `bins` holds the result after the last
    /// step.
    ///
    /// # Panics
    /// If `step` is not below [`forward_steps`](Self::forward_steps), a slice
    /// length is wrong, or `scratch` is shorter than
    /// [`scratch_len`](Self::scratch_len).
    pub fn forward_step(
        &self,
        step: usize,
        real: &[f32],
        bins: &mut [Complex32],
        scratch: &mut [Complex32],
    ) {
        assert!(step < self.forward_steps(), "BUG: no such step");
        let Some(split) = &self.split else {
            return self.fft.forward(real, bins, scratch);
        };
        let s = &split.0;
        let half = self.size() / 2;
        assert_eq!(real.len(), self.size(), "BUG: frame is one window");
        assert_eq!(
            bins.len(),
            self.bins(),
            "BUG: spectrum is size / 2 + 1 bins"
        );
        let (a, rest) = scratch.split_at_mut(half);
        let (b, inner) = rest.split_at_mut(half);
        if step < self.ways {
            if step == 0 {
                // Point `i` of the packed signal is `(real[2i], real[2i+1])`;
                // sub-FFT `r` takes every `ways`-th one from `r`.
                gather(a, self.ways, |i| {
                    Complex32::new(real[2 * i], real[2 * i + 1])
                });
            }
            sub_fft(s, step, a, b, inner);
        } else {
            let p = step - self.ways;
            let last = p + 1 == s.combine.len();
            let (src, dst): (&[Complex32], &mut [Complex32]) = match (p.is_multiple_of(2), last) {
                (_, true) => (if p.is_multiple_of(2) { b } else { a }, &mut bins[..half]),
                (true, false) => (b, a),
                (false, false) => (a, b),
            };
            combine(s, p, self.ways, src, dst);
            if last {
                forward_post_pass(&s.pass, bins);
            }
        }
    }

    /// Step `step` of the inverse transform of `bins` into `real`,
    /// **unnormalized**, as [`RealFft::inverse`] documents; `real` holds the
    /// result after the last step. `bins` is consumed by the first step.
    ///
    /// # Panics
    /// As [`forward_step`](Self::forward_step), for the inverse's steps.
    pub fn inverse_step(
        &self,
        step: usize,
        bins: &mut [Complex32],
        real: &mut [f32],
        scratch: &mut [Complex32],
    ) {
        assert!(step < self.inverse_steps(), "BUG: no such step");
        let Some(split) = &self.split else {
            return self.fft.inverse(bins, real, scratch);
        };
        let s = &split.1;
        let half = self.size() / 2;
        assert_eq!(real.len(), self.size(), "BUG: frame is one window");
        assert_eq!(
            bins.len(),
            self.bins(),
            "BUG: spectrum is size / 2 + 1 bins"
        );
        let (a, rest) = scratch.split_at_mut(half);
        let (b, inner) = rest.split_at_mut(half);
        if step == 0 {
            // DC and Nyquist's imaginary parts are zeroed first, as in
            // `RealFft::inverse`, so a NaN there cannot reach the output.
            inverse_pre_pass(&s.pass, bins);
            gather(a, self.ways, |i| bins[i]);
        } else if step <= self.ways {
            sub_fft(s, step - 1, a, b, inner);
        } else {
            let p = step - 1 - self.ways;
            let (src, dst): (&[Complex32], &mut [Complex32]) =
                if p.is_multiple_of(2) { (b, a) } else { (a, b) };
            combine(s, p, self.ways, src, dst);
            if p + 1 == s.combine.len() {
                for (pair, point) in real.as_chunks_mut::<2>().0.iter_mut().zip(dst.iter()) {
                    pair[0] = point.re;
                    pair[1] = point.im;
                }
            }
        }
    }
}

/// `dst[r * sub + j] = point(ways * j + r)`: the sub-FFTs' inputs, one after
/// the other.
fn gather(dst: &mut [Complex32], ways: usize, point: impl Fn(usize) -> Complex32) {
    let sub = dst.len() / ways;
    for (r, chunk) in dst.chunks_exact_mut(sub).enumerate() {
        for (j, d) in chunk.iter_mut().enumerate() {
            *d = point(ways * j + r);
        }
    }
}

/// Sub-FFT `r`: `a`'s sequence `r` into `b`'s.
fn sub_fft(s: &Split, r: usize, a: &mut [Complex32], b: &mut [Complex32], inner: &mut [Complex32]) {
    let sub = s.inner.len();
    let range = r * sub..(r + 1) * sub;
    s.inner.process_outofplace_with_scratch(
        &mut a[range.clone()],
        &mut b[range],
        &mut inner[..s.inner.get_outofplace_scratch_len()],
    );
}

/// Combine pass `p`: of the `ways >> p` sequences of `sub << p` points in
/// `src`, sequence `r` (the points congruent to `r` modulo `ways >> p`) is
/// the even part and sequence `r + ways >> (p + 1)` the odd part of the
/// sequence of twice the points congruent to `r` modulo `ways >> (p + 1)`,
/// which goes to `dst`'s sequence `r`.
fn combine(s: &Split, p: usize, ways: usize, src: &[Complex32], dst: &mut [Complex32]) {
    let twiddles = &s.combine[p];
    let l = twiddles.len();
    let seqs = ways >> p;
    for r in 0..seqs / 2 {
        let even = &src[r * l..(r + 1) * l];
        let odd = &src[(r + seqs / 2) * l..(r + seqs / 2 + 1) * l];
        let (lo, hi) = dst[r * 2 * l..(r + 1) * 2 * l].split_at_mut(l);
        for ((((lo, hi), &e), &o), &w) in lo.iter_mut().zip(hi).zip(even).zip(odd).zip(twiddles) {
            let t = w * o;
            *lo = e + t;
            *hi = e - t;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signal(n: usize) -> Vec<f32> {
        let mut state = 0x9e37_79b9_u32;
        (0..n)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
            })
            .collect()
    }

    fn norm(x: &[f32]) -> f64 {
        x.iter()
            .map(|&v| f64::from(v) * f64::from(v))
            .sum::<f64>()
            .sqrt()
    }

    fn run_forward(f: &StagedRealFft, x: &[f32]) -> Vec<Complex32> {
        let mut scratch = f.new_scratch();
        let mut bins = vec![Complex32::new(0.0, 0.0); f.bins()];
        for step in 0..f.forward_steps() {
            f.forward_step(step, x, &mut bins, &mut scratch);
        }
        bins
    }

    fn run_inverse(f: &StagedRealFft, mut bins: Vec<Complex32>) -> Vec<f32> {
        let mut scratch = f.new_scratch();
        let mut out = vec![0.0f32; f.size()];
        for step in 0..f.inverse_steps() {
            f.inverse_step(step, &mut bins, &mut out, &mut scratch);
        }
        out
    }

    /// One step is [`RealFft`], bit for bit, both ways: a layout that does
    /// not split keeps its transform.
    #[test]
    fn one_way_is_the_unsplit_transform() {
        // Under miri the sizes stop at 1024: the property is size-independent
        // and miri runs this about a thousand times slower than native.
        let sizes: &[usize] = if cfg!(miri) {
            &[4, 64, 1024]
        } else {
            &[4, 64, 1024, 4096]
        };
        for &n in sizes {
            let x = signal(n);
            let f = StagedRealFft::new(n, 1);
            assert_eq!((f.forward_steps(), f.inverse_steps()), (1, 1));
            let plain = RealFft::new(n);
            let mut scratch = plain.new_scratch();
            let mut want = vec![Complex32::new(0.0, 0.0); plain.bins()];
            plain.forward(&x, &mut want, &mut scratch);
            let got = run_forward(&f, &x);
            assert!(
                got.iter()
                    .zip(&want)
                    .all(|(a, b)| a.re.to_bits() == b.re.to_bits()
                        && a.im.to_bits() == b.im.to_bits())
            );
            let mut back = vec![0.0f32; n];
            plain.inverse(&mut want, &mut back, &mut scratch);
            let again = run_inverse(&f, got);
            assert!(
                again
                    .iter()
                    .zip(&back)
                    .all(|(a, b)| a.to_bits() == b.to_bits())
            );
        }
    }

    /// A split transform is the DFT (in f64) to the unsplit transform's
    /// accuracy: every bin within `2 * eps * log2(n) * |x|_2`, which the
    /// unsplit transform meets too (a bin of a float FFT is off by about
    /// `eps * log2(n)` of the signal's norm), and the split inverse of the
    /// split forward is `n` times the signal within the same bound.
    #[test]
    fn a_split_transform_is_the_dft() {
        // The DFT reference is O(n^2) in f64 sin/cos. Under miri the 2048
        // cases are left to the native run: every way count and the
        // decimation/combine code are already hit by the smaller sizes.
        let cases: &[(usize, usize)] = if cfg!(miri) {
            &[(16, 2), (64, 2), (64, 4), (256, 4), (512, 8)]
        } else {
            &[
                (16, 2),
                (64, 2),
                (64, 4),
                (256, 4),
                (512, 8),
                (2048, 2),
                (2048, 4),
            ]
        };
        for &(n, ways) in cases {
            let x = signal(n);
            let f = StagedRealFft::new(n, ways);
            let steps = ways + ways.trailing_zeros() as usize;
            assert_eq!(f.forward_steps(), steps);
            assert_eq!(f.inverse_steps(), steps + 1);
            let bound = 2.0 * f64::from(f32::EPSILON) * (n as f64).log2() * norm(&x);
            let bins = run_forward(&f, &x);
            for (k, bin) in bins.iter().enumerate() {
                let (mut re, mut im) = (0.0f64, 0.0f64);
                for (t, &v) in x.iter().enumerate() {
                    let angle = -2.0 * PI * ((k * t) % n) as f64 / n as f64;
                    re += f64::from(v) * angle.cos();
                    im += f64::from(v) * angle.sin();
                }
                let err = (f64::from(bin.re) - re).hypot(f64::from(bin.im) - im);
                assert!(err <= bound, "n {n} ways {ways} bin {k}: {err} > {bound}");
            }
            let back = run_inverse(&f, bins);
            for (i, (&b, &v)) in back.iter().zip(&x).enumerate() {
                let err = (f64::from(b) - n as f64 * f64::from(v)).abs();
                assert!(
                    err <= bound * n as f64 / (n as f64).sqrt() * 2.0,
                    "n {n} ways {ways} sample {i}: {err}"
                );
            }
        }
    }

    /// An empty frame is refused at construction, split or not, with a clear
    /// message and not a later index error.
    #[test]
    #[should_panic(expected = "a transform has at least one sample")]
    fn a_zero_size_is_refused() {
        let _ = StagedRealFft::new(0, 4);
    }

    /// All the transform's state is in its scratch: two transforms stepped
    /// alternately, on a scratch each, give the bits of each run alone, so a
    /// caller may run one step per host block with anything between.
    #[test]
    fn the_steps_keep_their_state_in_the_scratch() {
        let f = StagedRealFft::new(512, 4);
        let (x, y) = (signal(512), signal(1024)[512..].to_vec());
        let (alone_x, alone_y) = (run_forward(&f, &x), run_forward(&f, &y));
        let (mut sx, mut sy) = (f.new_scratch(), f.new_scratch());
        let zero = Complex32::new(0.0, 0.0);
        let (mut bx, mut by) = (vec![zero; f.bins()], vec![zero; f.bins()]);
        for step in 0..f.forward_steps() {
            f.forward_step(step, &x, &mut bx, &mut sx);
            f.forward_step(step, &y, &mut by, &mut sy);
        }
        let same = |a: &[Complex32], b: &[Complex32]| {
            a.iter()
                .zip(b)
                .all(|(a, b)| a.re.to_bits() == b.re.to_bits() && a.im.to_bits() == b.im.to_bits())
        };
        assert!(same(&bx, &alone_x) && same(&by, &alone_y));
    }
}
