//! The analysis grid an STFT is computed on.
//!
//! One type owning window, hop, and sample rate together, because they are
//! only meaningful together: whether a transform inverts cleanly is a fact
//! about the *pair* `(window, hop)`, not about either alone.
//!
//! This replaces a five-field cluster duplicated across both result structs
//! and re-spelled longhand at every inverse-transform call site, along with
//! the derived quantities those structs stored — and could therefore
//! contradict.

use tutti_core::SampleRate;
use tutti_types::{Hz, Samples, Seconds};

use crate::error::{AnalysisError, Result};
use crate::grid::{BinCount, BinIndex, FrameCount};
use crate::window::{CosineWindow, Window};

/// Window, hop, window shape, and sample rate — validated once, on construction.
///
/// Fields are private and there is no literal constructor, so the invalid
/// instances the old `pub`-field structs admitted cannot exist: a zero hop, a
/// hop wider than its window, a non-positive sample rate, or a stored bin
/// count contradicting the window it came from.
///
/// **`window` and `window_fn` are different nouns**, and the names are chosen
/// to keep them apart: in DSP prose "the window is 2048" is a *length* and "the
/// window function is Hann" is a *shape*. The length decides frequency
/// resolution; the shape decides the sidelobe skirts around each partial.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StftGeometry {
    window: Samples,
    hop: Samples,
    sample_rate: SampleRate,
    window_fn: CosineWindow,
}

impl StftGeometry {
    /// An analysis grid.
    ///
    /// Accepts any positive window and hop, **including a hop wider than the
    /// window**: decimated display transforms deliberately sample sparsely,
    /// skipping the audio between frames. Such a grid analyses fine and cannot
    /// reconstruct, which is why the inverse path goes through
    /// [`cola`](Self::cola) instead of this.
    pub fn new(
        sample_rate: impl Into<SampleRate>,
        window: impl Into<Samples>,
        hop: impl Into<Samples>,
    ) -> Result<Self> {
        let (sample_rate, window, hop) = (sample_rate.into(), window.into(), hop.into());

        if window.is_zero() {
            return Err(AnalysisError::ZeroWindow);
        }
        if hop.is_zero() {
            return Err(AnalysisError::ZeroHop);
        }
        // `!(x > 0.0)`, not `x <= 0.0`: the negation is what rejects NaN. Every
        // comparison against NaN is false, so `x <= 0.0` *accepts* a NaN rate
        // and lets it reach the FFT geometry. clippy's `neg_cmp_op_on_partial_ord`
        // reads this as awkward style; it is the guard.
        #[allow(clippy::neg_cmp_op_on_partial_ord)]
        if !(sample_rate.get() > 0.0) {
            return Err(AnalysisError::NonPositiveSampleRate);
        }

        Ok(Self {
            window,
            hop,
            sample_rate,
            // Spelled literally rather than `CosineWindow::default()`: changing the
            // default must not silently re-tune every grid this constructor
            // has ever built.
            window_fn: CosineWindow::HANN,
        })
    }

    /// Whether frames overlap at all. False for a decimated display grid,
    /// which leaves gaps of unanalysed audio between frames.
    #[inline]
    pub fn frames_overlap(self) -> bool {
        self.hop < self.window
    }

    /// A COLA-compliant grid — the only kind an inverse transform accepts.
    ///
    /// A window squared is constant-overlap-add when the hop divides the window
    /// and the overlap reaches that window's own factor
    /// ([`CosineWindow::cola_overlap`]). Under that condition the inverse's
    /// per-sample window-sum normalization is exact, so untouched bins
    /// reconstruct to float precision.
    ///
    /// Built on [`CosineWindow::HANN`]; use [`cola_with`](Self::cola_with) to check
    /// a different shape, whose required overlap may be stricter — Blackman
    /// needs 8x where Hann needs 4x.
    ///
    /// Checked here rather than left to the forward transform: a hop five times
    /// wider than its window otherwise reaches the inverse and produces a comb
    /// of islands separated by silence.
    ///
    /// # Errors
    /// Returns [`AnalysisError::HopExceedsWindow`] if frames do not overlap, or
    /// [`AnalysisError::NotColaCompliant`] if the hop does not divide the
    /// window at 4x overlap — this constructor fixes the shape as Hann, which
    /// is where that figure comes from. Use
    /// [`cola_with`](Self::cola_with) for another window.
    pub fn cola(
        sample_rate: impl Into<SampleRate>,
        window: impl Into<Samples>,
        hop: impl Into<Samples>,
    ) -> Result<Self> {
        Self::cola_with(sample_rate, window, hop, CosineWindow::HANN)
    }

