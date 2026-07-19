//! Streaming-encoder abstraction.
//!
//! Used by the streaming export path: the render driver hands blocks to a
//! [`StreamProcessor`], which produces [`Chunk`]s that go to a
//! [`StreamingEncoder`].
//!
//! Encoders that can stream (currently WAV) implement the trait. Encoders
//! that cannot (AIFF, OGG at present) are not wired into the opener — the
//! opener returns `UnsupportedFormat` for them.

use crate::error::{Error, Result};
use crate::options::{AudioFormat, BitDepth, ChannelMode, Flac, Ogg};
use crate::process::Chunk;
use std::path::Path;

/// Accepts pre-dithered, pre-downmixed chunks produced by
/// [`crate::process::StreamProcessor`].
pub(crate) trait StreamingEncoder {
    fn write_chunk(&mut self, chunk: Chunk) -> Result<()>;

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
    channels: ChannelMode,
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
