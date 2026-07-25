//! Streaming-encoder abstraction.
//!
//! Used by the streaming export path: the render driver pushes frame blocks
//! (already dithered by a [`DitherOut`](crate::process::DitherOut)) to a
//! [`StreamingEncoder`], which writes them incrementally. The
//! [`EncoderOut`](crate::render::EncoderOut) sink flattens each `[f32; CH]`
//! block into an interleaved `&[f32]` before handing it over, so the encoder
//! speaks a plain runtime channel count — matching the hound/vorbis/flac APIs
//! and keeping codecs off the const-generic frame width. The encoder knows its
//! own [`ChannelLayout`] (folding to mono at the file boundary when `count() ==
//! 1`).
//!
//! Encoders that can stream (currently WAV) implement the trait. Encoders
//! that cannot (AIFF, OGG at present) are not wired into the opener — the
//! opener returns `UnsupportedFormat` for them.

use crate::error::{Error, Result};
use crate::options::{AudioFormat, BitDepth, Flac, Ogg};
use std::path::Path;
use tutti_types::ChannelLayout;

/// Accepts interleaved frame blocks and encodes them incrementally.
pub(crate) trait StreamingEncoder {
    /// Write one block of interleaved samples: `interleaved.len()` must be a
    /// multiple of `channels`, laid out frame-major (`[f0c0, f0c1, …, f1c0, …]`).
    /// `channels` is the source width; the encoder maps it onto its own
    /// configured [`ChannelLayout`] (folding to mono when it emits a mono file).
    fn write_interleaved(&mut self, interleaved: &[f32], channels: u16) -> Result<()>;

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
