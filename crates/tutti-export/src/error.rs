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
    /// The sink failed: the output file could not be created, written (a full
    /// disk) or moved into place.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    /// A config field the encoder cannot act on. Caught before any frame is
    /// rendered.
    #[error("Invalid configuration: {0}")]
    InvalidConfig(#[from] ConfigError),

    /// The encoder refused the spec or a block: a spec the format cannot be
    /// written for, a WAV file past its size limit, or a codec failure. The
    /// file is removed. (The sink failing is [`Io`](Self::Io).)
    #[error(transparent)]
    Encode(tutti_codec::Error),

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

    /// A width the render cannot use, which means zero and nothing else.
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

    /// The graph dropped events during the render (a port past its capacity,
    /// a full delay): the audio of a dropped note-on or note-off is wrong, and
    /// a note whose note-off was dropped may sound to the end. Raised under
    /// [`EventLossPolicy::Fail`](crate::EventLossPolicy::Fail), the default;
    /// a file export removes the file. The render stops at the first block
    /// that lost an event, so `count` is the executor's `lost_event_count`
    /// from when the render began (or, for a graph that had a commit
    /// installed since its last render, from that render's end) to the end of
    /// that block: events that never arrived, not a note-off that was
    /// refused and sent again. [`write_buffers`](crate::write_buffers) raises
    /// it too, for planes whose render lost events.
    #[error("Export failed: the graph dropped {count} events during the render")]
    EventsDropped {
        /// How many.
        count: u64,
    },

    /// A forked unit failed while the render ran: a hosted plugin's
    /// server crashed, or stopped answering (`kind`). Past that point the
    /// render holds silence where the unit's output belongs, so the export is
    /// reported as failed rather than written as if it had succeeded. The
    /// editor's `fork_health` is checked after every graph render
    /// (see "When a forked unit fails" in tutti-graph's `fork` docs), and for a
    /// file export before the file is put in place: the file at the path keeps
    /// what it held.
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
    /// The export's target node has no audio outputs.
    #[error("export target node has no audio outputs")]
    TargetHasNoOutputs,
    /// The graph has no outputs to export.
    #[error("the graph has no outputs to export")]
    GraphHasNoOutputs,
    /// A prepare hook left the forked graph in a state that cannot commit.
    #[error("the prepare hook left the forked graph uncommittable: {0:?}")]
    PrepareHookUncommittable(tutti_graph::CommitError),
    /// The graph's channels have no place in the file's: the topology the file
    /// states cannot take them, or is not as wide as the file.
    #[error("the graph's channels cannot be placed into the file's: {0}")]
    Placement(tutti_types::FoldError),
}

impl From<tutti_codec::Error> for Error {
    fn from(error: tutti_codec::Error) -> Self {
        match error {
            tutti_codec::Error::Io(e) => Self::Io(e),
            other => Self::Encode(other),
        }
    }
}

/// `std::result::Result` with this crate's [`Error`](enum@Error) as the error
/// type.
pub type Result<T> = std::result::Result<T, Error>;
