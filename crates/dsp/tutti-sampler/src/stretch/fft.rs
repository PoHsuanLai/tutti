//! The vocoder's real-input FFT pair, planned once per window length.
//!
//! A thin owner of [`RealFft`] (the engine's CPU-independent real FFT, in
//! `tutti-types`) and the scratch one vocoder transforms with. The plans are
//! immutable and shared between clones; the scratch is per vocoder. A
//! transform allocates nothing.

pub(crate) use tutti_types::Complex32;
use tutti_types::RealFft;

/// Forward and inverse plans for one window length, and the scratch both use.
pub(super) struct Fft {
    plan: RealFft,
    scratch: Vec<Complex32>,
}

impl Fft {
    /// Plans for a window of `size` samples (an even length, which every
    /// [`FftSize`](super::FftSize) is).
    pub(super) fn new(size: usize) -> Self {
        assert!(
            size >= 2 && size.is_multiple_of(2),
            "BUG: window is even, got {size}"
        );
        Self::from_plan(RealFft::new(size))
    }

    /// The same plans over fresh scratch.
    pub(super) fn clone_fresh(&self) -> Self {
        Self::from_plan(self.plan.clone())
    }

    fn from_plan(plan: RealFft) -> Self {
        let scratch = plan.new_scratch();
        Self { plan, scratch }
    }

    /// `real` (`size` samples) to `bins` (`size / 2 + 1`), `DC..=Nyquist`.
    /// DC's and Nyquist's imaginary parts are zero.
    pub(super) fn forward(&mut self, real: &[f32], bins: &mut [Complex32]) {
        self.plan.forward(real, bins, &mut self.scratch);
    }

    /// `bins` (`size / 2 + 1`) to `real` (`size` samples), **unnormalized**
    /// (the output is `size` times the signal; the caller scales). The plan
    /// uses `bins` as scratch, so its contents are unspecified after.
    ///
    /// The imaginary parts of DC and Nyquist do not belong to a real signal and
    /// are ignored.
    pub(super) fn inverse(&mut self, bins: &mut [Complex32], real: &mut [f32]) {
        self.plan.inverse(bins, real, &mut self.scratch);
    }
}
