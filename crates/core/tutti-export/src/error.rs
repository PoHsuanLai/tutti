//! What an export can fail with.
//!
//! One `#[non_exhaustive]` [`Error`](enum@Error) for the whole crate — every stage (plan,
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
    #[error("Unsupported format {format}: {reason}")]
    UnsupportedFormat {
        /// The format's name (`"FLAC"`); for an unrecognised extension, the
        /// extension lowercased and without its dot, or `"(none)"` when the
        /// path has none.
        format: String,
        /// Why it cannot be written.
        reason: UnsupportedReason,
    },

    /// A config field the encoder cannot act on. Caught before any frame is
    /// rendered.
    #[error("Invalid configuration: {0}")]
    InvalidConfig(#[from] ConfigError),

    /// A codec rejected a block or failed to finalize its container.
    /// `detail` is the library's own report.
    #[error("{codec} encoding error: {detail}")]
    Encoding {
        /// The codec that failed.
        codec: Codec,
        /// The library's report.
        detail: String,
    },

    /// The resampler could not be constructed for this rate pair, or rejected a
    /// chunk.
    #[error("Resampling error: {0}")]
    Resample(#[from] tutti_dsp::ResampleError),

    /// The resampler's input carry stopped shrinking after `chunks` chunks.
    /// A bug in the resampler, not in the caller's input.
    #[error("Resampling error: the resample carry stopped shrinking after {chunks} chunks")]
    ResampleStalled {
        /// The chunk count the carry should have drained within.
        chunks: usize,
    },

    /// A width the render cannot use, which means **zero** and nothing else.
    ///
    /// The frame width is a runtime `ChannelLayout`, so any positive count
    /// exports — including the 3- and 5-wide masters a fixed enumeration of
    /// widths could not express.
    #[error("Unsupported channel count: {0} (a render needs at least one channel)")]
    UnsupportedChannels(u16),

    /// The WAV file would exceed what its 32-bit sizes can describe (just under
    /// 4 GiB; RF64 is not written). `limit_frames` is the most frames a file of
    /// this width and depth holds. Raised before the file is created when the plan shows
    /// it, otherwise at the frame that crosses the limit, and the partial file
    /// is removed. Choose another format (FLAC, Ogg) or a shorter
    /// span.
    #[error(
        "The export does not fit a WAV file (more than {limit_frames} frames; 32-bit sizes, just under 4 GiB)"
    )]
    WavTooLarge {
        /// The frames a WAV of this width and depth can hold.
        limit_frames: u64,
    },

    /// A loudness measurement was asked for and could not be taken — EBU R128
    /// accepts 1–64 channels at 16 Hz–2.8 MHz.
    ///
    /// An error rather than a skipped measurement, because a normalized export
    /// that quietly writes un-normalized audio reports success and leaves no
    /// trace: nothing in [`Written`](crate::Written) records that the gain the
    /// caller asked for was never applied.
    #[error("Cannot measure loudness: the meter rejected {channels} channels at {sample_rate} Hz")]
    Unmeasurable {
        /// The channel count of the audio.
        channels: u16,
        /// Its sample rate, in Hz.
        sample_rate: f64,
    },

    /// The graph could not be forked for the render because the node at
    /// `key` cannot be: it handed the editor no fork source (a mic monitor, a
    /// node inserted `Unforkable`), so a copy would share the live node. From [`RenderGraph::fork`](crate::RenderGraph::fork);
    /// nothing was rendered.
    ///
    /// A variant of its own rather than inside [`Fork`](Self::Fork) because it
    /// is the one a host acts on — "remove or freeze this node" — and the key
    /// is what it acts on.
    #[error(
        "Cannot export: node {key:?} cannot be forked for an offline render (a mic monitor, or a node built unforkable)"
    )]
    NotForkable {
        /// The node, as the live graph keys it.
        key: tutti_types::NodeKey,
    },

    /// The graph could not be forked for the render for any reason other
    /// than [`NotForkable`](Self::NotForkable): the target node is missing or
    /// has no outputs, or the forked graph did not commit.
    #[error("Cannot fork the graph for export: {0}")]
    Fork(tutti_graph::ForkError),

    /// A forked unit failed **while the render ran**: a hosted plugin's
    /// server crashed, or stopped answering (`kind`). Past that point the
    /// render holds silence where the unit's output belongs, so the export is
    /// reported as failed rather than written as if it had succeeded. The
    /// editor's `fork_health` is checked after every graph render
    /// (see "When a forked unit fails" in tutti-graph's `fork` docs). A file
    /// export may already have written the file; it is not a valid render.
    #[error("Export failed: forked node {key:?} {kind:?} during the render: {cause}")]
    ForkFailed {
        /// The node, as the live graph keys it.
        key: tutti_types::NodeKey,
        /// Crashed or timed out.
        kind: tutti_graph::ForkFaultKind,
        /// The unit's own account (for a plugin, a
        /// `tutti_plugin::PluginRenderFault`).
        cause: tutti_graph::ForkCause,
    },
}

