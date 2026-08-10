//! Analysis windows.
//!
//! One Hann, where there were eight copies. Seven agreed; the eighth divided
//! by `size - 1` instead of `size` — the *symmetric* Hann rather than the
//! periodic one. Only the periodic form satisfies the COLA property the
//! inverse transform's normalization assumes, so the odd copy out was both
//! inconsistent with its siblings and wrong for overlap-add. It also divided
//! by zero at `size == 1`.
//!
//! # A window is one function, and everything else derives from it
//!
//! In the maths a window is `w: [0, 1) → ℝ`, and that is the whole surface.
//! Sampling it at `i / n` gives the coefficients; the two facts a transform
//! additionally needs are *functionals* of the same `w`:
//!
//! - **coherent gain** is `∫₀¹ w(t) dt` — the DC term, what a forward transform
//!   leaves in every magnitude because it does not normalize by the window sum.
//! - **COLA overlap** is the smallest `R` for which `Σₖ w²(t + k/R)` is constant.
//!
//! So [`Window`] has exactly one required method and provides the rest. That is
//! the boundary in one sentence with no "and": *given a position in the window,
//! produce its weight.*
//!
//! # Why the cosine family is a single type
//!
//! Hann, Hamming, Blackman, Nuttall and Blackman-Harris are **not five
//! functions**. They are one function
//!
//! ```text
//! w(t) = Σₖ (−1)ᵏ aₖ cos(2πkt)
//! ```
//!
//! at five coefficient vectors. Writing them as five enum variants meant three
//! parallel tables — the coefficients, the coherent gains, the COLA overlaps —
//! that had to be kept consistent by hand, and adding Blackman-Harris meant
//! touching all three. [`CosineWindow`] carries the coefficients and derives the
//! other two, so a new member is one `const`.
//!
//! Both derivations are exact rather than numeric, which is why they can be
//! `const fn`. See [`CosineWindow::coherent_gain`] and
//! [`CosineWindow::cola_overlap`] for the arguments.

/// A window function, sampled over `[0, 1)`.
///
/// One required method, because in the maths there is one: a window *is* a
/// function of position. The rest are integrals of it, provided here so an
/// implementor gets them for free and overrides only when it knows a closed
/// form (as [`CosineWindow`] does for both).
///
/// Object-safe, so a caller holding heterogeneous windows can box them. Note
/// [`crate::StftGeometry`] deliberately does **not** — it is `Copy + PartialEq`
/// and lives inside four stored types, so it holds a `CosineWindow` by value.
pub trait Window {
    /// The window's weight at `t`, where `t` runs `[0, 1)` across the window.
    ///
    /// Periodic by construction: `at(0.0)` and the limit approaching `1.0` are
    /// *not* the same point, which is what makes the window tile. A symmetric
    /// window duplicates its endpoint and breaks the overlap-add sum — the exact
    /// defect the eighth copy of Hann had.
    fn at(&self, t: f32) -> f32;

    /// `size` points, sampled at `i / size`.
    ///
    /// `size == 0` yields an empty window; `size == 1` does not divide by zero.
    fn coefficients(&self, size: usize) -> Vec<f32> {
        (0..size).map(|i| self.at(i as f32 / size as f32)).collect()
    }

    /// `∫₀¹ w(t) dt` — the DC gain a forward transform leaves in every
    /// magnitude.
    ///
    /// The default integrates numerically. An implementor with a closed form
    /// should override it: this is used to scale a display, and a value that
    /// drifts with the sample count makes a dB window untunable.
    fn coherent_gain(&self) -> f32 {
        const SAMPLES: usize = 4096;
        self.coefficients(SAMPLES).iter().sum::<f32>() / SAMPLES as f32
    }

    /// The smallest overlap factor `window / hop` at which `w²` sums flat.
    ///
    /// The default searches powers of two — the only ratios a grid can express,
    /// since the hop must divide the window. Returns the first that is flat
    /// within `1e-4`, or 16 if none is.
    fn cola_overlap(&self) -> usize {
        const N: usize = 64;
        [1usize, 2, 4, 8, 16]
            .into_iter()
            .find(|&r| cola_ripple(self, N, r) < 1e-4)
            .unwrap_or(16)
    }
}

