//! Building the graph as a **value** from what the ECS declares.
//!
//! [`build`] reads the same three things [`rebuild`](super::wire::rebuild)
//! reads — the entities carrying [`AudioNode`], the [`PortSources`]
//! declarations, and the [`MasterSources`] resource — and produces a
//! [`Topology`]. Nothing here writes the engine.
//!
//! # Why this exists beside the per-port diff rather than instead of it
//!
//! The diff in [`wire`](super::wire) answers "what must I write" by reading
//! `Net::source` back port by port. That works, and it is why this layer keeps
//! no shadow state — but it means every *question* about the graph needs a live
//! runtime, and the one hazard `wire`'s own module docs admit ("an imperative
//! engine-side write this layer **cannot detect**") is a direct consequence: the
//! dirty gate watches ECS change ticks, so a write nothing in the ECS touched is
//! invisible.
//!
//! A value makes the question answerable without the runtime. [`LiveGraph`]
//! holds the last one built, so `want == live` is a whole-graph comparison a
//! test can write down, and [`tutti_types::latency::plan`] over it is the same
//! fold the engine's own PDC uses.
//!
//! # What the value can and cannot see
//!
//! **It sees exactly the entity-bound nodes.** Every route into the graph binds
//! its node to an entity through [`AudioNode`] — `spawn_audio_node`,
//! `insert_audio_node`, and the four sites that call `Net::add`/`push`
//! directly (soundfont promotion, plugin load, the audio-rate chain, the mod
//! source LFO) all end in an `AudioNode` insert, because that component's
//! presence is what despawn, MIDI unregistration and engine binding key on.
//!
//! **It does not see a node with no entity.** There is exactly one such class
//! today: the `PdcDelay` nodes `latency::compensate` splices into the `Net`
//! during [`Compensate`](super::GraphReconcileSystems::Compensate). Those are
//! derived, not declared — `clear_delays` re-derives them from scratch on every
//! compensation run — so their absence from the value is correct rather than a
//! gap: the value is what was *authored*, and compensation is a fold over it.

use std::collections::BTreeMap;

use bevy_ecs::prelude::*;

use tutti_core::dsp::AudioUnit as _;
use tutti_core::AudioNode;
use tutti_types::graph::{Edge, InPort, NodeKey, NodeSpec, OutPort, Source, Topology};
use tutti_types::ChannelLayout;

use super::wire::{MasterSources, PortSource, PortSources};
use super::AudioGraphRes;

/// The catalog id every entity-bound node carries in the shadow value.
///
/// The value's `kind` is what a [`Catalog`](tutti_core::topology::Catalog)
/// dispatches on when *building* a unit. Nothing in this adapter builds units
/// from a value — a unit arrives already boxed, from a host that owns it — so
/// there is no kind to dispatch on, and inventing one per node type here would
/// be a second, disagreeing name for something the host already knows.
///
/// One constant rather than an empty string so a value read in a debugger says
/// which layer produced it.
pub const ENTITY_NODE_KIND: &str = "bevy-tutti:entity";

/// The last [`Topology`] the wire phase built.
///
/// # Who owns it
///
/// Written by [`snapshot`](super::wire::rebuild)'s call to [`build`] in the
/// `Wire` phase and by nothing else. Read by tests, and by anything wanting to
/// ask a question about the graph without a runtime in hand:
/// [`tutti_types::latency::plan`] over it is the same fold `compensate_graph`
/// runs against the `Net`, and
/// [`Topology::validate`](tutti_types::graph::Topology::validate) reports every
/// structural fault at once.
///
/// It is **not** a cache the engine is derived from — the per-port diff is
/// still what writes `Net`. Holding it is what makes "did anything change"
/// answerable as one comparison rather than as a per-port read-back.
#[derive(Resource, Debug, Default)]
pub struct LiveGraph(Topology);

impl LiveGraph {
    /// The graph as of the last wire pass.
    pub fn topology(&self) -> &Topology {
        &self.0
    }

    /// Replace it. The `Wire` phase's to call; exposed for a host driving the
    /// phase itself.
    pub fn set(&mut self, topology: Topology) {
        self.0 = topology;
    }
}

/// `Entity` → [`NodeKey`]. The whole binding, and it is a bit-cast.
///
/// An `Entity` is already a stable, generational identity the adapter minted;
/// `NodeKey` is a stable identity chosen by the topology's author. Reusing the
/// bits rather than allocating a parallel numbering means there is no second
/// map to keep honest — the same reason [`PortSource::Node`] names an entity
/// rather than a `NodeId`.
pub fn key_of(entity: Entity) -> NodeKey {
    NodeKey(entity.to_bits())
}

/// [`key_of`]'s inverse, for reading a fault back out of a fold.
pub fn entity_of(key: NodeKey) -> Entity {
    Entity::from_bits(key.0)
}

