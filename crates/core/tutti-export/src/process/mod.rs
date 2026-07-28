//! Signal processing between the render and the encoder.
//!
//! Two stages, and both stream:
//!
//! - **dither** ([`DitherState`]) — per block, carrying only its RNG.
//! - **resample** ([`resample::resample_planar`]) — rubato is a block
//!   resampler; it never needs the whole signal.
//!
//! There is deliberately no "mastering" type and no whole-signal pass.
//! Normalization is the one step that genuinely needs two passes (measure, then
//! apply), and it is not this crate's: `tutti_analysis::loudness` measures —
//! streaming, while the render runs — and the caller applies the `Db` that
//! `Loudness::gain_to` returns. Keeping that out here is precisely what lets
//! every export stream.

pub(crate) mod dither;
pub(crate) mod resample;

pub(crate) use dither::DitherState;
pub use resample::ResampleQuality;
#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
pub(crate) use resample::Resampler;
