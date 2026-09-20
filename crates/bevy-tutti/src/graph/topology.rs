//! The graph as a **value**: what the ECS declares, and how it reaches the
//! engine.
//!
//! [`build`] reads the three things a declaration consists of — the entities
//! carrying [`AudioNode`], the [`PortSources`] on each sink, and the
//! [`MasterSources`] resource — and produces a [`Topology`]. [`apply`] writes
//! that value into the runtime. [`LiveGraph`] holds the last one applied, so
//! "did the graph change" is one comparison rather than a port-by-port read of
//! `Net`.
//!
//! # What the value owns, and what it does not
//!
//! **It owns edges and outputs.** Every `Net::set_source` and
//! `set_output_source` this adapter performs is driven by [`apply`] from a
//! value. Nothing reads a topology back out of the runtime to decide what to
//! write.
//!
//! **It does not own units.** A node is added by
//! [`spawn_audio_node`](super::SpawnAudioNode) / `insert_audio_node` and removed
//! by the `On<Remove, AudioNode>` observer, exactly as before. The value names
//! nodes by [`NodeKey`] and carries a spec describing each one's *shape*
//! (widths, latency, tail) — never a `Box<dyn AudioUnit>`, and never a catalog
//! id it could be rebuilt from.
//!
//! That split is forced by the runtime, and the two constraints behind it are
//! worth stating because they are what a later slice would have to answer:
//!
//! 1. **The backend handoff is one-time.** `engine::build_into` calls
//!    `Net::backend()` once — it `assert!`s `!has_backend()`, moves the vertex
//!    vector to the audio thread and hands the result to `Engine::new`, which
//!    parks it with no setter. A `Net` built from scratch by
//!    [`tutti_core::topology::compile`] has no backend and nowhere to land.
//! 2. **The frontend `Net` is live mutable state, not a derived artifact.**
//!    `plugin_host` mutates hosted-plugin units in place through `node_as_mut`
//!    (sound precisely because their input slots are `Arc`-shared across the
//!    frontend's clones), and `crossfade_audio_node` queues a `NodeEdit` into
//!    `Net::edit_queue` that `commit` drains. A from-scratch rebuild discards
//!    both. The sampler's butler-shared buffers and a decoded `SoundFontUnit`
//!    are in the same position: no `kind` string could reconstruct them.
//!
//! So [`compile`](tutti_core::topology::compile) stays what it is — the
//! from-scratch path, for tests and offline rendering, where there is no live
//! backend to preserve. This module is the incremental path over a running one.
//!
//! # PDC delays are runtime-only, deliberately
//!
//! `latency::compensate` splices [`PdcDelay`](tutti_core::PdcDelay) nodes into
//! the `Net` during [`Compensate`](super::GraphReconcileSystems::Compensate).
//! They have no entity, so they are not in the value — and minting a `NodeKey`
//! for them from their `NodeId` would be *actively wrong*, not merely awkward:
//! `NodeId::new` draws from a global counter, and `clear_delays` destroys and
//! re-mints every delay on each compensation run. Keyed that way the value would
//! differ from itself every frame compensation ran, so `want == live` would
//! never hold and every frame would become a full rebuild.
//!
//! They are also *derived*: `compensate` calls `clear_delays` before it plans,
//! so the plan is always computed over the authored graph — the one the value
//! describes. Compensation is a fold over the value, and its output does not
//! belong in its input. [`disagreements`] excludes them for that reason, by the
//! same `get_id` marker `clear_delays` uses, and skips the latency comparison
//! entirely while any are live rather than asserting the value against an
//! output it does not model.
//!
//! # Where the shape comes from
//!
//! Widths, latency and tail are read off the **live unit** through the `Net`,
//! because that is the only place they exist. A node's arity is not authored
//! anywhere in the ECS: `spawn_audio_node` takes a `U: AudioUnit`, and the four
//! sites that push a unit directly (soundfont promotion, plugin load, the
//! audio-rate chain, the mod-source LFO) take one already built. The unit is the
//! sole author of its own shape.
//!
//! Every one of those routes ends in an `AudioNode` insert — that component's
//! presence is what despawn, MIDI unregistration and engine binding key on — so
//! `AudioNode` is the one binding [`build`] needs, and there is no route into
//! the graph it cannot see.