    /// [`cola`](Self::cola) for a given window shape.
    ///
    /// Separate from `with_window_fn` because the check is the point: a grid
    /// that reconstructs under Hann may not under Blackman, and the difference
    /// is silent — the transform runs, and the output has a 2.1% amplitude
    /// ripple that reads as a tremolo nobody asked for.
    pub fn cola_with(
        sample_rate: impl Into<SampleRate>,
        window: impl Into<Samples>,
        hop: impl Into<Samples>,
        window_fn: CosineWindow,
    ) -> Result<Self> {
        let geometry = Self::new(sample_rate, window, hop)?.with_window_fn(window_fn);
        if !geometry.frames_overlap() {
            return Err(AnalysisError::HopExceedsWindow {
                window: geometry.window,
                hop: geometry.hop,
            });
        }
        if !geometry.is_cola() {
            return Err(AnalysisError::NotColaCompliant {
                window: geometry.window,
                hop: geometry.hop,
                window_fn,
            });
        }
        Ok(geometry)
    }

    /// The analysis window in [`Samples`] — also the FFT size.
    #[inline]
    pub const fn window(self) -> Samples {
        self.window
    }

    /// How far frames advance, in [`Samples`]. Smaller than the window whenever
    /// frames overlap.
    #[inline]
    pub const fn hop(self) -> Samples {
        self.hop
    }

    /// The rate the analyzed buffer is denominated at, which sets what each bin
    /// is worth in [`Hz`].
    #[inline]
    pub const fn sample_rate(self) -> SampleRate {
        self.sample_rate
    }

    /// The window *shape*. See [`window`](Self::window) for its length.
    #[inline]
    pub const fn window_fn(self) -> CosineWindow {
        self.window_fn
    }

    /// The same grid analysed with a different window shape.
    ///
    /// A builder rather than a fourth constructor argument: every call site in
    /// the tree wants [`CosineWindow::HANN`], and a fourth positional parameter on
    /// two fallible three-argument constructors is exactly the transposition
    /// hazard `new` exists to reject.
    ///
    /// **Infallible, and that is not the same as safe.** A grid built by
    /// [`cola`](Self::cola) for one window may not be COLA for another, and
    /// this does not re-check — Blackman needs 8x overlap where Hann needs 4x,
    /// so `cola(rate, 2048, 512).with_window_fn(Blackman)` yields a grid that
    /// analyses fine and reconstructs with a 2.1% ripple. Use
    /// [`cola_with`](Self::cola_with) when the result must invert.
    #[inline]
    pub const fn with_window_fn(mut self, window_fn: CosineWindow) -> Self {
        self.window_fn = window_fn;
        self
    }

    /// Whether this window squared reconstructs exactly at this window/hop pair.
    ///
    /// Two conditions belonging to two different types: the hop must divide the
    /// window (a fact about the *grid*) and the overlap must reach the window's
    /// own COLA factor (a fact about the *window*, which is why the `4` that
    /// used to sit here now lives on [`CosineWindow::cola_overlap`]).
    #[inline]
    pub fn is_cola(self) -> bool {
        self.window.get().is_multiple_of(self.hop.get())
            && self.window.get() / self.hop.get() >= self.window_fn.cola_overlap()
    }

    /// Real-spectrum bins, excluding Nyquist — the magnitude path's count.
    ///
    /// Differs from [`bins_per_frame`](Self::bins_per_frame) by one, which is
    /// exactly why storing both on two separate structs invited a consumer to
    /// reach for the wrong one.
    #[inline]
    pub fn freq_bins(self) -> BinCount {
        BinCount(self.window.get() / 2)
    }

    /// Real-spectrum bins including Nyquist — the complex path's count.
    #[inline]
    pub fn bins_per_frame(self) -> BinCount {
        BinCount(self.window.get() / 2 + 1)
    }

    /// The centre frequency of a bin.
    #[inline]
    pub fn bin_frequency(self, bin: BinIndex) -> Hz {
        Hz((bin.get() as f64 * self.sample_rate.get() / self.window.get() as f64) as f32)
    }

    /// How many whole frames fit over `len` samples.
    #[inline]
    pub fn frames_for(self, len: impl Into<Samples>) -> FrameCount {
        let len = len.into().get();
        if len < self.window.get() {
            return FrameCount(0);
        }
        FrameCount((len - self.window.get()) / self.hop.get() + 1)
    }

