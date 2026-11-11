//! Signal processing between the render and the encoder.
//!
//! Two stages, and both stream:
//!
//! - dither (`tutti_dsp::DitherSink`, over the encoder) — per block, carrying only its RNG.
//! - resample (`tutti_dsp::StreamingResampler`) — an FFT block resampler
//!   on the scalar `RealFft`; it never needs the whole signal.
//!
//! There is deliberately no "mastering" type and no whole-signal pass.
//! Normalization is the one step that genuinely needs two passes (measure, then
//! apply), and it is not a stage here: [`crate::normalize`] measures a spill of
//! the render and the write pass applies the `Db` it chooses. Keeping that out
//! is precisely what lets every other output stream.

pub(crate) mod dither;

pub(crate) use dither::dither_spec;
pub use tutti_dsp::ChunkSize;
