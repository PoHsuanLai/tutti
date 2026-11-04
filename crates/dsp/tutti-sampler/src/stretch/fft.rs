//! The vocoder's real-input FFT pair, planned once per window length.
//!
//! A frame is real, so its spectrum is `size / 2 + 1` bins (`DC..=Nyquist`) and
//! both directions run on `realfft`'s real-to-complex plans: half the work of a
//! complex transform over the same signal.
//!
//! The plans and the scratch they need are built in [`Fft::new`], on the
//! control thread. The plans are immutable and shared between clones; the
//! scratch is per vocoder. A transform allocates nothing.

pub(crate) use realfft::num_complex::Complex32;
use realfft::{ComplexToReal, FftError, RealFftPlanner, RealToComplex};
use std::sync::Arc;

/// Forward and inverse plans for one window length, and the scratch both use.
pub(super) struct Fft {
    forward: Arc<dyn RealToComplex<f32>>,
    inverse: Arc<dyn ComplexToReal<f32>>,
    scratch: Vec<Complex32>,
}

impl Fft {
    /// Plans for a window of `size` samples (an even length, which every
    /// [`FftSize`](super::FftSize) is).
    pub(super) fn new(size: usize) -> Self {
        let mut planner = RealFftPlanner::<f32>::new();
        let forward = planner.plan_fft_forward(size);
        let inverse = planner.plan_fft_inverse(size);
        Self::from_plans(forward, inverse)
    }

    /// The same plans over fresh scratch.
    pub(super) fn clone_fresh(&self) -> Self {
        Self::from_plans(Arc::clone(&self.forward), Arc::clone(&self.inverse))
    }

    fn from_plans(
        forward: Arc<dyn RealToComplex<f32>>,
        inverse: Arc<dyn ComplexToReal<f32>>,
    ) -> Self {
        let len = forward.get_scratch_len().max(inverse.get_scratch_len());
        Self {
            forward,
            inverse,
            scratch: vec![Complex32::new(0.0, 0.0); len],
        }
    }

    /// `real` (`size` samples) to `bins` (`size / 2 + 1`), `DC..=Nyquist`. The
    /// plan uses `real` as scratch, so its contents are unspecified after.
    /// DC's and Nyquist's imaginary parts are zero.
    pub(super) fn forward(&mut self, real: &mut [f32], bins: &mut [Complex32]) {
        let needed = self.forward.get_scratch_len();
        self.forward
            .process_with_scratch(real, bins, &mut self.scratch[..needed])
            .expect("BUG: buffer lengths are sized from the window");
    }

    /// `bins` (`size / 2 + 1`) to `real` (`size` samples), **unnormalized**
    /// (the output is `size` times the signal; the caller scales). The plan
    /// uses `bins` as scratch, so its contents are unspecified after.
    ///
    /// The imaginary parts of DC and Nyquist do not belong to a real signal and
    /// are ignored. `realfft` writes the output and then reports them as
    /// [`FftError::InputValues`] when they are non-zero (a NaN included), which
    /// is tolerated here: it is the one variant that leaves the output written.
    pub(super) fn inverse(&mut self, bins: &mut [Complex32], real: &mut [f32]) {
        let needed = self.inverse.get_scratch_len();
        match self
            .inverse
            .process_with_scratch(bins, real, &mut self.scratch[..needed])
        {
            Ok(()) | Err(FftError::InputValues(..)) => {}
            Err(e) => unreachable!("BUG: buffer lengths are sized from the window: {e}"),
        }
    }
}
