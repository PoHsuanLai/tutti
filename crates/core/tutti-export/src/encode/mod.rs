//! Audio encoding stage.
//!
//! Accepts mastered stereo `[f32; 2]` frames from the `process` stage and
//! writes them to disk via a format-specific encoder. Also defines the
//! frame-based [`StreamingEncoder`](sink::StreamingEncoder) trait that the
//! streaming export path uses.
//!
//! The root [`encode`] function is pure orchestration: a [`PhaseGuard`] for
//! progress and a `match` on [`AudioFormat`]. Mono downmix (from
//! [`EncodeRequest::channels`]) happens inside each encoder.

#[cfg(any(feature = "wav", feature = "flac"))]
pub(crate) mod sink;

#[cfg(feature = "wav")]
pub(crate) mod wav;

#[cfg(feature = "flac")]
pub(crate) mod flac;

#[cfg(feature = "aiff")]
pub(crate) mod aiff;

#[cfg(feature = "ogg")]
pub(crate) mod ogg;

use crate::error::Result;
use crate::options::{AudioFormat, BitDepth, Flac, Ogg};
use crate::progress::{Phase, PhaseGuard};
use std::path::Path;
use tutti_types::ChannelLayout;

/// Everything an encoder needs to write one file: where, what format, what
/// per-format knobs, and the [`ChannelLayout`] it should emit (the frames it
/// receives are always stereo; a mono file is folded inside the encoder, and a
/// layout wider than stereo is served as stereo — there are only two source
/// channels).
pub(crate) struct EncodeRequest<'a> {
    pub path: &'a Path,
    pub format: AudioFormat,
    pub sample_rate: u32,
    pub bit_depth: BitDepth,
    pub channels: ChannelLayout,
    #[allow(dead_code)]
    pub flac: Flac,
    #[allow(dead_code)]
    pub ogg: Ogg,
}

/// Encode `frames` (mastered stereo) to `request.path`. A [`PhaseGuard`]
/// brackets the operation with `(Encode, 0.0)` / `(Encode, 1.0)` progress
/// events.
pub(crate) fn encode(
    frames: &[[f32; 2]],
    request: EncodeRequest<'_>,
    on_progress: &(dyn Fn(Phase, f32) + Send + Sync),
) -> Result<()> {
    let _phase = PhaseGuard::new(on_progress, Phase::Encode);
    match request.format {
        #[cfg(feature = "wav")]
        AudioFormat::Wav => wav::encode(frames, &request)?,
        #[cfg(not(feature = "wav"))]
        AudioFormat::Wav => {
            return Err(crate::Error::UnsupportedFormat("WAV not enabled".into()));
        }

        #[cfg(feature = "flac")]
        AudioFormat::Flac => flac::encode(frames, &request)?,
        #[cfg(not(feature = "flac"))]
        AudioFormat::Flac => {
            return Err(crate::Error::UnsupportedFormat("FLAC not enabled".into()));
        }

        #[cfg(feature = "aiff")]
        AudioFormat::Aiff => aiff::encode(frames, &request)?,
        #[cfg(not(feature = "aiff"))]
        AudioFormat::Aiff => {
            return Err(crate::Error::UnsupportedFormat("AIFF not enabled".into()));
        }

        #[cfg(feature = "ogg")]
        AudioFormat::OggVorbis => ogg::encode(frames, &request)?,
        #[cfg(not(feature = "ogg"))]
        AudioFormat::OggVorbis => {
            return Err(crate::Error::UnsupportedFormat("OGG not enabled".into()));
        }
    }

    Ok(())
}