/// Build the topology the ECS currently declares.
///
/// Widths, latency and tail come from the **live unit**, read through the
/// `Net` — the only place they exist today. A node's arity is not authored
/// anywhere in the ECS: `spawn_audio_node` takes a `U: AudioUnit` and the four
/// direct-`add` sites take an already-built unit, so the unit is the sole
/// author of its own shape.
///
/// # Why an unbound entity is skipped rather than defaulted
///
/// A declaration routinely names an entity whose node arrives a frame later
/// (`insert_audio_node` lands as a deferred command). Emitting a placeholder
/// spec for it would make the value claim a node exists that does not, and the
/// next frame's comparison would see a change that is really an arrival. Absent
/// is the honest answer, and it is the same one `rebuild`'s `continue` gives.
pub fn build(
    graph: &AudioGraphRes,
    nodes: &Query<(Entity, &AudioNode)>,
    sinks: &Query<(Entity, &PortSources)>,
    master: &MasterSources,
) -> Topology {
    let mut topology = Topology {
        inputs: ChannelLayout::from_count(graph.0.inputs() as u16),
        ..Default::default()
    };

    for (entity, node) in nodes.iter() {
        if !graph.0.contains(node.0) {
            continue;
        }
        topology
            .nodes
            .insert(key_of(entity), spec_of(graph, node.0));
    }

    for (sink_entity, declared) in sinks.iter() {
        let key = key_of(sink_entity);
        // The declaration's own length, clamped to the node's arity exactly as
        // `rebuild` clamps it: a `Vec` longer than the node has ports declares
        // ports that do not exist, and the engine loop never writes them.
        let Some(spec) = topology.nodes.get(&key) else {
            continue;
        };
        let arity = declared.0.len().min(spec.inputs.count() as usize);
        for port in 0..arity {
            let Some(edge) = edge_of(declared.0[port], sink_entity, nodes, &topology) else {
                continue;
            };
            topology.edges.insert(
                InPort {
                    node: key,
                    port: port as u16,
                },
                edge,
            );
        }
    }

    // Only the channels the resource names, and only those the root has — the
    // same two clamps `rebuild` applies, for the reasons its comments give: a
    // shorter declaration is *undeclared*, not silent, and a longer one has
    // already widened the root by the time the loop runs.
    let root_channels = graph.0.outputs();
    topology.outputs = (0..master.0.len().min(root_channels))
        .map(|channel| {
            // Unresolvable, not silent — but `Topology::outputs` is positional,
            // so the channel must keep its slot. `Zero` is what the engine holds
            // for a channel nothing has written, which is exactly what an
            // unresolvable declaration leaves behind.
            source_of(master.0[channel], None, nodes, &topology).unwrap_or(Source::Zero)
        })
        .collect();

    topology
}

/// Every way the value and the engine disagree about the graph, as sentences.
///
/// The shadow half of the value layer: [`rebuild`](super::wire::rebuild) calls
/// this after its per-port loop has written the engine, under a
/// `debug_assert_eq!` against the empty list. A disagreement is therefore a test
/// failure and never a dropout, and it names the port rather than saying only
/// that two graphs differ.
///
/// # What is compared, and what deliberately is not
///
/// **Compared:** every edge the value declares, against
/// [`Net::source`](tutti_core::dsp::Net::source); every output channel, against
/// `output_source`; and the latency plan, since PDC is the fold with the most to
/// lose from a wrong edge.
///
/// **Not compared:** a port the value says nothing about. The loop's own
/// contract is that an undeclared port belongs to whoever wired it — a `Vec`
/// shorter than the node's arity leaves the trailing ports alone, and
/// `MasterSources` past its length is undeclared rather than silent. Asserting
/// the engine holds `Zero` there would be asserting the opposite of what
/// `wire`'s docs promise.
///
/// **Not compared, second class:** the `PdcDelay` nodes compensation splices
/// in. They have no entity, so they are not in the value, and they *re-point*
/// edges the value declares — a compensated edge reads `Local(delay, 0)` where
/// the value says `Local(source, port)`. That is the compensation working, not
/// a disagreement, so an edge whose engine source is a PDC delay is skipped.
/// The latency comparison below is the one that would catch compensation going
/// wrong, and it runs on the pre-compensation plan for both sides.
pub fn disagreements(
    want: &Topology,
    graph: &AudioGraphRes,
    nodes: &Query<(Entity, &AudioNode)>,
) -> Vec<String> {
    use tutti_core::dsp::Source as NetSource;

    let mut faults = Vec::new();

    // `Entity` → live `NodeId`, so a value `Source::Node` can be turned into
    // the engine source it should have produced. Rebuilt per call rather than
    // held, for the reason `wire`'s docs give: a stored id goes stale on a
    // crossfade.
    let ids: BTreeMap<NodeKey, tutti_core::NodeId> = nodes
        .iter()
        .filter(|(_, n)| graph.0.contains(n.0))
        .map(|(e, n)| (key_of(e), n.0))
        .collect();

    let lower = |source: Source| -> Option<NetSource> {
        Some(match source {
            Source::Zero => NetSource::Zero,
            Source::Global(ch) => NetSource::Global(ch as usize),
            Source::Node(p) => NetSource::Local(*ids.get(&p.node)?, p.port as usize),
        })
    };

    for (at, edge) in &want.edges {
        let Edge::Direct(source) = *edge else {
            continue;
        };
        let (Some(&sink), Some(expected)) = (ids.get(&at.node), lower(source)) else {
            continue;
        };
        let live = graph.0.source(sink, at.port as usize);
        if live != expected && !is_pdc_delay(graph, live) {
            faults.push(format!(
                "node {:?} port {}: value says {expected:?}, engine holds {live:?}",
                entity_of(at.node),
                at.port
            ));
        }
    }

    for (channel, source) in want.outputs.iter().enumerate() {
        let Some(expected) = lower(*source) else {
            continue;
        };
        let live = graph.0.output_source(channel);
        if live != expected && !is_pdc_delay(graph, live) {
            faults.push(format!(
                "output {channel}: value says {expected:?}, engine holds {live:?}"
            ));
        }
    }

    // The fold, not just the edges — PDC is the question with the most to lose
    // from a wrong edge, and it reads a *path*, so a single misdirected source
    // moves a figure the edge-by-edge comparison above would already have
    // caught but a partial value would not.
    //
    // Skipped once the engine holds compensation delays, and the skip is not a
    // weakening: `latency::compensate` calls `clear_delays` before it plans, so
    // the plan it acts on is always over the *authored* graph — the one the
    // value describes. A compensated `Net` is the plan's output, and comparing
    // a value against an output it does not model would be asserting the two
    // disagree by construction.
    if !graph
        .0
        .ids()
        .any(|&id| graph.0.node(id).get_id() == tutti_core::PDC_DELAY_ID)
    {
        let want_plan = tutti_types::latency::plan(want);
        let live_plan = tutti_types::latency::plan(&graph.0);
        if want_plan.channels() != live_plan.channels() || want_plan.total() != live_plan.total() {
            faults.push(format!(
                "latency plan: value gives {:?} total {:?}, engine gives {:?} total {:?}",
                want_plan.channels(),
                want_plan.total(),
                live_plan.channels(),
                live_plan.total()
            ));
        }
    }

    faults
}