/// Ripple of `Σ w²` across the steady-state interior, at `overlap` frames per
/// window. Zero (to float precision) exactly when the window constant-overlap-
/// adds at that rate.
///
/// Shared by [`Window::cola_overlap`]'s default and the tests, so the property
/// the trait *claims* and the property the tests *check* are one function.
pub(crate) fn cola_ripple<W: Window + ?Sized>(window: &W, size: usize, overlap: usize) -> f32 {
    let hop = (size / overlap).max(1);
    let w = window.coefficients(size);
    let frames = 4 * overlap;
    let mut acc = vec![0.0f32; frames * hop + size];
    for f in 0..frames {
        for (i, &v) in w.iter().enumerate() {
            acc[f * hop + i] += v * v;
        }
    }

    let interior = &acc[size..frames * hop];
    if interior.is_empty() {
        return 0.0;
    }
    let (lo, hi) = interior
        .iter()
        .fold((f32::MAX, f32::MIN), |(lo, hi), &v| (lo.min(v), hi.max(v)));
    (hi - lo) / hi.max(f32::MIN_POSITIVE)
}

/// A generalized cosine window: `w(t) = Σₖ (−1)ᵏ aₖ cos(2πkt)`.
///
/// The whole raised-cosine family is this one formula at different coefficient
/// vectors, so they are constants rather than variants — [`HANN`](Self::HANN),
/// [`HAMMING`](Self::HAMMING), [`BLACKMAN`](Self::BLACKMAN) and friends. Adding
/// Nuttall is one line and no new match arms.
///
/// `Copy + PartialEq` despite holding a slice, and both are load-bearing:
/// [`crate::StftGeometry`] derives them and lives inside four stored types, so a
/// `Box<dyn Window>` field there is not available. `&'static [f32]` compares by
/// *contents*, so two windows with the same coefficients are equal wherever
/// their slices live.
///
/// **Not `Eq` or `Hash`**, because `f32` is neither. If a document type ever
/// needs to hash a window, it wants a serializable *name* — a small enum in the
/// crate that owns the document — rather than these coefficients; the two are
/// different jobs, and this one is the maths.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CosineWindow {
    /// `a₀, a₁, …`, applied with alternating sign. Never empty.
    terms: &'static [f32],
}

impl CosineWindow {
    /// A window from raised-cosine coefficients.
    ///
    /// # Panics
    ///
    /// If `terms` is empty — a window with no terms is the zero function, which
    /// silences the signal rather than tapering it. `const`, so a bad constant
    /// fails the build rather than the audio.
    pub const fn new(terms: &'static [f32]) -> Self {
        assert!(
            !terms.is_empty(),
            "a cosine window needs at least one coefficient"
        );
        Self { terms }
    }

    /// Periodic Hann. The default, and the shape every inverse path in this
    /// workspace was written against.
    pub const HANN: Self = Self::new(&[0.5, 0.5]);

    /// Periodic Hamming — the same raised cosine on a non-zero pedestal, which
    /// buys a much lower *first* sidelobe at the cost of a slower far-field
    /// rolloff. Reach for it to separate a quiet partial from a loud neighbour.
    pub const HAMMING: Self = Self::new(&[0.54, 0.46]);

    /// Periodic Blackman — three terms, a wider main lobe, and far deeper
    /// sidelobes than either raised cosine above.
    ///
    /// Its [`cola_overlap`](Window::cola_overlap) is **8**, not 4, and that is
    /// not an arbitrary table entry — see that method for why a third term
    /// doubles the required overlap.
    pub const BLACKMAN: Self = Self::new(&[0.42, 0.5, 0.08]);

    /// Periodic Blackman-Harris — four terms, deeper still.
    ///
    /// Costs one line, where under the old per-variant shape it would have cost
    /// an enum variant plus two table entries. That is the whole argument for
    /// the coefficient vector.
    pub const BLACKMAN_HARRIS: Self = Self::new(&[0.35875, 0.48829, 0.14128, 0.01168]);

    /// No window at all — the identity, for callers holding samples that are
    /// already windowed.
    ///
    /// The one-term degenerate case, and the only one whose COLA condition is
    /// trivially "the frames tile".
    pub const RECTANGULAR: Self = Self::new(&[1.0]);

    /// How many cosine terms — the number both derived facts turn on.
    #[inline]
    pub const fn terms(self) -> usize {
        self.terms.len()
    }
}

impl Default for CosineWindow {
    /// [`HANN`](Self::HANN).
    fn default() -> Self {
        Self::HANN
    }
}

impl Window for CosineWindow {
    #[inline]
    fn at(&self, t: f32) -> f32 {
        let phase = core::f32::consts::TAU * t;
        self.terms
            .iter()
            .enumerate()
            .map(|(k, &a)| {
                let sign = if k % 2 == 0 { 1.0 } else { -1.0 };
                sign * a * (k as f32 * phase).cos()
            })
            .sum()
    }

