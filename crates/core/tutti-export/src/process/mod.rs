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
//! apply), and it is not a stage here: `tutti_analysis::loudness` measures —
//! streaming, while the render runs — and the caller applies the `Db` that
//! `Loudness::gain_to` returns. Keeping that out is precisely what lets every
//! export stream.

pub(crate) mod dither;
pub(crate) mod resample;

// Consumed only by `encode_to_file`'s per-format arms, which are codec-gated.
#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
pub(crate) use dither::DitherState;
pub use resample::ChunkSize;
// `resample_rendered` is ungated: `normalize::render_normalized_to_file` calls
// it before measuring, and that entry point is available with no codec on — it
// renders and normalizes, then fails at the encode. The streaming `Resampler`
// is only reached from the encode arms, so it keeps the gate.
pub(crate) use resample::resample_rendered;
#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
pub(crate) use resample::Resampler;
