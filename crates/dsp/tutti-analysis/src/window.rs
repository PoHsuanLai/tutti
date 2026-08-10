//! Analysis windows.
//!
//! One Hann, where there were eight copies. Seven agreed; the eighth divided
//! by `size - 1` instead of `size` — the *symmetric* Hann rather than the
//! periodic one. Only the periodic form satisfies the COLA property the
//! inverse transform's normalization assumes, so the odd copy out was both
//! inconsistent with its siblings and wrong for overlap-add. It also divided
//! by zero at `size == 1`.
//!
//! # Why a window is a *type* and not a function
//!
//! A transform needs three facts about its window, and only one of them is the
//! coefficients. It also needs the overlap at which the window squared
//! constant-overlap-adds — the `4` that used to sit hardcoded in
//! [`StftGeometry::is_cola`](crate::StftGeometry::is_cola) — and the coherent
//! gain a forward pass leaves in every magnitude, which the spectrogram upload
//! used to spell as a bare `2.0 / window`.
//!
//! Those two constants are properties of the *window shape*, and while there
//! was only one shape they could live anywhere. Spread across three crates,
//! they are three chances to add a window and get quietly wrong numbers.

/// Which analysis window a grid is computed on.
///
/// A closed set rather than a trait: every variant differs only in its
/// coefficient formula, and the two derived facts a transform needs are
/// per-variant constants. A `match` makes the compiler enumerate them when a
/// variant is added, which a trait's default methods would silently paper over.
/// (The [`VoiceSource`] shape, for the `ClipReader` reason — a trait whose
/// methods mean different things per implementor hides divergence rather than
/// removing it.)
///
/// Fieldless on purpose, and `#[non_exhaustive]` to keep it that way visibly.
/// The derives are load-bearing beyond convenience: `Eq` and `Hash` hold only
/// because no variant carries a float, and `dawai-spectral-edit`'s serializable
/// `AnalysisGrid` mirror derives `Eq` in a *document* type. A parameterized
/// variant — `Gaussian(f32)` is the tempting one — would take `Eq` away from a
/// serialized shape two crates from here.
///
/// [`VoiceSource`]: https://docs.rs/tutti-sampler
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum WindowFn {
    /// Periodic Hann. The default, and the shape every inverse path in this
    /// workspace was written against.
    #[default]
    Hann,
    /// Periodic Hamming — the same raised cosine on a non-zero pedestal, which
    /// buys a much lower *first* sidelobe at the cost of a slower far-field
    /// rolloff. Reach for it to separate a quiet partial from a loud neighbour.
    Hamming,
    /// Periodic Blackman — three terms, a wider main lobe, and far deeper
    /// sidelobes than either raised cosine above.
    ///
    /// Note its [`cola_overlap`](Self::cola_overlap) is **8**, not 4: measured,
    /// Blackman² at 75% overlap ripples by 2.1%, so a 4x grid does not
    /// reconstruct. This is the variant that makes `cola_overlap` a real
    /// accessor rather than a constant wearing a method.
    Blackman,
    /// No window at all.
    ///
    /// Not a display choice — the identity, for callers holding samples that
    /// are already windowed. It is also the only variant whose COLA condition
    /// is trivially "the frames tile", i.e. any integer overlap.
    Rectangular,
}

impl WindowFn {
    /// The window's coefficients, `size` points, periodic.
    ///
    /// Periodic (`i / size`), not symmetric (`i / (size - 1)`): the periodic
    /// form is what constant-overlap-add requires, because it tiles without the
    /// duplicated endpoint that breaks the window sum.
    ///
    /// `size == 0` yields an empty window; `size == 1` does not divide by zero.
    pub fn coefficients(self, size: usize) -> Vec<f32> {
        // Raised-cosine coefficients, alternating sign per term. Hann and
        // Hamming are two-term, Blackman three; writing them as one series
        // rather than three loops keeps the periodic `i / size` denominator in
        // exactly one place, which is the thing the eight-copies story was
        // about.
        let terms: &[f32] = match self {
            Self::Hann => &[0.5, 0.5],
            Self::Hamming => &[0.54, 0.46],
            Self::Blackman => &[0.42, 0.5, 0.08],
            Self::Rectangular => return vec![1.0; size],
        };

        (0..size)
            .map(|i| {
                let phase = core::f32::consts::TAU * i as f32 / size as f32;
                terms
                    .iter()
                    .enumerate()
                    .map(|(k, &a)| {
                        let sign = if k % 2 == 0 { 1.0 } else { -1.0 };
                        sign * a * (k as f32 * phase).cos()
                    })
                    .sum()
            })
            .collect()
    }

