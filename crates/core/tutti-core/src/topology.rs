//! Compiling a [`Topology`](tutti_types::graph::Topology) value into a runnable
//! [`Net`].
//!
//! # Ownership rule
//!
//! [`tutti_types::graph::Topology`] **is** the graph. `Net` is one *interpreter*
//! of it, and this module is the only place the two meet. The arrow runs one
//! way: [`compile`] reads a value and writes a runtime. Nothing here — and
//! nothing downstream of here — reads a topology back out of a `Net`.
//!
//! That is the whole reason the layer exists. Today `Net` is the only record of
//! what is wired to what, so answering the question means reading `Net::source`
//! port by port with a live runtime in hand, and any layer wanting to remember
//! what it built keeps shadow state that goes stale. With the value in hand,
//! every static question — latency, tail, evaluation order, "did anything
//! change" — is a fold over the `Topology`, and `Net` is left doing the one job
//! it is good at: rendering blocks. See the [`graph`](tutti_types::graph) module
//! docs for the value's side of the same rule.
//!
//! # What this does not do
//!
//! It is not how the live graph is built. `bevy_tutti::graph::wire` does build
//! a [`Topology`](tutti_types::graph::Topology) every rebuild, but it applies
//! that value *incrementally* to the running `Net` (`bevy_tutti`'s
//! `topology::apply` rewrites only the ports that differ) rather than compiling
//! a fresh one, because the live `Net` owns state no value can rebuild — its
//! backend handoff, hosted plugins mutated in place, queued crossfades. The
//! `bevy_tutti::graph::topology` module docs give the two constraints.
//!
//! [`compile`] is the from-scratch route, for a caller with no live backend to
//! preserve. Only tests call it today (`tests/topology_compile.rs`), and that is
//! its job for now: it keeps the equality the value layer rests on —
//! `latency::plan(&topology) == latency::plan(&compile(&topology))` — checkable.

use std::collections::BTreeMap;

use fundsp::audiounit::AudioUnit;
use fundsp::net::{Net, NodeId, Source as NetSource};
use tutti_types::graph::{Edge, InPort, NodeKey, NodeSpec, Source, Valid};
use tutti_types::ChannelLayout;

use crate::SampleRate;

/// Builds the DSP unit a [`NodeSpec`] names.
///
/// Object-safe on purpose: [`compile`] takes `&dyn Catalog`, because the set of
/// node kinds is **open** — a plugin host, a WASM extension host and the
/// built-ins each register their own, and none can name the others' types. That
/// is the same reason `NodeSpec::kind` is a string rather than an enum.
///
/// # Why the sample rate is an argument
///
/// A node that sizes a buffer in *samples* — a delay line, an FFT window, a
/// lookahead ring — cannot be built without knowing the rate, and building it at
/// the wrong rate and fixing it afterwards is what `AudioUnit::set_sample_rate`
/// is for on a *running* node, not on one being constructed. `Net::push` does
/// call `set_sample_rate` on every unit it takes, so a catalog that ignores this
/// argument is still correct; one that allocates from it is spared a reallocation
/// on the first block.
pub trait Catalog {
    /// Build the unit `spec` names, or `None` if this catalog has no such kind.
    ///
    /// `None` is not a failure of the catalog — it is how a caller composes two
    /// of them. It becomes [`CompileError::UnknownKind`] only when no catalog
    /// claims the kind.
    fn build(&self, spec: &NodeSpec, sample_rate: SampleRate) -> Option<Box<dyn AudioUnit>>;
}

