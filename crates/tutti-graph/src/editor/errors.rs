//! Why an editor call was refused.

use tutti_types::{Latency, Samples};

use crate::compile::{CompileError, VerifyError};
use crate::exec::QUEUE_CAPACITY;
use crate::spec::GraphInvalid;
use crate::{EventTapId, MeterId, NodeKey, TapId};

/// Why [`Editor::commit`](crate::Editor::commit) failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CommitError {
    /// The spec is malformed.
    #[error("graph is invalid ({} faults)", .0.len())]
    Invalid(Vec<GraphInvalid>),
    /// The spec did not compile against the units' shapes.
    #[error("{0}")]
    Compile(CompileError),
    /// A node in the spec has no unit — it was added to the spec directly
    /// rather than through [`Editor::insert`](crate::Editor::insert).
    #[error("node {} has no unit", .node.0)]
    MissingUnit {
        /// The node.
        node: NodeKey,
    },
    /// [`QUEUE_CAPACITY`] commits are out. Nothing was compiled or sent and
    /// the plan did not advance; retry once the executor has run a block.
    #[error("{} commits in flight", QUEUE_CAPACITY)]
    Backpressure,
    /// A [`reprepare`](crate::Editor::reprepare) is waiting for the executor to
    /// hand its units back. Nothing was compiled or sent; retry once the
    /// executor has run a block (the next [`collect`](crate::Editor::collect) or
    /// `commit` finishes the re-prepare first).
    #[error("a re-prepare is waiting for its units")]
    Repreparing,
    /// A re-prepare failed after the units had left the executor — a node
    /// panicked in [`Node::prepare`](crate::Node::prepare), or the re-prepared shapes no longer
    /// compile (a unit's event resolution coarsened under a marked edge,
    /// say). The units are gone and the executor is suspended: it renders
    /// silence, forever, and never panics. Every later call on this editor
    /// returns this error. **Recovery: build a new editor/executor pair.**
    #[error("the editor is poisoned ({cause}); build a new pair")]
    Poisoned {
        /// What went wrong.
        cause: String,
    },
    /// The spec bypasses, mixes or drives the mix of a node with no pass
    /// ([`Editor::check_pass`](crate::Editor::check_pass)): its input would go where a guess put it.
    /// Nothing was compiled or sent, and the last plan goes on sounding.
    ///
    /// **Recovery:** it is usually a node replaced while bypassed or mixed. Set
    /// that node's mode to [`Active`](crate::NodeMode::Active)
    /// ([`Editor::set_mode`](crate::Editor::set_mode)), its mix to [`Mix::WET`](tutti_types::Mix::WET) ([`Editor::set_mix`](crate::Editor::set_mix)),
    /// or take the lane off its mix, and commit again; or declare the node's
    /// buses so it has a pass.
    #[error("node {} is bypassed or mixed with no pass: {error}", .node.0)]
    Unplaced {
        /// The node.
        node: NodeKey,
        /// What it would need to declare.
        error: crate::PassError,
    },
    /// The graph has more global outputs than the host running the executor
    /// can take ([`Limits::max_global_outputs`](crate::Limits::max_global_outputs), set with
    /// [`Editor::set_limits`](crate::Editor::set_limits)). Nothing was compiled or sent.
    #[error("{outputs} global outputs; the host takes {limit}")]
    TooManyOutputs {
        /// The graph's global outputs.
        outputs: usize,
        /// The host's limit.
        limit: usize,
    },
    /// A re-prepare asked for a larger block than the host running the
    /// executor can take ([`Limits::max_block`](crate::Limits::max_block)). Nothing was sent; the
    /// graph runs on at its current `Prepare`.
    #[error("a {max_block}-frame MaxBlock; the host takes {limit}")]
    BlockTooLong {
        /// The `MaxBlock` asked for.
        max_block: usize,
        /// The host's limit.
        limit: usize,
    },
    /// [`Editor::replace`](crate::Editor::replace) was handed a unit whose shape differs from the
    /// running one's in more than its tail — ports, latency, in-place
    /// acceptance, event resolution or event capacity. Nothing changed; swap it with
    /// [`Editor::insert`](crate::Editor::insert) instead.
    #[error("node {} cannot crossfade to a unit of another shape", .node.0)]
    FadeShape {
        /// The node.
        node: NodeKey,
    },
    /// [`Editor::replace`](crate::Editor::replace) names a node with no running unit to fade from:
    /// not in the plan sent last, or removed since.
    #[error("node {} has no running unit to fade from", .node.0)]
    NotRunning {
        /// The node.
        node: NodeKey,
    },
    /// A delta handed to [`Editor::package`](crate::Editor::package) carries a crossfade the
    /// verifier refuses ([`Plan::verify_fades`](crate::Plan::verify_fades)). Nothing was
    /// sent.
    #[error("{0}")]
    Fade(VerifyError),
    /// A tap's lanes in the spec no longer match the width it was opened
    /// with ([`Editor::open_tap`](crate::Editor::open_tap) fixed the reader's and the writer's width
    /// there): the spec's `TapSpec::Ports` sources were edited. Nothing was sent.
    /// Close the tap and open another to change its width.
    #[error("tap {} names {sources} lanes but was opened with {opened}", .tap.0)]
    TapWidth {
        /// The tap.
        tap: TapId,
        /// The lanes the spec names now.
        sources: usize,
        /// The lanes it was opened with.
        opened: usize,
    },
    /// A meter's lanes in the spec do not fit the cells it was opened with
    /// ([`Editor::open_meter`](crate::Editor::open_meter) fixed them there): the spec's
    /// `MeterSpec::Ports` was edited, or the outputs a master meter follows
    /// grew past [`MAX_ROOT_CHANNELS`](crate::MAX_ROOT_CHANNELS). Nothing was sent. Close the meter and
    /// open another to change its width.
    #[error("meter {} names {sources} lanes but its cells hold {capacity}", .meter.0)]
    MeterWidth {
        /// The meter.
        meter: MeterId,
        /// The lanes the spec names now.
        sources: usize,
        /// The lanes its cells hold.
        capacity: usize,
    },
    /// No node at this key ([`Editor::set_latency`](crate::Editor::set_latency)).
    #[error("no node {}", .node.0)]
    NoSuchNode {
        /// The key.
        node: NodeKey,
    },
    /// A latency past what PDC compensates
    /// (`crate::MAX_NODE_LATENCY`), refused rather than
    /// silently clamped ([`Editor::set_latency`](crate::Editor::set_latency)).
    #[error("node {}: latency {} frames; PDC compensates at most {}", .node.0, .latency.samples().get(), .limit.samples().get())]
    LatencyTooLong {
        /// The node.
        node: NodeKey,
        /// The latency asked for.
        latency: Latency,
        /// The most PDC compensates.
        limit: Latency,
    },
    /// A finite tail past what the graph's tail figures bound
    /// (`crate::MAX_NODE_TAIL`), refused rather than silently clamped
    /// ([`Editor::set_tail`](crate::Editor::set_tail)).
    #[error("node {}: tail {} frames; a finite tail is at most {}", .node.0, .tail.get(), .limit.get())]
    TailTooLong {
        /// The node.
        node: NodeKey,
        /// The tail asked for, in frames.
        tail: Samples,
        /// The longest finite tail a node may declare.
        limit: Samples,
    },
}

