//! What an export can fail with.
//!
//! One `#[non_exhaustive]` [`Error`](enum@Error) for the whole crate — every stage (plan,
//! render, resample, dither, encode) reports through it, so a caller matches one
//! type whichever entry point it used.

use crate::TargetError;
use std::io;
use thiserror::Error;
use tutti_types::ChannelLayout;

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
    #[error("Unsupported channel count: 0 (a render needs at least one channel)")]
    UnsupportedChannels,

    /// A loudness measurement was asked for and the audio cannot be
    /// normalized by it (`reason` says why): the meter rejects it, or the
    /// reading it gives is not a measurement of the programme.
    ///
    /// An error rather than a skipped or invented gain, because a normalized
    /// export that quietly writes un-normalized audio (or applies +47 dB to a
    /// hiss at the gate's floor) reports success and leaves no trace: nothing
    /// in [`Written`](crate::Written) records that the gain the caller asked
    /// for was never applied, or what it was. A file export leaves the target
    /// as it was.
    #[error("Cannot normalize: {channels:?} at {sample_rate:?} is unmeasurable: {reason}")]
    Unmeasurable {
        /// The output whose audio it is, by its index in the list given to
        /// [`render`](crate::render()); `None` for planes measured on their own
        /// ([`Normalize::gain_for_rendered`](crate::Normalize::gain_for_rendered)),
        /// which belong to no output.
        output: Option<usize>,
        /// The channel layout of the audio.
        channels: ChannelLayout,
        /// Its sample rate.
        sample_rate: tutti_types::SampleRate,
        /// Why the reading is not usable.
        reason: Unmeasured,
    },

    /// The graph could not be forked for the render because the node at
    /// `key` cannot be: it handed the editor no fork source (a mic monitor, a
    /// node inserted `Unforkable`), so a copy would share the live node. From the session's fork of a live graph;
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
        key: tutti_graph::NodeKey,
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
    /// a file output is not put in place. The render stops at the first block
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

    /// The render would hold its planes in memory (a [`BufferTarget`](crate::BufferTarget)) and
    /// the allocation for `frames` frames of `channels` channels failed or
    /// overflowed. Nothing was rendered. A file export streams and never asks
    /// for this much.
    #[error("Cannot hold the render in memory: {frames:?} frames of {channels:?} channels")]
    RenderTooLarge {
        /// Frames per plane the render needed.
        frames: tutti_types::Samples,
        /// The planes it needed them for.
        channels: ChannelLayout,
    },

    /// An output cannot be rendered as it was given ([`TargetError`] says why);
    /// `output` is its index in the list given to [`render`](crate::render()).
    /// Nothing was rendered.
    #[error("output {output} cannot be rendered: {why}")]
    Target {
        /// The output's index.
        output: usize,
        /// What is wrong with it.
        why: TargetError,
    },

    /// A render was given no outputs.
    #[error("a render needs at least one output")]
    NoOutputs,

    /// A tap's ring lost frames: the render did not drain it as fast as the
    /// graph filled it, and what was written would have a hole. With rings of
    /// `2 x max_block` frames and one drain after each block it cannot happen;
    /// this is the guard, not a mode.
    #[error("output {output}'s tap lost {lost:?} frames: its ring was too small")]
    TapOverrun {
        /// The output's index.
        output: usize,
        /// The frames lost.
        lost: tutti_types::Samples,
    },

    /// A tap had fewer frames than the block rendered: it was never committed
    /// to the graph being rendered, or its source is not in it.
    #[error("output {output}'s tap held less than the block rendered; was it committed?")]
    TapStarved {
        /// The output's index.
        output: usize,
    },

    /// A rename failed after earlier files were put in place: the files that
    /// landed (`landed`) are kept, nothing is rolled back, and no output that
    /// did not land is left behind. The failure of the first rename is the
    /// plain error: nothing had landed then.
    #[error(
        "{} file(s) were put in place before a rename failed: {cause}",
        .landed.len()
    )]
    NotCommitted {
        /// The files that landed, in the order they were given.
        landed: Vec<std::path::PathBuf>,
        /// The rename's failure.
        cause: tutti_codec::Error,
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
        key: tutti_graph::NodeKey,
        /// Crashed or timed out.
        kind: tutti_graph::ForkFaultKind,
        /// The unit's own account (for a plugin, a
        /// `tutti_plugin::PluginRenderFault`).
        cause: tutti_graph::ForkCause,
    },
}

/// Why audio cannot be normalized ([`Error::Unmeasurable`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Unmeasured {
    /// The meter does not take this width or rate: EBU R128 accepts 1 to 64
    /// channels at 16 Hz to 2.8 MHz.
    Format,
    /// [`Normalize::Lufs`](crate::Normalize::Lufs): no part of the audio
    /// passed the integrated loudness gate. It is silent, quieter than the
    /// absolute gate (-70 LUFS), or shorter than the 400 ms block the gate is
    /// judged in; any gain chosen from that reading would be made up.
    BelowGate,
    /// [`Normalize::Peak`](crate::Normalize::Peak): the true peak is at the
    /// floor ([`Db::FLOOR`](tutti_types::Db::FLOOR)), which is silence.
    Silent,
}

impl std::fmt::Display for Unmeasured {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Format => "the meter does not take this width or rate",
            Self::BelowGate => {
                "nothing passed the loudness gate (silent, under -70 LUFS, or under 400 ms)"
            }
            Self::Silent => "the true peak is at the floor (silence)",
        })
    }
}

/// A config the export cannot act on ([`Error::InvalidConfig`]).
#[derive(Error, Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum ConfigError {
    /// The graph's editor does not feed its executor.
    #[error("the graph's editor does not feed its executor; pass the pair built together")]
    UnpairedGraph,
    /// The graph was prepared at a rate that is not finite and positive
    /// ([`RenderGraph::assemble`](crate::RenderGraph::assemble)).
    #[error("the graph is prepared at {prepared} Hz, which is not a sample rate")]
    InvalidRate {
        /// The rate the graph was prepared at, in Hz.
        prepared: f64,
    },
    /// The graph is prepared at another rate than the render.
    #[error(
        "the graph is prepared at {prepared} Hz and the render is at {render} Hz; \
         prepare it with `RenderGraph::prepare`"
    )]
    RateMismatch {
        /// The rate the graph was prepared at, in Hz.
        prepared: f64,
        /// The rate the render asks for, in Hz.
        render: f64,
    },
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