/// Why a [`Valid`] topology could not be turned into a [`Net`].
///
/// Every variant names the node, because "the graph failed to compile" sends the
/// reader back to the whole graph.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompileError {
    /// No catalog builder claimed this node's kind.
    UnknownKind {
        /// The node whose kind is unknown.
        node: NodeKey,
        /// The kind string, verbatim.
        kind: String,
    },
    /// The unit the catalog built disagrees with the width the spec declared.
    ///
    /// [`Topology::validate`](tutti_types::graph::Topology::validate) checks
    /// edges against the *declared* widths; only the catalog knows the built
    /// unit's. Without this check a spec that lies about its arity reaches
    /// `Net::set_source`, which `assert!`s on the port index — so the failure
    /// would be a panic naming a port, from inside a graph rebuild, instead of
    /// an error naming the node and the catalog that built it.
    WidthMismatch {
        /// The node whose unit disagrees with its spec.
        node: NodeKey,
        /// The kind string, so a catalog bug is identifiable without the graph.
        kind: String,
        /// What the spec declared: `(inputs, outputs)`.
        declared: (ChannelLayout, ChannelLayout),
        /// What the built unit reports: `(inputs, outputs)`.
        built: (ChannelLayout, ChannelLayout),
    },
    /// The topology contains a [`Edge::Feedback`] edge.
    ///
    /// # Why this is an error rather than a lowering
    ///
    /// `Net` has **no feedback edge**. Its `Source` enum is
    /// `Local | Global | Zero`; a cycle among those is
    /// `NetError::Cycle`, a recoverable error condition it reports from
    /// `Net::error`, and `set_source` even `assert!`s against the trivial
    /// self-connection. Feedback in fundsp is a *node* — `feedback()` wraps a
    /// sub-graph and owns the unit delay internally — so lowering an `Edge`
    /// into it would mean synthesising a wrapper node that is in the runtime and
    /// not in the value, which is precisely the shadow state this layer exists
    /// to delete.
    ///
    /// So PR 1 validates feedback edges (they legally break a cycle, and the
    /// folds treat them as holes) and refuses to compile them. Lowering belongs
    /// with a `Feedback` *node kind* in the value, where the delay is visible to
    /// every fold rather than hidden inside a `Net` vertex.
    FeedbackUnsupported {
        /// The sink port carrying the feedback edge.
        at: InPort,
    },
    /// The topology contains an unbroken cycle.
    ///
    /// Unreachable from a [`Valid`], and kept as a variant rather than a panic
    /// because `Valid` is a *runtime* check: a bug in it should surface here as
    /// an error, not as an abort inside a graph rebuild.
    Cyclic {
        /// The nodes the topological walk could not place.
        involving: Vec<NodeKey>,
    },
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownKind { node, kind } => {
                write!(f, "no catalog builds kind {kind:?} for node {}", node.0)
            }
            Self::WidthMismatch {
                node,
                kind,
                declared,
                built,
            } => write!(
                f,
                "node {} ({kind:?}) declares {}in/{}out but the built unit has {}in/{}out",
                node.0,
                declared.0.count(),
                declared.1.count(),
                built.0.count(),
                built.1.count(),
            ),
            Self::FeedbackUnsupported { at } => write!(
                f,
                "feedback edge into node {} port {}: Net has no feedback edge",
                at.node.0, at.port
            ),
            Self::Cyclic { involving } => {
                write!(f, "unbroken cycle involving {} nodes", involving.len())
            }
        }
    }
}

impl std::error::Error for CompileError {}

/// What [`compile`] produces: the runtime, plus the key→id map that named it.
///
/// The map is returned rather than dropped because `Net` mints its own ids from
/// a global counter and offers no way to ask "which node is this key?" — `ids()`
/// yields them in hash order, which is neither the value's order nor stable. A
/// caller that wants to address a compiled node (to read a meter, to swap a
/// unit) needs this, and re-deriving it later is impossible rather than merely
/// awkward.
///
/// It is deliberately **not** state a caller is expected to keep. A recompile
/// produces a fresh map; holding the old one across one is the stale-id bug
/// [`NodeKey`] exists to remove. Read it, use it, drop it with the `Net`.
pub struct Compiled {
    /// The graph, unwired to any backend. Take one with `Net::backend`.
    pub net: Net,
    /// Which `Net` node each [`NodeKey`] became.
    pub ids: BTreeMap<NodeKey, NodeId>,
}