use std::collections::BTreeMap;

use bevy_ecs::prelude::*;

use tutti_core::AudioNode;
use tutti_core::AudioUnit as _;
use tutti_types::graph::{Edge, InPort, NodeKey, NodeSpec, OutPort, Source, Topology};
use tutti_types::ChannelLayout;

use super::wire::{MasterSources, PortSource, PortSources};
use super::AudioGraphRes;

/// The catalog id every entity-bound node carries.
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
    // Only the audio-rate modulation reconciler authors shaping, and that
    // reconciler is the `modulation` feature's. Without it no shaper exists, so
    // the parameter would name a component nothing can spawn.
    #[cfg(feature = "modulation")] shaping: &Query<&crate::modulation::audio_rate::ShaperShaping>,
) -> Topology {
    let mut topology = Topology {
        inputs: ChannelLayout::from_count(graph.0.inputs() as u16),
        ..Default::default()
    };

    for (entity, node) in nodes.iter() {
        if !graph.0.contains(node.0) {
            continue;
        }
        // `mut` only under `modulation`: without that feature no shaper exists,
        // so nothing writes to the spec after it is built.
        #[cfg_attr(not(feature = "modulation"), allow(unused_mut))]
        let mut spec = spec_of(graph, node.0);
        // A shaper's identity is not observable from its unit: `ParamShaperNode`
        // bakes depth, polarity and curve into a LUT and exposes no accessor, so
        // two shapers built from different sliders present identically here.
        // The authoring reconciler records what it built on the entity, and this
        // lifts it into the value — which is what lets "did this route's shaping
        // move" be answered by comparing topologies rather than by a sidecar.
        #[cfg(feature = "modulation")]
        if let Ok(s) = shaping.get(entity) {
            put_shaping(&mut spec, s.0);
        }
        topology.nodes.insert(key_of(entity), spec);
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

#[cfg(feature = "modulation")]
/// Record a shaper's shaping in its [`NodeSpec`], losslessly.
///
/// Separate params rather than one opaque id, because the value is compared —
/// `Topology` derives `Eq` — and a hash would make two different shapings
/// collide into "unchanged", which is the exact bug class this slice removes.
///
/// **Three params for most curves, five for `Bezier`.** Depth, polarity and
/// curve are always written. `CurveType::Bezier(f32, f32)` carries a payload, so
/// a discriminant alone would be lossy the same way a hash is: its two control
/// points ride as `shaper.curve.a` / `shaper.curve.b`, which are absent for
/// every other curve. A spec's param count is therefore not fixed, and nothing
/// should key on it.
///
/// The names are namespaced so they cannot be confused with a param a node
/// genuinely exposes: nothing in the catalog builds a node from these, and
/// `bevy-tutti` builds no nodes from specs at all (see the module docs).
fn put_shaping(spec: &mut NodeSpec, shaping: tutti_nodes::ParamModShaping) {
    use tutti_types::graph::ParamValue;

    spec.params.insert(
        "shaper.depth".into(),
        ParamValue::Scalar(shaping.depth.get()),
    );
    spec.params.insert(
        "shaper.polarity".into(),
        ParamValue::Index(match shaping.polarity {
            tutti_mod::Polarity::Bipolar => 0,
            tutti_mod::Polarity::Unipolar => 1,
        }),
    );
    // Discriminant plus payload. `Bezier`'s two control points are what make a
    // bare discriminant lossy, so they are carried beside it rather than folded
    // into it.
    let (curve, bezier) = curve_key(shaping.curve);
    spec.params
        .insert("shaper.curve".into(), ParamValue::Index(curve));
    if let Some((a, b)) = bezier {
        spec.params
            .insert("shaper.curve.a".into(), ParamValue::Scalar(a));
        spec.params
            .insert("shaper.curve.b".into(), ParamValue::Scalar(b));
    }
}

#[cfg(feature = "modulation")]
/// A `CurveType`'s stable discriminant, plus its `Bezier` payload when it has
/// one.
///
/// Written as an exhaustive match rather than a cast so that adding a variant
/// upstream is a compile error here, instead of silently sharing a number with
/// an existing curve — which would make two different shapings compare equal.
fn curve_key(curve: tutti_mod::CurveType) -> (u32, Option<(f32, f32)>) {
    use tutti_mod::CurveType as C;
    match curve {
        C::Linear => (0, None),
        C::Exponential => (1, None),
        C::Logarithmic => (2, None),
        C::SCurve => (3, None),
        C::Stepped => (4, None),
        C::Bezier(a, b) => (5, Some((a, b))),
        C::Elastic => (6, None),
        C::Bounce => (7, None),
        C::Back => (8, None),
        C::Circular => (9, None),
        C::QuadIn => (10, None),
        C::QuadOut => (11, None),
        C::QuadInOut => (12, None),
        C::CubicIn => (13, None),
        C::CubicOut => (14, None),
        C::CubicInOut => (15, None),
        C::QuartIn => (16, None),
        C::QuartOut => (17, None),
        C::QuartInOut => (18, None),
        C::QuintIn => (19, None),
        C::QuintOut => (20, None),
        C::QuintInOut => (21, None),
    }
}

/// Bring the engine into line with the value, and say whether anything moved.
///
/// **The value is the truth for edges and outputs; this is the only place it
/// reaches the runtime.** Three calls — `Net::set_source`, `set_output_source`
/// — driven by what the value says rather than by re-reading the declaration
/// port by port. Nothing here calls `connect`, `pipe_input` or `pipe_output`:
/// those walk *every* port of a node, which is how a later wiring call silently
/// clobbers an earlier one.
///
/// # Why it still compares before writing
///
/// [`rebuild`](super::wire::rebuild) has already decided the graph *changed* —
/// one value comparison, not a per-port read. This second, per-port comparison
/// answers a different question: **which** ports changed. `Net` offers only
/// incremental edits, and writing a port re-invalidates the topological order,
/// so writing all of them because one moved would make every edit cost a full
/// reorder.
///
/// It is also what closes the hazard. An imperative engine-side write leaves the
/// declaration — and therefore the value — untouched, so `want == live` and the
/// old loop never ran. Now the value is compared against the *engine*, so the
/// port is found and rewritten. See
/// [`repair`](super::wire::rebuild)'s caller for the frame this takes.
///
/// # What it does not touch
///
/// A port the value says nothing about. `wire`'s contract is that an undeclared
/// port belongs to whoever wired it — a `PortSources` shorter than the node's
/// arity leaves the trailing ports alone, and a `MasterSources` shorter than the
/// root leaves the remaining channels alone. The value carries exactly the
/// declared ports (see [`build`]), so iterating it *is* that contract rather
/// than a clamp reimposed here.
pub fn apply(
    want: &Topology,
    graph: &mut AudioGraphRes,
    nodes: &Query<(Entity, &AudioNode)>,
) -> bool {
    let ids = live_ids(graph, nodes);
    let mut wrote = false;

    for (at, edge) in &want.edges {
        let Edge::Direct(source) = *edge else {
            // No `Net` edge kind carries feedback — `tutti_core::topology::compile`
            // refuses one for the same reason. Unreachable from a value this
            // adapter builds (`build` emits only `Direct`), and a `continue`
            // rather than an `unreachable!` so a future edge kind cannot become
            // a panic inside a graph rebuild.
            continue;
        };
        let (Some(&sink), Some(source)) = (ids.get(&at.node), lower(source, &ids)) else {
            continue;
        };
        if graph.0.source(sink, at.port as usize) != source {
            graph.0.set_source(sink, at.port as usize, source);
            wrote = true;
        }
    }

    for (channel, source) in want.outputs.iter().enumerate() {
        let Some(source) = lower(*source, &ids) else {
            continue;
        };
        if graph.0.output_source(channel) != source {
            graph.0.set_output_source(channel, source);
            wrote = true;
        }
    }

    wrote
}

/// `NodeKey` → the live `NodeId` for every entity-bound node the engine holds.
///
/// Rebuilt per call rather than held, and that is the same rule
/// [`PortSource::Node`] follows: a stored id goes stale the moment anything
/// replaces the node, so the id is re-derived from the entity every time and
/// this layer keeps no `Entity → NodeId` map between frames.
fn live_ids(
    graph: &AudioGraphRes,
    nodes: &Query<(Entity, &AudioNode)>,
) -> BTreeMap<NodeKey, tutti_core::NodeId> {
    nodes
        .iter()
        .filter(|(_, n)| graph.0.contains(n.0))
        .map(|(e, n)| (key_of(e), n.0))
        .collect()
}

/// A value [`Source`] as the runtime's own, or `None` if it names a node the
/// engine does not hold.
///
/// Total over the value's arms, which is what keeps the two enums from drifting
/// silently. `None` is "not yet", never "silent": collapsing the two would drive
/// a port to zero on the frame before its source appears and then never revisit
/// it, since the declaration would not have changed.
fn lower(
    source: Source,
    ids: &BTreeMap<NodeKey, tutti_core::NodeId>,
) -> Option<tutti_core::dsp::Source> {
    use tutti_core::dsp::Source as NetSource;
    Some(match source {
        Source::Zero => NetSource::Zero,
        Source::Global(ch) => NetSource::Global(ch as usize),
        Source::Node(p) => NetSource::Local(*ids.get(&p.node)?, p.port as usize),
    })
}

/// Every way the value and the engine disagree about the graph, as sentences.
///
/// [`rebuild`](super::wire::rebuild) calls this immediately after [`apply`],
/// under a `debug_assert_eq!` against the empty list, so a divergence is a test
/// failure and never a dropout — and it names the port rather than saying only
/// that two graphs differ.
///
/// It is not redundant with `apply`'s own per-port comparison. `apply` decides
/// which ports to write; this asks whether
/// the engine now agrees — including on the fold, which no single port write
/// can be checked against.
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
    let mut faults = Vec::new();
    // The same map and the same lowering `apply` writes through — shared rather
    // than re-derived, so a check that passed could not be checking a different
    // translation from the one that ran.
    let ids = live_ids(graph, nodes);

    for (at, edge) in &want.edges {
        let Edge::Direct(source) = *edge else {
            continue;
        };
        let (Some(&sink), Some(expected)) = (ids.get(&at.node), lower(source, &ids)) else {
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
        let Some(expected) = lower(*source, &ids) else {
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
///
/// # Why two of the refusals warn and the rest do not
///
/// `None` means "not resolvable", and that covers two different futures. An
/// entity whose node has not spawned yet resolves *next frame*, and warning
/// about it would fire once per frame for a case that is both normal and
/// self-correcting. A self-loop and a port past the source's output count never
/// resolve — no later frame changes either — so silence there would leave a port
/// unwired forever with nothing said. Those two warn.
///
/// The self-loop refusal is not merely tidy: `Net::set_source` `assert!`s on it,
/// so a value carrying one would turn a caller's mistake into a panic inside a
/// graph rebuild.
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
                bevy_log::warn!(
                    "PortSources on {entity:?} names itself as a source; skipping (a node \
                     cannot feed its own input)"
                );
                return None;
            }
            // Present in `nodes` *and* in the value: an entity whose node the
            // engine no longer contains was skipped by `build`, and an edge to
            // it would name a key that is not in `Topology::nodes` — which is
            // `Invalid::UnknownNode`, not silence.
            let key = key_of(nodes.get(entity).ok()?.0);
            let spec = topology.nodes.get(&key)?;
            let outputs = spec.outputs.count() as usize;
            if outputs <= port {
                bevy_log::warn!(
                    "declared source {entity:?} port {port}, but its node has only \
                     {outputs} output(s); that port stays unwired. A mono node feeding \
                     both master channels is `MasterSources::mono_from`."
                );
                return None;
            }
            Some(Source::Node(OutPort {
                node: key,
                port: port as u16,
            }))
        }
    }
}
