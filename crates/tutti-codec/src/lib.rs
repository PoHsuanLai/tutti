#![doc = include_str!("../README.md")]
//! **The audio file codecs**: WAV, AIFF, FLAC and Ogg Vorbis out behind one
//! `Encoder`, over any `Write + Seek` sink; WAV, FLAC, MP3 and Ogg in as a `Wave`
//! or a streaming `FileIn`.
//!
//! ## What is here
//!
//! - `Format` names the container and carries only the settings it accepts:
//!   `Wav(Pcm)`, `Aiff(Pcm)`, `Flac { depth: IntDepth, level: FlacLevel }` and
//!   `Vorbis { quality: Quality }`. FLAC at float and Ogg at a sample width are not
//!   expressible. Each variant exists only with its feature (`encode-wav`, `encode-aiff`, `encode-flac`,
//!   `encode-vorbis`), none on by default.
//! - `Spec` is what a file is written as: the format, the speakers (the topology,
//!   which is also the width) and the rate. `Spec::of_width` takes a width and
//!   states the engine's order for it.
//! - `Encoder` is `create(spec, sink)`, a fallible `write(block)` and `finish()`.
//!   `Encoder::create_file` writes beside the path and moves the file into place
//!   on `finish`; dropping the encoder first removes it, and so does `finish` after a
//!   failed `write`.
//! - `FileOut` is the file sink, `FileIn`'s mirror: a `Sink` over an
//!   `Encoder`, created from a `Spec` and a path, for any `Format` the build can
//!   write. It keeps whole frames only, latches the first write error (later
//!   writes are skipped and return `SinkFailed`) and `finalize` returns that
//!   error, else the result of `finish`. It writes in place rather than staged,
//!   because a recording cut short is worth keeping.
//! - `Wave::write(path, format)` saves a `Wave` at its own rate and topology.
//! - Decoding is plain functions and types of our own, no trait: `Wave::read`
//!   decodes a file into a `Wave` (a resident, planar sample buffer),
//!   `Wave::topology` and `FileIn::topology` say which speaker each channel feeds,
//!   `Wave::read_metadata` reads a header, `Wave::from_bytes` decodes a file held
//!   in memory, and `FileIn` streams a file as a `Source` with a
//!   sample-accurate `seek`. A failure is a `DecodeError`. `symphonia` does the
//!   work behind them, in `src/decode/` alone, and none of its types is in a
//!   signature or an error. `can_decode` and `decodable_extensions` say what this
//!   build reads. Each `decode-*` feature (`decode-wav`, `decode-flac`,
//!   `decode-mp3`, `decode-ogg`) pulls exactly one container.
//! - `Pcm`, `IntDepth` and `Sample` are the sample widths and the one quantization
//!   every container shares.
//!
//! ## What this crate does not own
//!
//! The pipeline in front of the encoder: pulling blocks from a render or a live
//! source, resampling and dithering. An encoder takes blocks already at the rate
//! and width the `Spec` states, and nothing here stops a caller from handing it
//! others. `tutti-export` is the offline pipeline and
//! `FileOut` the live one; both write through this crate, so a bounced
//! and a recorded file quantize and lay out a given sample identically.
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]
// With no format feature a `Format` has no value, so every body that follows
// one is unreachable by construction. That build compiles and writes nothing.
#![cfg_attr(
    not(encode),
    allow(unreachable_code, unused_mut, unused_variables, dead_code)
)]

#[cfg(feature = "encode-aiff")]
mod aiff;
#[cfg(all(test, encode))]
mod contract;
mod decode;
mod decode_error;
mod encoder;
mod error;
mod file_out;
#[cfg(feature = "encode-flac")]
mod flac;
mod format;
mod pcm;
mod spec;
mod staged;
#[cfg(test)]
mod test_sink;
#[cfg(feature = "encode-vorbis")]
mod vorbis;
#[cfg(feature = "encode-wav")]
mod wav;
mod wave;
#[cfg(any(feature = "encode-wav", decode))]
mod wave_mask;
mod writer;

#[cfg(feature = "encode-aiff")]
pub use aiff::AiffWriter;
#[cfg(decode)]
pub use decode::{FileIn, WaveMetadata};
pub use decode::{can_decode, decodable_extensions};
pub use decode_error::DecodeError;
pub use encoder::{Encoder, Finalized};
pub use error::{Codec, Error, Result, SpecError};
pub use file_out::FileOut;
#[cfg(feature = "encode-flac")]
pub use flac::FlacWriter;
pub use format::{FlacLevel, Format, Quality, QualityOutOfRange};
pub use pcm::{IntDepth, Pcm, Sample};
pub use spec::Spec;
#[cfg(feature = "encode-vorbis")]
pub use vorbis::VorbisWriter;
#[cfg(feature = "encode-wav")]
pub use wav::WavWriter;
pub use wave::Wave;
pub use writer::FormatWriter;