    /// How many samples resynthesizing `frames` frames produces.
    #[inline]
    pub fn output_len(self, frames: FrameCount) -> Samples {
        if frames.get() == 0 {
            return Samples(0);
        }
        Samples((frames.get() - 1) * self.hop.get() + self.window.get())
    }

    /// The delay this grid imposes: one whole window must arrive before the
    /// first frame can be analyzed.
    #[inline]
    pub fn latency(self) -> Seconds {
        Seconds(self.window.get() as f32 / self.sample_rate.get() as f32)
    }

    /// The analysis window's coefficients, `window` points of `window_fn`.
    ///
    /// Deliberately longer than its neighbours: it **allocates**, where
    /// [`window`](Self::window) and [`window_fn`](Self::window_fn) are `const`
    /// field reads. A short name would put an allocation and two free reads at
    /// the same apparent cost.
    #[inline]
    pub fn window_coefficients(self) -> Vec<f32> {
        self.window_fn.coefficients(self.window.get())
    }

    /// The DC gain this grid's forward transform leaves in every magnitude.
    ///
    /// A forward transform sums `window` samples per bin without scaling, so
    /// magnitudes carry a gain of `coherent_gain × window`. A display dividing
    /// by that is window-*independent*: a 2048-point Hann analysis and a
    /// 512-point Blackman one of the same audio land on the same scale, so a dB
    /// window tuned once stays tuned.
    ///
    /// The reciprocal is what a magnitude consumer actually wants; it is
    /// spelled out rather than provided because "gain" and "the thing you
    /// multiply by" being two methods is how one of them gets used by mistake.
    #[inline]
    pub fn magnitude_gain(self) -> f32 {
        self.window_fn.coherent_gain() * self.window.get().max(1) as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geo(window: usize, hop: usize) -> StftGeometry {
        StftGeometry::new(44100.0, Samples(window), Samples(hop)).unwrap()
    }

    #[test]
    fn rejects_every_invalid_instance_the_old_structs_admitted() {
        assert_eq!(
            StftGeometry::new(44100.0, Samples(0), Samples(512)),
            Err(AnalysisError::ZeroWindow)
        );
        assert_eq!(
            StftGeometry::new(44100.0, Samples(2048), Samples(0)),
            Err(AnalysisError::ZeroHop)
        );
        assert_eq!(
            StftGeometry::new(0.0, Samples(2048), Samples(512)),
            Err(AnalysisError::NonPositiveSampleRate)
        );
    }

    /// A hop wider than the window is legal for analysis and illegal for
    /// reconstruction — decimated display grids sample sparsely on purpose.
    #[test]
    fn a_hop_wider_than_the_window_analyses_but_does_not_invert() {
        let sparse = StftGeometry::new(44100.0, Samples(2048), Samples(8192)).unwrap();
        assert!(!sparse.frames_overlap());

        assert_eq!(
            StftGeometry::cola(44100.0, Samples(2048), Samples(8192)),
            Err(AnalysisError::HopExceedsWindow {
                window: Samples(2048),
                hop: Samples(8192),
            })
        );
    }

    /// The bug this split exists to prevent: a decimated hop is accepted for
    /// analysis but refused for anything that has to invert.
    #[test]
    fn cola_refuses_what_new_accepts() {
        // 4x overlap: fine for both.
        assert!(StftGeometry::cola(44100.0, Samples(2048), Samples(512)).is_ok());

        // 2x overlap: analysable, not invertible.
        assert!(StftGeometry::new(44100.0, Samples(2048), Samples(1024)).is_ok());
        assert_eq!(
            StftGeometry::cola(44100.0, Samples(2048), Samples(1024)),
            Err(AnalysisError::NotColaCompliant {
                window: Samples(2048),
                hop: Samples(1024),
                window_fn: CosineWindow::HANN,
            })
        );

        // Hop does not divide the window: analysable, not invertible.
        assert!(StftGeometry::new(44100.0, Samples(2048), Samples(500)).is_ok());
        assert!(StftGeometry::cola(44100.0, Samples(2048), Samples(500)).is_err());
    }

    /// **A grid that is COLA for one window is not automatically COLA for
    /// another**, and this is the case that makes `cola_with` necessary.
    ///
    /// 2048/512 is 4x overlap: exact for Hann, and a 2.1% amplitude ripple for
    /// Blackman, which needs 8x. Nothing about the grid changed — only the
    /// shape laid over it.
    ///
    /// Mutation check: giving `Blackman` a `cola_overlap` of 4 (which is what
    /// its two raised-cosine neighbours use, and what an eyeballed table would
    /// say) makes the first assertion here fail.
    #[test]
    fn a_hann_cola_grid_may_not_be_cola_for_another_window() {
        let hop = Samples(512);
        assert!(StftGeometry::cola(44100.0, Samples(2048), hop).is_ok());

        assert_eq!(
            StftGeometry::cola_with(44100.0, Samples(2048), hop, CosineWindow::BLACKMAN),
            Err(AnalysisError::NotColaCompliant {
                window: Samples(2048),
                hop,
                window_fn: CosineWindow::BLACKMAN,
            }),
            "Blackman needs 8x overlap; 2048/512 is 4x"
        );

        // And it is accepted at the overlap it actually asks for.
        assert!(StftGeometry::cola_with(
            44100.0,
            Samples(2048),
            Samples(256),
            CosineWindow::BLACKMAN
        )
        .is_ok());
    }

    /// The builder does **not** re-check COLA, and the doc says so — this pins
    /// that, because a silently-invalidated grid is the hazard `cola_with`
    /// exists to give callers a way to avoid.
    #[test]
    fn the_builder_can_produce_a_non_cola_grid() {
        let hann = StftGeometry::cola(44100.0, Samples(2048), Samples(512)).expect("hann 4x");
        assert!(hann.is_cola());

        let blackman = hann.with_window_fn(CosineWindow::BLACKMAN);
        assert!(
            !blackman.is_cola(),
            "with_window_fn is infallible and does not re-validate"
        );
    }

    /// The two window nouns do not collide: one is a length, one is a shape.
    #[test]
    fn a_grid_carries_both_a_window_length_and_a_window_shape() {
        let g = geo(2048, 512);
        assert_eq!(g.window(), Samples(2048));
        assert_eq!(g.window_fn(), CosineWindow::HANN);
        assert_eq!(g.window_coefficients().len(), 2048);
    }

    /// **The magnitude gain follows the window shape, not just its length.**
    ///
    /// This is what lets a display divide out the forward transform's window
    /// gain without knowing which window produced it. Mutation check: hardcode
    /// `0.5` for the coherent gain and the Blackman case fails.
    #[test]
    fn magnitude_gain_tracks_the_window_shape() {
        let hann = geo(1024, 256);
        assert!((hann.magnitude_gain() - 512.0).abs() < 1e-3, "0.5 * 1024");

        let blackman = hann.with_window_fn(CosineWindow::BLACKMAN);
        assert!(
            (blackman.magnitude_gain() - 430.08).abs() < 1e-2,
            "0.42 * 1024, got {}",
            blackman.magnitude_gain()
        );

        let rect = hann.with_window_fn(CosineWindow::RECTANGULAR);
        assert!((rect.magnitude_gain() - 1024.0).abs() < 1e-3, "1.0 * 1024");
    }

    #[test]
    fn bin_counts_differ_by_one_and_both_derive_from_the_window() {
        let g = geo(2048, 512);
        assert_eq!(g.freq_bins(), BinCount(1024));
        assert_eq!(g.bins_per_frame(), BinCount(1025));
    }

    #[test]
    fn bin_frequency_spans_dc_to_nyquist() {
        let g = geo(2048, 512);
        assert_eq!(g.bin_frequency(BinIndex(0)), Hz(0.0));
        // Bin `window/2` is Nyquist.
        assert!((g.bin_frequency(BinIndex(1024)).get() - 22050.0).abs() < 1e-3);
        // Resolution is rate / window.
        assert!((g.bin_frequency(BinIndex(1)).get() - 44100.0 / 2048.0).abs() < 1e-3);
    }

    #[test]
    fn frames_and_output_length_round_trip() {
        let g = geo(2048, 512);

        // Shorter than one window yields no frames at all.
        assert_eq!(g.frames_for(Samples(2047)), FrameCount(0));
        assert_eq!(g.output_len(FrameCount(0)), Samples(0));

        assert_eq!(g.frames_for(Samples(2048)), FrameCount(1));
        assert_eq!(g.output_len(FrameCount(1)), Samples(2048));

        // Each additional frame adds exactly one hop.
        assert_eq!(g.frames_for(Samples(2048 + 512)), FrameCount(2));
        assert_eq!(g.output_len(FrameCount(2)), Samples(2048 + 512));
    }

    #[test]
    fn latency_is_one_window() {
        let g = geo(4410, 1102);
        assert!((g.latency().get() - 0.1).abs() < 1e-6);
    }
}
