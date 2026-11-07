//! Signal processing between the render and the encoder.
//!
//! Two stages, and both stream:
//!
//! - dither ([`DitherState`]) — per block, carrying only its RNG.
//! - resample ([`Resampler`]) — an FFT block resampler
//!   on the scalar `RealFft`; it never needs the whole signal.
//!
//! There is deliberately no "mastering" type and no whole-signal pass.
//! Normalization is the one step that genuinely needs two passes (measure, then
//! apply), and it is not a stage here: `tutti_dsp::measure_loudness` measures —
//! streaming, while the render runs — and the caller applies the `Db` that
//! `Loudness::gain_to` returns. Keeping that out is precisely what lets every
//! export stream.

pub(crate) mod dither;
pub(crate) mod resample;

pub(crate) use dither::DitherState;
pub use resample::ChunkSize;
pub(crate) use resample::{Resampler, resample_rendered};