/// A config the export cannot act on ([`Error::InvalidConfig`]).
#[derive(Error, Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum ConfigError {
    /// The graph's editor does not feed its executor.
    #[error("the graph's editor does not feed its executor; pass the pair built together")]
    UnpairedGraph,
    /// The graph is prepared at another rate than the render.
    #[error(
        "the graph is prepared at {prepared} Hz and the render is at {render} Hz; \
         prepare it with `RenderGraph::prepare` or fork it with `RenderGraph::fork`"
    )]
    RateMismatch {
        /// The rate the graph was prepared at, in Hz.
        prepared: f64,
        /// The rate the render asks for, in Hz.
        render: f64,
    },
    /// The channel topology and the file differ in width.
    #[error("the topology is {topology:?} wide but the file has {file:?}")]
    TopologyWidth {
        /// The topology's layout.
        topology: tutti_types::ChannelLayout,
        /// The file's layout.
        file: tutti_types::ChannelLayout,
    },
    /// The export's target node has no audio outputs.
    #[error("export target node has no audio outputs")]
    TargetHasNoOutputs,
    /// The graph has no outputs to export.
    #[error("the graph has no outputs to export")]
    GraphHasNoOutputs,
    /// A prepare hook left the forked graph in a state that cannot commit.
    #[error("the prepare hook left the forked graph uncommittable: {0:?}")]
    PrepareHookUncommittable(tutti_graph::CommitError),
    /// More channels than the container's header can count.
    #[error("{channels} channels exceed the format's limit of {max}")]
    TooManyChannels {
        /// The requested channel count.
        channels: u16,
        /// The most the format holds.
        max: u16,
    },
    /// A sample rate of zero.
    #[error("sample rate must be non-zero")]
    ZeroSampleRate,
    /// A channel count of zero.
    #[error("channel count must be non-zero")]
    ZeroChannels,
}

/// The codec behind an [`Error::Encoding`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Codec {
    /// FLAC.
    Flac,
    /// AIFF and AIFF-C.
    Aiff,
    /// Ogg Vorbis.
    Ogg,
}

impl std::fmt::Display for Codec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Flac => "FLAC",
            Self::Aiff => "AIFF",
            Self::Ogg => "Ogg",
        })
    }
}

/// Why an [`AudioFormat`](crate::AudioFormat) cannot be written
/// ([`Error::UnsupportedFormat`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum UnsupportedReason {
    /// The format's cargo feature is off in this build.
    FeatureDisabled,
    /// No format is written for the file extension.
    UnknownExtension,
    /// The format cannot hold the configured bit depth (FLAC refuses `Float32`).
    BitDepth,
}

impl std::fmt::Display for UnsupportedReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::FeatureDisabled => "its cargo feature is not enabled",
            Self::UnknownExtension => "the extension is not recognised",
            Self::BitDepth => "it cannot hold the configured bit depth",
        })
    }
}

impl Error {
    #[cfg_attr(
        not(any(feature = "flac", feature = "aiff", feature = "ogg")),
        expect(dead_code, reason = "only the flac, aiff and ogg encoders report this")
    )]
    pub(crate) fn encoding(codec: Codec, detail: impl Into<String>) -> Self {
        Self::Encoding {
            codec,
            detail: detail.into(),
        }
    }
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
