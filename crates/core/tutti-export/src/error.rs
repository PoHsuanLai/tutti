//! What an export can fail with.
//!
//! One `#[non_exhaustive]` [`Error`] for the whole crate — every stage (plan,
//! render, resample, dither, encode) reports through it, so a caller matches one
//! type whichever entry point it used.

use std::io;
use thiserror::Error;

/// Anything that can go wrong rendering or writing an export.
///
/// `#[non_exhaustive]`: the codecs behind the format features each bring their
/// own failure modes, so a caller must keep a wildcard arm.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum Error {
    /// Creating, writing or finalizing the output file failed. Also carries the
    /// codec libraries' own I/O faults, via the `From` impls below.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    /// The requested [`AudioFormat`](crate::AudioFormat) cannot be written: its
    /// cargo feature is off, its extension is unrecognised, or the format does
    /// not support the configured bit depth (FLAC refuses `Float32`).
    #[error("Unsupported format: {0}")]
    UnsupportedFormat(String),

    /// A config field the encoder cannot act on — a zero sample rate, a zero
    /// channel count. Caught before any frame is rendered.
    #[error("Invalid configuration: {0}")]
    InvalidConfig(String),

    /// The codec rejected a block or failed to finalize its container. The
    /// string carries the library's own report.
    #[error("Encoding error: {0}")]
    Encoding(String),

    /// The render loop could not produce the planned frames.
    ///
    /// Reserved: no path in this crate constructs it today. A render is driven
    /// to a known frame count and its source never starves.
    #[error("Render error: {0}")]
    Render(String),

    /// rubato could not be constructed for this rate pair, or rejected a chunk.
    /// Converted from both of its error types by the `From` impls below.
    #[error("Resampling error: {0}")]
    Resample(String),

    /// The samples handed in are not something an encoder can write.
    ///
    /// Reserved: no path in this crate constructs it today.
    #[error("Invalid audio data: {0}")]
    InvalidData(String),

    /// A width the render cannot use, which means **zero** and nothing else.
    ///
    /// The frame width is a runtime `ChannelLayout`, so any positive count
    /// exports — including the 3- and 5-wide masters a fixed enumeration of
    /// widths could not express.
    #[error("Unsupported channel count: {0} (a render needs at least one channel)")]
    UnsupportedChannels(u16),

    /// A loudness measurement was asked for and could not be taken — EBU R128
    /// accepts 1–64 channels at 16 Hz–2.8 MHz.
    ///
    /// An error rather than a skipped measurement, because a normalized export
    /// that quietly writes un-normalized audio reports success and leaves no
    /// trace: nothing in [`Written`](crate::Written) records that the gain the
    /// caller asked for was never applied.
    #[error("Cannot measure loudness: {0}")]
    Unmeasurable(String),

    /// A native graph could not be forked for the render because the node at
    /// `key` cannot be: it handed the editor no fork source (a plugin, a mic
    /// monitor, a `Legacy` built unforkable), so a copy would drive or share
    /// the live node. From [`RenderGraph::fork`](crate::RenderGraph::fork);
    /// nothing was rendered.
    ///
    /// A variant of its own rather than inside [`Fork`](Self::Fork) because it
    /// is the one a host acts on — "remove or freeze this node" — and the key
    /// is what it acts on.
    #[error("Cannot export: node {key:?} cannot be forked for an offline render (a plugin, a mic monitor, or a node built unforkable)")]
    NotForkable {
        /// The node, as the live graph keys it.
        key: tutti_types::NodeKey,
    },

    /// A native graph could not be forked for the render for any reason other
    /// than [`NotForkable`](Self::NotForkable): the target node is missing or
    /// has no outputs, or the forked graph did not commit.
    #[error("Cannot fork the graph for export: {0}")]
    Fork(tutti_graph::ForkError),
}

/// `std::result::Result` with this crate's [`Error`](enum@Error) as the error
/// type.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(feature = "wav")]
impl From<hound::Error> for Error {
    fn from(e: hound::Error) -> Self {
        Self::Io(io::Error::other(e))
    }
}

impl From<rubato::ResamplerConstructionError> for Error {
    fn from(e: rubato::ResamplerConstructionError) -> Self {
        Self::Resample(e.to_string())
    }
}

impl From<rubato::ResampleError> for Error {
    fn from(e: rubato::ResampleError) -> Self {
        Self::Resample(e.to_string())
    }
}