/// Whether an engine source names a compensation delay.
///
/// Identified by `AudioUnit::get_id`, which is how
/// `DelayInsertion::clear_delays` finds
/// them too — one marker, one definition of "this node is derived, not
/// authored".
fn is_pdc_delay(graph: &AudioGraphRes, source: tutti_core::dsp::Source) -> bool {
    let tutti_core::dsp::Source::Local(id, _) = source else {
        return false;
    };
    graph.0.contains(id) && graph.0.node(id).get_id() == tutti_core::PDC_DELAY_ID
}

/// The spec of one live node, read off the unit.
///
/// `latency` and `tail` go through the `Net`'s own `LatencyGraph` / `TailGraph`
/// impls rather than being re-derived: both `AudioUnit` methods take
/// `&mut self`, so a `&Net` cannot call them, and those impls are the one place
/// the clone-to-probe is already written down.
fn spec_of(graph: &AudioGraphRes, node: tutti_core::NodeId) -> NodeSpec {
    use tutti_types::latency::LatencyGraph as _;
    use tutti_types::tail::TailGraph as _;

    NodeSpec {
        kind: ENTITY_NODE_KIND.to_string(),
        inputs: ChannelLayout::from_count(graph.0.inputs_in(node) as u16),
        outputs: ChannelLayout::from_count(graph.0.outputs_in(node) as u16),
        latency: graph.0.latency(node),
        tail: graph.0.tail(node),
        params: BTreeMap::new(),
    }
}

/// One declared port as an [`Edge`], or `None` if it cannot resolve *yet*.
///
/// Mirrors `wire::resolve` arm for arm, including the two refusals: a self-loop
/// (which `Net::set_source` asserts on) and a source port past the node's
/// output count. Both are `None` there and must be `None` here, or the value
/// would claim an edge the engine will never hold.
fn edge_of(
    declared: PortSource,
    sink: Entity,
    nodes: &Query<(Entity, &AudioNode)>,
    topology: &Topology,
) -> Option<Edge> {
    source_of(declared, Some(sink), nodes, topology).map(Edge::Direct)
}

/// The [`Source`] a declaration names, or `None` if unresolvable.
///
/// `sink` is `Some` for a node port and `None` for a global output channel —
/// the master cannot feed itself, so only the former checks for a self-loop.
fn source_of(
    declared: PortSource,
    sink: Option<Entity>,
    nodes: &Query<(Entity, &AudioNode)>,
    topology: &Topology,
) -> Option<Source> {
    match declared {
        PortSource::Silence => Some(Source::Zero),
        PortSource::Input { port } => {
            (port < topology.inputs.count() as usize).then_some(Source::Global(port as u16))
        }
        PortSource::Node { entity, port } => {
            if sink == Some(entity) {
                return None;
            }
            // Present in `nodes` *and* in the value: an entity whose node the
            // engine no longer contains was skipped by `build`, and an edge to
            // it would name a key that is not in `Topology::nodes` — which is
            // `Invalid::UnknownNode`, not silence.
            let key = key_of(nodes.get(entity).ok()?.0);
            let spec = topology.nodes.get(&key)?;
            (port < spec.outputs.count() as usize).then_some(Source::Node(OutPort {
                node: key,
                port: port as u16,
            }))
        }
    }
}