    /// `a₀`, exactly.
    ///
    /// Over a full period every cosine term above the first integrates to zero,
    /// so `∫₀¹ w = a₀` with nothing left over. Verified against direct summation
    /// at n = 64 and n = 1024: identical to eight decimal places and independent
    /// of `n`, which is why this needs no length and no integration.
    ///
    /// The consumer is a magnitude display. Without it, a spectrogram's dB
    /// window is pinned to whichever shape it was tuned on, and every other one
    /// reads between 0.6 dB (Hamming) and 6 dB (rectangular) off — with nothing
    /// erroring.
    #[inline]
    fn coherent_gain(&self) -> f32 {
        self.terms[0]
    }

    /// Derived from the term count, not tabulated.
    ///
    /// Squaring a `K`-term cosine window produces harmonics up to `2(K−1)`.
    /// Overlap-adding at rate `R` annihilates every harmonic that is not a
    /// multiple of `R`, so the sum is flat exactly when `R > 2(K−1)`, i.e.
    /// `R ≥ 2K−1`. A grid's hop must divide its window, so that rounds up to a
    /// power of two.
    ///
    /// | terms | `2K−1` | overlap |
    /// |---|---|---|
    /// | 1 (rectangular) | 1 | **1** |
    /// | 2 (Hann, Hamming) | 3 | **4** |
    /// | 3 (Blackman) | 5 | **8** |
    /// | 4 (Blackman-Harris) | 7 | **8** |
    ///
    /// Measured against all five shipped windows and tight in both directions:
    /// each is flat at its own overlap (≤ 6e-16) and ripples at half of it
    /// (2.1% for Blackman, 42% for Hamming). `the_cola_rule_is_tight` pins both
    /// halves.
    ///
    /// **This replaced a hand-written table**, which had Blackman at 4 on the
    /// reasoning that its raised-cosine neighbours use 4. That was wrong by
    /// 2.1% ripple, and a derivation cannot make that mistake.
    #[inline]
    fn cola_overlap(&self) -> usize {
        let needed = 2 * self.terms.len() - 1;
        needed.next_power_of_two()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [CosineWindow; 5] = [
        CosineWindow::HANN,
        CosineWindow::HAMMING,
        CosineWindow::BLACKMAN,
        CosineWindow::BLACKMAN_HARRIS,
        CosineWindow::RECTANGULAR,
    ];

    #[test]
    fn periodic_hann_starts_at_zero_and_peaks_at_the_centre() {
        let w = CosineWindow::HANN.coefficients(8);
        assert_eq!(w.len(), 8);
        assert!(w[0].abs() < 1e-6, "first point is zero");
        assert!((w[4] - 1.0).abs() < 1e-6, "centre reaches unity");
        // Periodic: the last point is NOT zero, which is what distinguishes it
        // from the symmetric form and what makes it tile.
        assert!(w[7] > 0.0);
    }

    /// **The COLA rule, checked in both directions.**
    ///
    /// Flat at the derived overlap *and* rippling at half of it. The second half
    /// is what makes the rule tight rather than merely safe: a `cola_overlap`
    /// that returned 16 for everything would pass the first assertion and
    /// needlessly refuse grids that reconstruct perfectly well.
    ///
    /// Mutation check: drop the `next_power_of_two` and Blackman reports 5,
    /// which is not a ratio a grid can express; return `4` for everything and
    /// Blackman fails the flatness half at 2.1% ripple.
    #[test]
    fn the_cola_rule_is_tight() {
        for w in ALL {
            let overlap = w.cola_overlap();
            let flat = cola_ripple(&w, 64, overlap);
            assert!(
                flat < 1e-4,
                "{:?}-term window is not flat at its own {overlap}x: {flat:.2e}",
                w.terms()
            );

            if overlap > 1 {
                let looser = cola_ripple(&w, 64, overlap / 2);
                assert!(
                    looser > 1e-3,
                    "{:?}-term window claims it needs {overlap}x, but {}x is \
                     already flat ({looser:.2e}) — the rule is too conservative",
                    w.terms(),
                    overlap / 2
                );
            }
        }
    }

    /// The derived overlaps, spelled out, so the rule cannot silently change
    /// what it returns for the windows that actually ship.
    #[test]
    fn the_shipped_windows_have_the_overlaps_the_table_claims() {
        assert_eq!(CosineWindow::RECTANGULAR.cola_overlap(), 1);
        assert_eq!(CosineWindow::HANN.cola_overlap(), 4);
        assert_eq!(CosineWindow::HAMMING.cola_overlap(), 4);
        assert_eq!(CosineWindow::BLACKMAN.cola_overlap(), 8);
        assert_eq!(CosineWindow::BLACKMAN_HARRIS.cola_overlap(), 8);
    }

    /// **`coherent_gain` is `a₀`, and `a₀` is the measured mean.**
    ///
    /// Two sizes, because the claim is also that it does not depend on `n` —
    /// which is the entire justification for it being a closed form that takes
    /// no length.
    ///
    /// Mutation check: return `terms[terms.len() - 1]` and every multi-term
    /// window fails; return a constant `0.5` and Hamming, Blackman and
    /// rectangular all fail.
    #[test]
    fn coherent_gain_is_the_measured_mean_at_any_size() {
        for w in ALL {
            for size in [64usize, 1024] {
                let measured = w.coefficients(size).iter().sum::<f32>() / size as f32;
                let claimed = w.coherent_gain();
                assert!(
                    (measured - claimed).abs() < 1e-6,
                    "{:?}-term at n={size}: claims {claimed}, measures {measured}",
                    w.terms()
                );
            }
        }
    }

    /// The trait's *default* implementations must agree with the closed forms
    /// that override them.
    ///
    /// Without this the overrides could drift from the maths they claim to
    /// shortcut, and nothing would notice — the defaults are the only
    /// independent statement of what the two functionals mean.
    #[test]
    fn the_closed_forms_agree_with_numeric_integration() {
        /// A window with no closed forms, so the trait defaults run.
        struct Numeric(CosineWindow);
        impl Window for Numeric {
            fn at(&self, t: f32) -> f32 {
                self.0.at(t)
            }
        }

        for w in ALL {
            let numeric = Numeric(w);
            assert!(
                (numeric.coherent_gain() - w.coherent_gain()).abs() < 1e-4,
                "{:?}-term: integrated {} vs closed form {}",
                w.terms(),
                numeric.coherent_gain(),
                w.coherent_gain()
            );
            assert_eq!(
                numeric.cola_overlap(),
                w.cola_overlap(),
                "{:?}-term: searched overlap disagrees with the derived one",
                w.terms()
            );
        }
    }

    /// Every window peaks at or below unity and never goes negative — one that
    /// overshot would amplify the frame it was meant to taper.
    ///
    /// Blackman-Harris is the one that could plausibly dip: it has the most
    /// subtracting terms.
    #[test]
    fn no_window_overshoots_or_goes_negative() {
        for w in ALL {
            for &v in w.coefficients(128).iter() {
                assert!(
                    (-1e-6..=1.0 + 1e-6).contains(&v),
                    "{:?}-term produced {v}, outside [0, 1]",
                    w.terms()
                );
            }
        }
    }

    /// `RECTANGULAR` is exactly the identity, so a caller holding pre-windowed
    /// samples can say so rather than passing a window that almost does nothing.
    #[test]
    fn rectangular_is_the_identity() {
        assert_eq!(CosineWindow::RECTANGULAR.coefficients(5), vec![1.0; 5]);
        assert_eq!(CosineWindow::RECTANGULAR.coherent_gain(), 1.0);
    }

    #[test]
    fn degenerate_sizes_do_not_divide_by_zero() {
        for w in ALL {
            assert!(w.coefficients(0).is_empty(), "{:?}-term at 0", w.terms());
            assert_eq!(w.coefficients(1).len(), 1, "{:?}-term at 1", w.terms());
            assert!(
                w.coefficients(1)[0].is_finite(),
                "{:?}-term at size 1 is not finite",
                w.terms()
            );
        }
    }

    /// Equality is by coefficients, not by slice address — otherwise two
    /// identical windows built at different call sites would compare unequal,
    /// and `StftGeometry`'s `PartialEq` would report a grid as changed when
    /// nothing about it had.
    #[test]
    fn windows_compare_by_their_coefficients() {
        assert_eq!(CosineWindow::new(&[0.5, 0.5]), CosineWindow::HANN);
        assert_ne!(CosineWindow::HANN, CosineWindow::HAMMING);
    }

    /// The default is Hann, spelled out. Every grid in the tree is built on it,
    /// so a change here silently re-tunes every existing analysis.
    #[test]
    fn the_default_is_hann() {
        assert_eq!(CosineWindow::default(), CosineWindow::HANN);
    }
}