/// Turn a validated topology into a [`Net`] rendering it, plus the key→id map.
///
/// Allocating and control-thread only: it builds every node, wires every edge
/// and never touches the audio thread. The returned `Net` has no backend — a
/// caller that means to render takes one with `Net::backend` and commits, the
/// same as any imperatively built graph.
///
/// # What it writes, and why exactly these calls
///
/// [`Net::add`] per node, [`Net::set_source`] per edge,
/// [`Net::set_output_source`] per output channel: the same three calls
/// `bevy_tutti::graph::wire::rebuild` makes, so a graph compiled from a value is
/// indistinguishable from one the adapter wired by hand. Nothing here calls
/// `connect`, `pipe_input` or `pipe_output` — those walk *every* port of a node,
/// which is how a later wiring call silently clobbers an earlier one (see
/// `PortSources::with`'s doc on exactly that failure).
///
/// # Global inputs
///
/// The `Net`'s input arity is the topology's own
/// [`Topology::inputs`](tutti_types::graph::Topology::inputs), so a
/// [`Source::Global`] edge resolves to the channel the value names. A master
/// graph declares [`ChannelLayout::EMPTY`] and has none.
pub fn compile(
    valid: &Valid,
    catalog: &dyn Catalog,
    sample_rate: SampleRate,
) -> Result<Compiled, CompileError> {
    let topology = valid.get();
    // Not for ordering — `Net` computes its own — but because a cycle that
    // survived `validate` must not reach `set_source`, where it becomes a
    // deferred `NetError` a caller has to remember to poll for.
    topology
        .topo_order()
        .map_err(|involving| CompileError::Cyclic { involving })?;

    if let Some((&at, _)) = topology
        .edges
        .iter()
        .find(|(_, e)| matches!(e, Edge::Feedback(_)))
    {
        return Err(CompileError::FeedbackUnsupported { at });
    }

    // `.max(1)`: a topology that declares no outputs still compiles, to a net
    // with one silent channel. `Net::new(_, 0)` is legal but renders nothing a
    // caller can read, and `Engine::process_segment` clamps the root width to at
    // least 1 anyway — so a zero here would be a shape the runtime silently
    // widens, which is the kind of disagreement this layer exists to remove.
    let mut net = Net::new(
        topology.inputs.count() as usize,
        topology.outputs.len().max(1),
    );
    // Before pushing anything: `Net::push` stamps each unit with the net's
    // current rate, so setting it afterwards would touch every vertex a second
    // time and mark them all changed for the next commit.
    net.set_sample_rate(sample_rate);

    let mut ids: BTreeMap<NodeKey, NodeId> = BTreeMap::new();
    for (&key, spec) in &topology.nodes {
        let unit = catalog
            .build(spec, sample_rate)
            .ok_or_else(|| CompileError::UnknownKind {
                node: key,
                kind: spec.kind.clone(),
            })?;
        check_width(key, spec, unit.as_ref())?;
        ids.insert(key, net.push(unit));
    }

    for (&at, edge) in &topology.edges {
        let Edge::Direct(source) = *edge else {
            // The feedback scan above returned already; this arm is unreachable
            // and stays a `continue` rather than an `unreachable!` so a future
            // edge kind cannot turn into a panic inside a graph rebuild.
            continue;
        };
        // `validate` rejects an edge whose sink is not in `nodes`, so the lookup
        // always succeeds from a `Valid`. Skipping rather than indexing for the
        // same reason `lower` returns `Zero` for a missing source: a bug in the
        // check must not become a panic inside a graph rebuild.
        let Some(&sink) = ids.get(&at.node) else {
            continue;
        };
        net.set_source(sink, at.port as usize, lower(source, &ids));
    }

    for (channel, source) in topology.outputs.iter().enumerate() {
        net.set_output_source(channel, lower(*source, &ids));
    }

    Ok(Compiled { net, ids })
}

/// Every port index a [`Valid`] can name is in range *of the spec*; this is the
/// separate question of whether the built unit agrees with the spec.
fn check_width(key: NodeKey, spec: &NodeSpec, unit: &dyn AudioUnit) -> Result<(), CompileError> {
    let built = (
        ChannelLayout::from_count(unit.inputs() as u16),
        ChannelLayout::from_count(unit.outputs() as u16),
    );
    let declared = (spec.inputs, spec.outputs);
    if built == declared {
        return Ok(());
    }
    Err(CompileError::WidthMismatch {
        node: key,
        kind: spec.kind.clone(),
        declared,
        built,
    })
}

/// A value [`Source`] as the runtime's own. Total: every arm of the value has an
/// arm here, which is what keeps the two enums from drifting silently.
fn lower(source: Source, ids: &BTreeMap<NodeKey, NodeId>) -> NetSource {
    match source {
        Source::Node(p) => match ids.get(&p.node) {
            Some(&id) => NetSource::Local(id, p.port as usize),
            // `validate` rejects an edge naming a node not in the map, so this
            // is unreachable from a `Valid`. Silence rather than a panic: a bug
            // in the check must not abort a graph rebuild.
            None => NetSource::Zero,
        },
        Source::Global(ch) => NetSource::Global(ch as usize),
        Source::Zero => NetSource::Zero,
    }
}
