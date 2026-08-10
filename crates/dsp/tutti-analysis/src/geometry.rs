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
use crate::window::hann;

/// Window, hop, and sample rate — validated once, on construction.
///
/// Fields are private and there is no literal constructor, so the invalid
/// instances the old `pub`-field structs admitted cannot exist: a zero hop, a
/// hop wider than its window, a non-positive sample rate, or a stored bin
/// count contradicting the window it came from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StftGeometry {
    window: Samples,
    hop: Samples,
    sample_rate: SampleRate,
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
        if !(sample_rate.get() > 0.0) {
            return Err(AnalysisError::NonPositiveSampleRate);
        }

        Ok(Self {
            window,
            hop,
            sample_rate,
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
    /// Hann² is constant-overlap-add when the hop divides the window and the
    /// overlap is at least 75%. Under that condition the inverse's per-sample
    /// window-sum normalization is exact, so untouched bins reconstruct to
    /// float precision.
    ///
    /// Checked here rather than left to the forward transform: a hop five times
    /// wider than its window otherwise reaches the inverse and produces a comb
    /// of islands separated by silence.
    ///
    /// # Errors
    /// Returns [`AnalysisError::HopExceedsWindow`] if frames do not overlap, or
    /// [`AnalysisError::NotColaCompliant`] if the hop does not divide the
    /// window at 4x overlap.
    pub fn cola(
        sample_rate: impl Into<SampleRate>,
        window: impl Into<Samples>,
        hop: impl Into<Samples>,
    ) -> Result<Self> {
        let geometry = Self::new(sample_rate, window, hop)?;
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

    /// Whether Hann² reconstructs exactly at this window/hop pair.
    #[inline]
    pub fn is_cola(self) -> bool {
        self.window.get().is_multiple_of(self.hop.get()) && self.window.get() / self.hop.get() >= 4
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

    /// The analysis window, periodic Hann.
    #[inline]
    pub fn hann(self) -> Vec<f32> {
        hann(self.window.get())
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
            })
        );

        // Hop does not divide the window: analysable, not invertible.
        assert!(StftGeometry::new(44100.0, Samples(2048), Samples(500)).is_ok());
        assert!(StftGeometry::cola(44100.0, Samples(2048), Samples(500)).is_err());
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
