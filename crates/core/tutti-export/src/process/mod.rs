//! Mastering pipeline for rendered audio.
//!
//! Whole-signal processing (resample → normalize → dither → mono) via
//! [`Chain`] + [`process`], or per-chunk processing for streaming exports via
//! [`StreamProcessor`]. Leaves are feature-gated to match the encoder
//! features that consume them.

#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
pub(crate) mod chain;
#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
pub(crate) mod dither;
#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
pub(crate) mod loudness;
#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
pub(crate) mod mono;
pub(crate) mod resample;
#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
pub(crate) mod stream;

#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
pub(crate) use chain::Chain;
#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
pub(crate) use dither::{apply_dither, DitherState};
#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
pub(crate) use loudness::{analyze_loudness, normalize_loudness, normalize_peak};
#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
pub(crate) use mono::stereo_to_mono;
#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
pub(crate) use resample::resample_stereo;
pub use resample::ResampleQuality;
#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
pub(crate) use stream::{Chunk, StreamConfig, StreamProcessor};

#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
use crate::options::{BitDepth, ChannelMode, Dither, Normalize};
#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
use crate::Result;

/// Inputs for one whole-signal pass through the mastering chain.
#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
pub(crate) struct ProcessRequest<'a> {
    pub left: &'a [f32],
    pub right: &'a [f32],
    pub source_sample_rate: u32,
    /// Output rate. Equal to `source_sample_rate` for a no-op.
    pub target_sample_rate: u32,
    pub normalize: Normalize,
    pub dither: Dither,
    pub bit_depth: BitDepth,
    pub channels: ChannelMode,
    pub resample_quality: ResampleQuality,
}

/// Output of [`process`]. Shape is determined by [`ProcessRequest::channels`].
#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
pub(crate) enum ProcessedAudio {
    Stereo { left: Vec<f32>, right: Vec<f32> },
    Mono(Vec<f32>),
}

/// Run the mastering chain: resample → normalize → dither → finalize (mono
/// downmix if requested).
///
/// The body is pure composition of [`Chain`] methods.
#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
pub(crate) fn process(request: ProcessRequest<'_>) -> Result<ProcessedAudio> {
    let mut chain = Chain::new(request.left, request.right, request.source_sample_rate);
    chain.resample_to(request.target_sample_rate, request.resample_quality)?;
    chain.normalize(request.normalize);
    chain.dither(request.dither, request.bit_depth);
    Ok(match request.channels {
        ChannelMode::Stereo => {
            let (left, right) = chain.into_stereo();
            ProcessedAudio::Stereo { left, right }
        }
        ChannelMode::Mono => ProcessedAudio::Mono(chain.into_mono()),
    })
}
