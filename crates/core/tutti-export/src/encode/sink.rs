//! Streaming-encoder abstraction.
//!
//! Used by the streaming export path: the render driver pushes `[f32; 2]` frame
//! blocks (already dithered by a [`DitherOut`](crate::process::DitherOut)) to a
//! [`StreamingEncoder`], which writes them incrementally. The encoder knows its
//! own [`ChannelLayout`] and folds to mono at the file boundary if asked.
//!
//! Encoders that can stream (currently WAV) implement the trait. Encoders
//! that cannot (AIFF, OGG at present) are not wired into the opener — the
//! opener returns `UnsupportedFormat` for them.

use crate::error::{Error, Result};
use crate::options::{AudioFormat, BitDepth, Flac, Ogg};
use tutti_types::ChannelLayout;
use std::path::Path;

/// Accepts stereo `[f32; 2]` frame blocks and encodes them incrementally.
pub(crate) trait StreamingEncoder {
    fn write_frames(&mut self, frames: &[[f32; 2]]) -> Result<()>;

    /// Flush any pending buffers, update headers, and close the file. Must
    /// be called exactly once.
    fn finalize(self: Box<Self>) -> Result<()>;
}

/// Open a streaming encoder for the format implied by `format` and the
/// configured rate/bit-depth. Returns `UnsupportedFormat` for formats that
/// do not implement streaming (AIFF, OGG, FLAC for now).
pub(crate) fn open_stream_encoder(
    path: &Path,
    format: AudioFormat,
    sample_rate: u32,
    bit_depth: BitDepth,
    channels: ChannelLayout,
    _flac: Flac,
    _ogg: Ogg,
) -> Result<Box<dyn StreamingEncoder>> {
    match format {
        #[cfg(feature = "wav")]
        AudioFormat::Wav => super::wav::open_stream(path, sample_rate, bit_depth, channels),
        #[cfg(not(feature = "wav"))]
        AudioFormat::Wav => Err(Error::UnsupportedFormat("WAV not enabled".into())),

        AudioFormat::Flac | AudioFormat::Aiff | AudioFormat::OggVorbis => Err(
            Error::UnsupportedFormat(format!("Streaming export not supported for {format:?}")),
        ),
    }
}
