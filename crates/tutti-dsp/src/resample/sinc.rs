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
mod tests;