    /// The minimum overlap factor `window / hop` at which this window squared
    /// constant-overlap-adds.
    ///
    /// This is the `4` that was hardcoded in `StftGeometry::is_cola`. It is a
    /// property of the window shape, not of the grid, which is why it moved
    /// here — and the values are **measured**, not assumed:
    ///
    /// | | 2x | 4x | 8x |
    /// |---|---|---|---|
    /// | `Hann` | ripples | **flat** (6e-16) | flat |
    /// | `Hamming` | ripples | **flat** (4e-16) | flat |
    /// | `Blackman` | ripples | ripples (2.1%) | **flat** (4e-16) |
    ///
    /// Blackman at 4x is the case worth naming: it *looks* like it should work
    /// — it is a raised cosine like its neighbours — and it does not.
    /// `every_window_colas_at_its_own_overlap` is what pins each of these.
    pub const fn cola_overlap(self) -> usize {
        match self {
            Self::Hann | Self::Hamming => 4,
            Self::Blackman => 8,
            // Rect² is exactly flat as soon as the frames tile, which they do
            // at any integer overlap including 1.
            Self::Rectangular => 1,
        }
    }

    /// Coherent gain: `Σw / n`, the DC gain a forward transform leaves in every
    /// magnitude because it does not normalize by the window sum.
    ///
    /// A closed form rather than a runtime sum, and exactly so: for a periodic
    /// raised cosine every term above the first sums to zero over a full
    /// period, leaving `Σw = a₀ · n`. Verified against a direct summation at
    /// n = 64 and n = 1024 — identical to eight decimal places, and independent
    /// of `n`, which is why this takes no length.
    ///
    /// The consumer is a magnitude display: without this, a spectrogram's dB
    /// window is pinned to whichever window shape it was tuned on, and every
    /// other one reads between 0.6 dB (Hamming) and 6 dB (Rectangular) off with
    /// nothing erroring.
    pub const fn coherent_gain(self) -> f32 {
        match self {
            Self::Hann => 0.5,
            Self::Hamming => 0.54,
            Self::Blackman => 0.42,
            Self::Rectangular => 1.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [WindowFn; 4] = [
        WindowFn::Hann,
        WindowFn::Hamming,
        WindowFn::Blackman,
        WindowFn::Rectangular,
    ];

    #[test]
    fn periodic_hann_starts_at_zero_and_peaks_at_the_centre() {
        let w = WindowFn::Hann.coefficients(8);
        assert_eq!(w.len(), 8);
        assert!(w[0].abs() < 1e-6, "first point is zero");
        assert!((w[4] - 1.0).abs() < 1e-6, "centre reaches unity");
        // Periodic: the last point is NOT zero, which is what distinguishes it
        // from the symmetric form and what makes it tile.
        assert!(w[7] > 0.0);
    }

    /// Sum `w²` at `overlap` frames per window and report the ripple across the
    /// steady-state interior, away from ramp-in and ramp-out.
    fn cola_ripple(window: WindowFn, size: usize, overlap: usize) -> f32 {
        let hop = size / overlap;
        let w = window.coefficients(size);
        let frames = 4 * overlap;
        let mut acc = vec![0.0f32; frames * hop + size];
        for f in 0..frames {
            for (i, &v) in w.iter().enumerate() {
                acc[f * hop + i] += v * v;
            }
        }

        let interior = &acc[size..frames * hop];
        let (lo, hi) = interior
            .iter()
            .fold((f32::MAX, f32::MIN), |(lo, hi), &v| (lo.min(v), hi.max(v)));
        (hi - lo) / hi.max(f32::MIN_POSITIVE)
    }

    /// **The claim [`WindowFn::cola_overlap`] makes, checked per variant.**
    ///
    /// The distinguishing property: overlapping window² frames at the stated
    /// overlap sum to a constant. The symmetric Hann variant does not, which is
    /// why the odd copy out was a real defect and not a rounding preference.
    ///
    /// Mutation check: setting `Blackman`'s overlap back to 4 (which is what an
    /// eyeballed table would say, since its neighbours are 4) fails here at
    /// 2.1% ripple. That is the assertion that makes this accessor a checked
    /// claim rather than four numbers someone typed.
    #[test]
    fn every_window_colas_at_its_own_overlap() {
        for window in ALL {
            let overlap = window.cola_overlap();
            let ripple = cola_ripple(window, 64, overlap);
            assert!(
                ripple < 1e-4,
                "{window:?} squared does not sum flat at its own {overlap}x \
                 overlap: {ripple:.4} ripple"
            );
        }
    }

    /// The overlap each variant reports is the *minimum* — one step looser must
    /// actually fail, or the number is merely conservative rather than correct.
    ///
    /// Without this, every variant could claim 8x and pass the test above while
    /// refusing grids that reconstruct perfectly well.
    #[test]
    fn one_step_below_the_stated_overlap_ripples() {
        for window in ALL {
            let overlap = window.cola_overlap();
            if overlap < 2 {
                // Rectangular is already at the floor: there is no looser grid
                // than "the frames tile", so there is nothing to refute.
                continue;
            }
            let ripple = cola_ripple(window, 64, overlap / 2);
            assert!(
                ripple > 1e-3,
                "{window:?} claims it needs {overlap}x, but {}x is already flat \
                 ({ripple:.2e}) — the stated overlap is too conservative",
                overlap / 2
            );
        }
    }

    /// **`coherent_gain` is the mean of the coefficients**, and the closed form
    /// must match a direct sum — that is the entire justification for it being
    /// a `const fn` that takes no length.
    ///
    /// Two sizes, because the claim is also that it does not depend on `n`.
    #[test]
    fn coherent_gain_is_the_measured_mean_at_any_size() {
        for window in ALL {
            for size in [64usize, 1024] {
                let w = window.coefficients(size);
                let measured = w.iter().sum::<f32>() / size as f32;
                let claimed = window.coherent_gain();
                assert!(
                    (measured - claimed).abs() < 1e-6,
                    "{window:?} at n={size}: claims {claimed}, measures {measured}"
                );
            }
        }
    }

    /// Every window peaks at or below unity and never goes negative — a window
    /// that overshot would amplify the frame it was meant to taper.
    ///
    /// Blackman is the one that could plausibly dip: its third term subtracts.
    #[test]
    fn no_window_overshoots_or_goes_negative() {
        for window in ALL {
            for &v in window.coefficients(128).iter() {
                assert!(
                    (-1e-6..=1.0 + 1e-6).contains(&v),
                    "{window:?} produced {v}, outside [0, 1]"
                );
            }
        }
    }

    /// `Rectangular` is exactly the identity, so a caller holding pre-windowed
    /// samples can say so rather than passing a window that almost does nothing.
    #[test]
    fn rectangular_is_the_identity() {
        assert_eq!(WindowFn::Rectangular.coefficients(5), vec![1.0; 5]);
        assert_eq!(WindowFn::Rectangular.coherent_gain(), 1.0);
    }

    #[test]
    fn degenerate_sizes_do_not_divide_by_zero() {
        for window in ALL {
            assert!(window.coefficients(0).is_empty(), "{window:?} at size 0");
            assert_eq!(window.coefficients(1).len(), 1, "{window:?} at size 1");
            assert!(
                window.coefficients(1)[0].is_finite(),
                "{window:?} at size 1 is not finite"
            );
        }
    }

    /// The default is Hann, spelled out. Every grid in the tree is built on it,
    /// so a change here silently re-tunes every existing analysis.
    #[test]
    fn the_default_is_hann() {
        assert_eq!(WindowFn::default(), WindowFn::Hann);
    }
}
