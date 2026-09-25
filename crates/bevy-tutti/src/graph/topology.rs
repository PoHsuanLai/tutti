//! The graph as a **value**: what the ECS declares, and how it reaches the
//! engine.
//!
//! [`build`] reads the three things a declaration consists of — the entities
//! carrying [`AudioNode`], the [`PortSources`] on each sink, and the
//! [`MasterSources`] resource — and produces a [`Topology`]. [`apply`] writes
//! that value into the runtime. [`LiveGraph`] holds the last one applied, so
//! "did the graph change" is one comparison rather than a port-by-port read of
//! the graph.
//!
//! # What the value owns, and what it does not
//!
//! **It owns edges and outputs.** Every `AudioGraphRes::set_source` and
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
//! That split is forced by what a unit is: live state a host built, not a
//! derived artifact. A hosted plugin's process, the sampler's butler-shared
//! buffers and a decoded `SoundFontUnit` arrive boxed, and no `kind` string
//! could reconstruct them; the editor owns each one from its insert to its
//! removal, and a crossfade replaces one in place under its key.
//!
//! # Nothing derived is in the graph
//!
//! PDC is the compiler's: each commit's plan delays the early paths, and no
//! node is spliced in to do it (before design doc 013's PR 13, `Net` had
//! compensation delay nodes with no entity, which the value had to be kept
//! apart from). So every node in the graph is one a host inserted, and every
//! edge one the value or a host wrote.
//!
//! # Where the shape comes from
//!
//! Widths, latency and tail are read off the editor's shapes — probed from
//! the **live unit** when it was inserted — because that is the only place
//! they exist. A node's arity is not authored
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
use tutti_types::graph::{Edge, InPort, NodeKey, NodeSpec, OutPort, Source, Topology};
use tutti_types::ChannelLayout;

use super::wire::{MasterSources, PortSource, PortSources};
use super::{AudioGraphRes, GraphSource};

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

/// The last [`Topology`] the wire rebuild applied to the engine.
///
/// # Who owns it
///
/// Written by [`rebuild`](super::wire::rebuild) — the system `GraphWirePlugin`
/// schedules between `Spawn` and `Compensate` — after [`apply`] has brought the
/// engine into line with it, and by nothing else. Read by tests, and by
/// anything wanting to ask a question about the graph without a runtime in
/// hand:
/// [`tutti_types::latency::plan`] over it is the fold the graph's plans
/// compensate by, for a graph whose every port the ECS declares (a port a host
/// wired by hand is not in it; see [`disagreements`]), and
/// [`Topology::validate`](tutti_types::graph::Topology::validate) reports every
/// structural fault at once.
///
/// It is **not** what writes the graph, and not a cache the engine is derived
/// from. Each rebuild builds a fresh value from the declarations, and
/// [`apply`] writes that value — every declared port the engine disagrees
/// with. This is the value applied *last time*; holding it is what makes "did
/// anything change" answerable as one comparison (`want == live`) rather than
/// as a per-port read-back.
#[derive(Resource, Debug, Default)]
pub struct LiveGraph(Topology);

impl LiveGraph {
    /// The graph as of the last wire pass.
    pub fn topology(&self) -> &Topology {
        &self.0
    }

