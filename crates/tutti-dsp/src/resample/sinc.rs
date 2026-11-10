//! The windowed-sinc kernel: a low-pass impulse response, evaluated at any
//! fractional offset.
//!
//! This is the kernel of band-limited interpolation, which reads a signal
//! between its samples: `x(t) = Σₘ x[m] · kernel(t − m)`. It is **not**
//! [`FftResampler`](super::FftResampler), which converts a whole block between
//! two integer rates in the frequency domain; the two share nothing but the
//! word "resample". A caller that needs a response evaluated at arbitrary
//! offsets (the HRIR set's rate change, where each output tap sits at its own
//! fractional input position) owns the loop and the batching and asks this for
//! the weights.

use crate::CosineWindow;

/// A sinc low-pass cut at a fraction of the Nyquist rate, tapered by a
/// [`CosineWindow`] over a fixed number of zero crossings each side.
///
/// Offsets are in input samples. The kernel is centred (`weight(0)` is the
/// peak, `weight(-t) == weight(t)`) so it delays nothing, and normalised so
/// that it integrates to 1: `Σₘ weight(t − m)` is 1 (to the window's
/// truncation) for a cutoff at or below 1, so a constant signal keeps its
/// level. Evaluated in `f64`; the sample sums that use it stay in the caller.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowedSinc {
    /// The cutoff as a fraction of Nyquist, in `(0, ∞)`. A plain ratio:
    /// no unit covers it.
    cutoff: f64,
    /// The half-width of the support, in input samples.
    half: f64,
    window: CosineWindow,
}

impl WindowedSinc {
    /// A kernel with `cutoff` (a fraction of Nyquist, so 1.0 passes up to it)
    /// and `crossings` zero crossings of the sinc each side of the centre,
    /// shaped by `window`.
    ///
    /// The support's half-width is `crossings / cutoff` samples: a lower cutoff
    /// is a wider kernel. `None` for a cutoff or crossing count that is not
    /// finite and positive.
    #[must_use]
    pub fn new(cutoff: f64, crossings: f64, window: CosineWindow) -> Option<Self> {
        let ok = |v: f64| v.is_finite() && v > 0.0;
        (ok(cutoff) && ok(crossings)).then(|| Self {
            cutoff,
            half: crossings / cutoff,
            window,
        })
    }

    /// The half-width of the support, in input samples: the kernel is zero
    /// beyond `±half_width`.
    #[inline]
    #[must_use]
    pub fn half_width(&self) -> f64 {
        self.half
    }

    /// The kernel at offset `t` input samples from its centre, or 0 beyond
    /// [`half_width`](Self::half_width).
    #[inline]
    #[must_use]
    pub fn weight(&self, t: f64) -> f64 {
        if t.abs() > self.half {
            return 0.0;
        }
        let x = std::f64::consts::PI * self.cutoff * t;
        let sinc = if x == 0.0 { 1.0 } else { x.sin() / x };
        self.cutoff * sinc * self.window.centred(t / (2.0 * self.half))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kernel(cutoff: f64) -> WindowedSinc {
        WindowedSinc::new(cutoff, 32.0, CosineWindow::BLACKMAN).unwrap()
    }

    /// The centre is `cutoff` (sinc and window are 1 there), and at cutoff 1
    /// every other integer offset is a sinc zero crossing, so the kernel is
    /// an exact interpolator: it reads samples back unchanged.
    ///
    /// Mutation (run): `x.sin() / x` -> `x.cos() / x` fails the zeros;
    /// dropping the `self.cutoff *` factor fails the centre at 0.95.
    #[test]
    fn the_centre_is_the_cutoff_and_integers_are_zero_crossings() {
        let k = kernel(1.0);
        assert!((k.weight(0.0) - 1.0).abs() < 1e-7);
        for m in 1..32 {
            assert!(k.weight(f64::from(m)).abs() < 1e-7, "offset {m}");
        }
        assert!((kernel(0.95).weight(0.0) - 0.95).abs() < 1e-7);
    }

    /// Unity DC gain: the kernel's samples at any fractional offset sum to 1
    /// (cutoff 1 and 0.95 alike), so a constant keeps its level. The
    /// truncation to 32 crossings leaves the sum within a few 1e-4.
    ///
    /// Mutation (run): scaling the weight by 1.01 fails it; a rectangular
    /// window in place of Blackman widens the error far past the bound.
    #[test]
    fn the_samples_at_any_offset_sum_to_one() {
        for cutoff in [1.0, 0.95] {
            let k = kernel(cutoff);
            let reach = k.half_width().ceil() as i64 + 1;
            for frac in [0.0, 0.25, 0.5, 0.731] {
                let sum: f64 = (-reach..=reach).map(|m| k.weight(frac - m as f64)).sum();
                assert!(
                    (sum - 1.0).abs() < 5e-4,
                    "cutoff {cutoff}, frac {frac}: {sum}"
                );
            }
        }
    }

    /// The kernel is even, and zero at and beyond its half-width, where the
    /// Blackman window ends.
    ///
    /// Mutation (run): `t.abs() > self.half` -> `> 2.0 * self.half` fails the
    /// cut beyond the half-width. (The weight *at* the half-width is 0 through
    /// the sinc's last zero crossing as well as the window, so it pins the
    /// support, not the window; the window's scale is pinned by
    /// `the_samples_at_any_offset_sum_to_one`, which fails under
    /// `t / self.half`.) Symmetry is exact by construction, so no mutation
    /// of it is claimed.
    #[test]
    fn it_is_even_and_ends_at_the_half_width() {
        let k = kernel(0.95);
        for t in [0.3, 1.7, 5.25, 20.0] {
            assert_eq!(k.weight(t), k.weight(-t));
        }
        let h = k.half_width();
        assert!(k.weight(h).abs() < 1e-7, "window end: {}", k.weight(h));
        assert_eq!(k.weight(h * 1.001), 0.0);
        assert_eq!(k.weight(-h * 1.001), 0.0);
    }

    /// A kernel needs a finite positive cutoff and crossing count.
    #[test]
    fn a_bad_shape_is_refused() {
        let w = CosineWindow::BLACKMAN;
        for (c, n) in [
            (0.0, 8.0),
            (-1.0, 8.0),
            (f64::NAN, 8.0),
            (1.0, 0.0),
            (1.0, f64::INFINITY),
        ] {
            assert!(WindowedSinc::new(c, n, w).is_none(), "{c} {n}");
        }
    }
}
