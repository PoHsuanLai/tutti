#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]
// With no format feature a `Format` has no value, so every body that follows
// one is unreachable by construction. That build compiles and writes nothing.
#![cfg_attr(not(encode), allow(unreachable_code, unused_mut, dead_code))]

#[cfg(feature = "encode-aiff")]
mod aiff;
#[cfg(all(test, encode))]
mod contract;
mod decode;
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
pub use decode::{DecodeError, FileIn, WaveMetadata};
pub use decode::{can_decode, decodable_extensions};
pub use encoder::Encoder;
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
