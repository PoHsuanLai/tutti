#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]
// With no format feature a `Format` has no value, so every body that follows
// one is unreachable by construction. That build compiles and writes nothing.
#![cfg_attr(not(codec), allow(unreachable_code, unused_mut, dead_code))]

#[cfg(feature = "aiff")]
mod aiff;
mod encoder;
mod error;
#[cfg(feature = "flac")]
mod flac;
mod format;
mod pcm;
mod spec;
mod staged;
#[cfg(test)]
mod test_sink;
#[cfg(feature = "vorbis")]
mod vorbis;
#[cfg(feature = "wav")]
mod wav;

pub use encoder::Encoder;
pub use error::{Codec, Error, Result, SpecError};
pub use format::{FlacLevel, Format, Quality, QualityOutOfRange};
pub use pcm::{IntDepth, Pcm, Sample};
pub use spec::Spec;
