//! Audio encoding stage.
//!
//! Accepts a [`Chunk`] buffer from the `process` stage and writes
//! it to disk via a format-specific encoder. Also defines the chunk-based
//! [`StreamingEncoder`] trait that the streaming export path uses.
//!
//! The root [`encode`] function is pure orchestration: a [`PhaseGuard`] for
//! progress and a `match` on [`AudioFormat`].

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
use crate::process::Chunk;
use crate::progress::{Phase, PhaseGuard};
use std::path::Path;

/// Everything an encoder needs to write one file: where, what format, what
/// per-format knobs. Channel mode is carried by the [`Chunk`]
/// enum itself, so it is not duplicated here.
pub(crate) struct EncodeRequest<'a> {
    pub path: &'a Path,
    pub format: AudioFormat,
    pub sample_rate: u32,
    pub bit_depth: BitDepth,
    #[allow(dead_code)]
    pub flac: Flac,
    #[allow(dead_code)]
    pub ogg: Ogg,
}

/// Encode `audio` to `request.path`. A [`PhaseGuard`] brackets the operation
/// with `(Encode, 0.0)` / `(Encode, 1.0)` progress events.
pub(crate) fn encode(
    audio: Chunk,
    request: EncodeRequest<'_>,
    on_progress: &(dyn Fn(Phase, f32) + Send + Sync),
) -> Result<()> {
    let _phase = PhaseGuard::new(on_progress, Phase::Encode);
    match request.format {
        #[cfg(feature = "wav")]
        AudioFormat::Wav => wav::encode(audio, &request)?,
        #[cfg(not(feature = "wav"))]
        AudioFormat::Wav => {
            return Err(crate::Error::UnsupportedFormat("WAV not enabled".into()));
        }

        #[cfg(feature = "flac")]
        AudioFormat::Flac => flac::encode(audio, &request)?,
        #[cfg(not(feature = "flac"))]
        AudioFormat::Flac => {
            return Err(crate::Error::UnsupportedFormat("FLAC not enabled".into()));
        }

        #[cfg(feature = "aiff")]
        AudioFormat::Aiff => aiff::encode(audio, &request)?,
        #[cfg(not(feature = "aiff"))]
        AudioFormat::Aiff => {
            return Err(crate::Error::UnsupportedFormat("AIFF not enabled".into()));
        }

        #[cfg(feature = "ogg")]
        AudioFormat::OggVorbis => ogg::encode(audio, &request)?,
        #[cfg(not(feature = "ogg"))]
        AudioFormat::OggVorbis => {
            return Err(crate::Error::UnsupportedFormat("OGG not enabled".into()));
        }
    }

    Ok(())
}