/// What [`Editor::close_tap`](crate::Editor::close_tap), [`Editor::close_event_tap`](crate::Editor::close_event_tap) and
/// [`Editor::close_meter`](crate::Editor::close_meter) found not open: the id that was asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CloseError {
    /// No plan tap with this id is open.
    #[error("no tap {} is open", .0.0)]
    Tap(TapId),
    /// No event tap with this id is open.
    #[error("no event tap {} is open", .0.0)]
    EventTap(EventTapId),
    /// No meter with this id is open.
    #[error("no meter {} is open", .0.0)]
    Meter(MeterId),
}

/// Why [`Editor::open_tap`](crate::Editor::open_tap), [`Editor::open_event_tap`](crate::Editor::open_event_tap) or
/// [`Editor::open_meter`](crate::Editor::open_meter) refused: a source that
/// does not exist in the spec **at the time of the call**, edits not yet
/// committed included. Nothing was opened.
///
/// Only the open is checked. A port that narrows or vanishes *later* (a node
/// replaced by one with fewer outputs, a removed node) makes its lane read
/// silence and the commit succeeds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum MissingSource {
    /// No node has this key.
    #[error("no node {}", .node.0)]
    Node {
        /// The key.
        node: NodeKey,
    },
    /// The node has no such output port.
    #[error("node {} has no output {port} (it has {width})", .node.0)]
    Port {
        /// The node.
        node: NodeKey,
        /// The port asked for.
        port: u16,
        /// The node's output width.
        width: usize,
    },
    /// The graph has no such global input channel.
    #[error("no global input {channel} (the graph has {width})")]
    Global {
        /// The channel asked for.
        channel: u16,
        /// The graph's input width.
        width: usize,
    },
    /// The node has no such event input port.
    #[error("node {} has no event input {port} (it has {width})", .node.0)]
    EventInPort {
        /// The node.
        node: NodeKey,
        /// The port asked for.
        port: u16,
        /// The node's event input width.
        width: usize,
    },
    /// The node has no such event output port.
    #[error("node {} has no event output {port} (it has {width})", .node.0)]
    EventPort {
        /// The node.
        node: NodeKey,
        /// The port asked for.
        port: u16,
        /// The node's event output width.
        width: usize,
    },
}