    /// Replace it. [`rebuild`](super::wire::rebuild)'s to call; exposed for a
    /// host driving the rebuild itself.
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
/// Widths, latency and tail come from the **live unit**, as the editor probed
/// it at insert — the only place they exist today. A node's arity is not authored
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
        inputs: ChannelLayout::from_count(graph.inputs() as u16),
        ..Default::default()
    };

    for (entity, node) in nodes.iter() {
        if !graph.contains(*node) {
            continue;
        }
        // `mut` only under `modulation`: without that feature no shaper exists,
        // so nothing writes to the spec after it is built.
        #[cfg_attr(not(feature = "modulation"), allow(unused_mut))]
        let mut spec = spec_of(graph, *node);
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

    // A written `MasterSources` owns **every** root channel, not just the ones
    // its `Vec` reaches: a channel past its length is declared silent. Empty
    // declares nothing, and the value then carries no outputs at all.
    //
    // It used to stop at the declaration's length, reading a shorter `Vec` as
    // "undeclared". That made a shrink impossible to express: the channel the
    // host dropped kept its last source, since nothing declared it any more
    // and so nothing wrote it, and the value — one channel short of the root —
    // folded to a different latency plan from the engine it had just been
    // applied to, which is what tripped `rebuild`'s consistency check. A value
    // as wide as the root is also what makes `LiveGraph` answer the questions
    // its docs promise: `latency::plan` over it is the graph's fold only when
    // every output the graph has is in it.
    //
    // As wide as the root, not as the declaration: a longer declaration has
    // already widened the root by the time this runs (`rebuild`), and a
    // shorter one leaves it at its width — the root is the device's, and a
    // shrink is not a narrowing.
    topology.outputs = if master.0.is_empty() {
        Vec::new()
    } else {
        (0..graph.outputs())
            .map(|channel| match master.0.get(channel) {
                // Unresolvable, not silent — but `Topology::outputs` is
                // positional, so the channel must keep its slot. `Zero` is what
                // the engine holds for a channel nothing has written, which is
                // exactly what an unresolvable declaration leaves behind.
                Some(&declared) => {
                    source_of(declared, None, nodes, &topology).unwrap_or(Source::Zero)
                }
                None => Source::Zero,
            })
            .collect()
    };

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
/// reaches the runtime.** Two calls — `AudioGraphRes::set_source`, `set_output_source`
/// — driven by what the value says rather than by re-reading the declaration
/// port by port. Nothing here calls `connect`, `pipe_input` or `pipe_output`:
/// those walk *every* port of a node, which is how a later wiring call silently
/// clobbers an earlier one.
///
/// # Why it still compares before writing
///
/// [`rebuild`](super::wire::rebuild) has already decided the graph *changed* —
/// one value comparison, not a per-port read. This second, per-port comparison
/// answers a different question: **which** ports changed, so that a rebuild
/// that moves nothing leaves the graph clean (no `GraphDirty`, no commit, no
/// compile).
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
/// arity leaves the trailing ports alone, and an *empty* `MasterSources` leaves
/// every output channel alone. The value carries exactly the declared ports
/// (see [`build`]), so iterating it *is* that contract rather than a clamp
/// reimposed here. (A non-empty `MasterSources` declares the whole root, a
/// channel past its length as silence, so every one of its channels is in the
/// value and written here.)
pub fn apply(
    want: &Topology,
    graph: &mut AudioGraphRes,
    nodes: &Query<(Entity, &AudioNode)>,
) -> bool {
    let ids = live_ids(graph, nodes);
    let mut wrote = false;

    for (at, edge) in &want.edges {
        let Edge::Direct(source) = *edge else {
            // The declaration has no feedback edge: `PortSources` names a
            // direct source. Unreachable from a value this adapter builds
            // (`build` emits only `Direct`), and a `continue`
            // rather than an `unreachable!` so a future edge kind cannot become
            // a panic inside a graph rebuild.
            continue;
        };
        let (Some(&sink), Some(source)) = (ids.get(&at.node), lower(source, &ids)) else {
            continue;
        };
        if graph.source(sink, at.port as usize) != source {
            graph.set_source(sink, at.port as usize, source);
            wrote = true;
        }
    }

    for (channel, source) in want.outputs.iter().enumerate() {
        let Some(source) = lower(*source, &ids) else {
            continue;
        };
        if graph.output_source(channel) != source {
            graph.set_output_source(channel, source);
            wrote = true;
        }
    }

    wrote
}

/// `NodeKey` → the live [`AudioNode`] for every entity-bound node the engine
/// holds.
///
/// Rebuilt per call rather than held, and that is the same rule
/// [`PortSource::Node`] follows: a stored handle goes stale the moment anything
/// replaces the node, so it is re-derived from the entity every time and this
/// layer keeps no `Entity → AudioNode` map between frames.
fn live_ids(
    graph: &AudioGraphRes,
    nodes: &Query<(Entity, &AudioNode)>,
) -> BTreeMap<NodeKey, AudioNode> {
    nodes
        .iter()
        .filter(|(_, n)| graph.contains(**n))
        .map(|(e, n)| (key_of(e), *n))
        .collect()
}

/// A value [`Source`] as the graph's own [`GraphSource`], or `None` if it names
/// a node the engine does not hold.
///
/// Total over the value's arms, which is what keeps the two enums from drifting
/// silently. `None` is "not yet", never "silent": collapsing the two would drive
/// a port to zero on the frame before its source appears and then never revisit
/// it, since the declaration would not have changed.
fn lower(source: Source, ids: &BTreeMap<NodeKey, AudioNode>) -> Option<GraphSource> {
    Some(match source {
        Source::Zero => GraphSource::Silence,
        Source::Global(ch) => GraphSource::Input(ch as usize),
        Source::Node(p) => GraphSource::Node(*ids.get(&p.node)?, p.port as usize),
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
/// which ports to write; this asks whether the engine now agrees — read back
/// through the graph's own queries, so a lowering that wrote one thing and
/// reads back another is caught where it happens.
///
/// # What is compared, and what deliberately is not
///
/// **Compared:** every edge the value declares, against
/// [`AudioGraphRes::source`], and every output channel it declares, against
/// [`output_source`](AudioGraphRes::output_source).
///
/// **Not compared:** a port the value says nothing about. The loop's own
/// contract is that an undeclared port belongs to whoever wired it — a `Vec`
/// shorter than the node's arity leaves the trailing ports alone, and an empty
/// `MasterSources` declares no output channel. Asserting
/// the engine holds `Zero` there would be asserting the opposite of what
/// `wire`'s docs promise.
///
/// **Not compared: the latency plan.** It used to be, over the whole graph,
/// and that was wrong for exactly the graphs the contract above allows: a
/// host that wires the master itself (an empty `MasterSources`), or a port a
/// short `PortSources` leaves to it, from a latent node, gives a graph whose
/// plan the value — which holds only the declared ports — cannot fold to, and
/// the check panicked a debug build over a graph that was right. Restricted to
/// the declared ports, the fold is a function of the value's node specs (read
/// off the same shapes the graph holds) and of the declared edges and
/// outputs, so it agrees exactly when the two comparisons above find
/// nothing: it could not fail on its own. What the plan compensates is pinned
/// against the plan the commit sends (`latency`'s
/// `publishes_the_compensation_its_commit_sends`).
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
        let live = graph.source(sink, at.port as usize);
        if live != expected {
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
        let live = graph.output_source(channel);
        if live != expected {
            faults.push(format!(
                "output {channel}: value says {expected:?}, engine holds {live:?}"
            ));
        }
    }

    faults
}

/// The spec of one live node, read off the unit through the graph's shape
/// queries.
fn spec_of(graph: &AudioGraphRes, node: AudioNode) -> NodeSpec {
    NodeSpec {
        kind: ENTITY_NODE_KIND.to_string(),
        inputs: ChannelLayout::from_count(graph.node_inputs(node) as u16),
        outputs: ChannelLayout::from_count(graph.node_outputs(node) as u16),
        latency: graph.node_latency(node),
        tail: graph.node_tail(node),
        params: BTreeMap::new(),
    }
}

/// One declared port as an [`Edge`], or `None` if it cannot resolve *yet*.
///
/// Mirrors `wire::resolve` arm for arm, including the two refusals: a self-loop
/// (which `AudioGraphRes::set_source` asserts on) and a source port past the node's
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
/// The self-loop refusal is not merely tidy: `AudioGraphRes::set_source` `assert!`s on it,
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
