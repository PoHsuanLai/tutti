//! The graph value this crate compiles: a [`Topology`] plus event edges and
//! unit generations.
//!
//! # Why a wrapper and not an extension of `tutti_types::graph`
//!
//! Giving `InPort`/`OutPort` a kind in `tutti-types` would break every place
//! that builds them by literal (`InPort { node, port }`). Event edges also
//! have a different *shape* from audio edges — fan-in is allowed on event
//! ports, so an event sink holds a list of sources where an audio sink holds
//! exactly one — so the two maps cannot share `Topology::edges`' type anyway.
//!
//! [`GraphSpec`] therefore **embeds** the `Topology` unchanged (audio stays one
//! source per port, keyed on the sink — fan-in still unrepresentable there) and
//! adds what is new beside it, with distinct port types ([`EventIn`],
//! [`EventOut`]) so an audio port cannot be used as an event port by accident.

use std::collections::{BTreeMap, BTreeSet};

use tutti_types::graph::{Invalid, Valid};
use tutti_types::latency::{Feed, LatencyGraph};
use tutti_types::{NodeKey, Samples, Topology};

use crate::node::Resolution;
use crate::param::MAX_PARAM_SOURCES;
use crate::param::{ParamFrom, ParamIn, ParamMod, ParamRange, ParamShaping, ParamSource};

/// An event **input** port of a node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EventIn {
    /// The node.
    pub node: NodeKey,
    /// Which of its event input ports.
    pub port: u16,
}

/// An event **output** port of a node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EventOut {
    /// The node.
    pub node: NodeKey,
    /// Which of its event output ports.
    pub port: u16,
}

/// One source of an event input port.
///
/// Port kinds match by type: an event edge runs from an [`EventOut`] to an
/// [`EventIn`], and an audio port is a different type, so an audio↔event
/// edge is not a graph error for `compile` to catch — it does not
/// type-check:
///
/// ```compile_fail
/// use tutti_graph::{EventEdge, EventIn, GraphSpec};
/// use tutti_types::graph::OutPort;
/// use tutti_types::NodeKey;
/// let mut g = GraphSpec::default();
/// let audio = OutPort { node: NodeKey(1), port: 0 };
/// g.connect_events(EventIn { node: NodeKey(2), port: 0 }, EventEdge::Direct(audio));
/// ```
///
/// ```
/// use tutti_graph::{EventEdge, EventIn, EventOut, GraphSpec};
/// use tutti_types::NodeKey;
/// let mut g = GraphSpec::default();
/// let events = EventOut { node: NodeKey(1), port: 0 };
/// g.connect_events(EventIn { node: NodeKey(2), port: 0 }, EventEdge::Direct(events));
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EventEdge {
    /// This block's events from the named port.
    Direct(EventOut),
    /// The named port's events delayed by exactly `delay` frames — a
    /// declared cycle, exactly as `tutti_types::graph::FeedbackFrom` is for
    /// audio, with the same rule: the delay belongs to the edge, and must be
    /// at least the interpreter's maximum block (see `FeedbackFrom`).
    Feedback {
        /// The source port.
        from: EventOut,
        /// How far back, in frames.
        delay: Samples,
    },
}

impl EventEdge {
    /// The port the events leave from.
    pub const fn from(self) -> EventOut {
        match self {
            Self::Direct(p) | Self::Feedback { from: p, .. } => p,
        }
    }

    /// A feedback edge from `from`, delayed by `delay` frames.
    pub const fn feedback(from: EventOut, delay: Samples) -> Self {
        Self::Feedback { from, delay }
    }
}

/// The graph as a value: a [`Topology`] (nodes, audio edges, global inputs
/// and outputs) plus event edges, param modulation and unit generations.
///
/// Plain data — `Clone`, `Eq` and `Hash` — built and edited on the control
/// thread, usually through [`Editor::spec_mut`](crate::Editor::spec_mut).
/// [`validate`](Self::validate) checks what can be checked without the
/// nodes' shapes and returns a [`ValidGraph`], which is what
/// [`compile`](crate::compile) takes.
///
/// Audio edges live in the embedded `Topology`, one source per input port,
/// so audio fan-in cannot be written (summing is a node's job). Event ports
/// ([`EventIn`], [`EventOut`]) are separate types, so an audio port cannot be
/// used as an event port by accident, and an event input may have several
/// sources, merged by offset.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct GraphSpec {
    /// Nodes, audio edges, global outputs and global input width — unchanged.
    pub topology: Topology,
    /// Event edges, keyed on the sink port: every source of the port (an
    /// event input may have several). An empty `Vec` is
    /// the same as an absent key.
    ///
    /// The `Vec`'s order does **not** matter: fan-in merges by offset, and
    /// events at equal offsets go in **source order**, the source's
    /// `(NodeKey, port)` ([`EventOut`]'s `Ord`), whatever order they are
    /// listed in. [`connect_events`](Self::connect_events) keeps the list in
    /// that order, so two specs with the same wiring built in different
    /// orders compare equal.
    pub events: BTreeMap<EventIn, Vec<EventEdge>>,
    /// Unit generation per node. Bumping it is how a value says "same key, new
    /// unit" — a rebind or a replacement. Absent
    /// means generation 0.
    pub generations: BTreeMap<NodeKey, u32>,
    /// Event edges that require their sink to honour offsets at least this
    /// finely, keyed `(sink, source)` — see
    /// [`require_resolution`](Self::require_resolution).
    pub required_resolution: BTreeMap<(EventIn, EventOut), Resolution>,
    /// Modulated params, keyed on the param port (see
    /// [`connect_param`](Self::connect_param)): each one's range and
    /// sources, in source order. A port with no entry, or an entry with no
    /// sources, reads its base.
    pub params: BTreeMap<ParamIn, ParamMod>,
}

impl GraphSpec {
    /// A spec over `topology` with no event edges and every node at
    /// generation 0.
    pub fn new(topology: Topology) -> Self {
        Self {
            topology,
            ..Self::default()
        }
    }

    /// The generation of `node`: 0 unless one was set.
    pub fn generation(&self, node: NodeKey) -> u32 {
        self.generations.get(&node).copied().unwrap_or(0)
    }

    /// Adds one event source to `at`, in source order (see
    /// [`events`](Self::events)). A source already listed at `at` is added
    /// again, and [`validate`](Self::validate) refuses the duplicate.
    pub fn connect_events(&mut self, at: EventIn, edge: EventEdge) {
        let sources = self.events.entry(at).or_default();
        let i = sources.partition_point(|e| e.from() <= edge.from());
        sources.insert(i, edge);
    }

    /// Marks the event edge `from → at` as requiring its sink to honour event
    /// offsets at least as finely as `resolution`.
    ///
    /// **The rule:** `compile` refuses a marked edge whose sink declares a
    /// coarser [`Shape::event_resolution`](crate::Shape::event_resolution)
    /// ([`CompileError::ResolutionTooCoarse`](crate::CompileError::ResolutionTooCoarse)).
    /// Unmarked edges are never refused — a note into a block-rate node is
    /// late by at most a block, which is a choice a patch may make; sample-
    /// accurate automation into one silently is not. So the edge carrying
    /// [`ParamRamp`](crate::ParamRamp) automation is the one to mark
    /// [`Resolution::Sample`]. A marked edge must exist ([`validate`](Self::validate)
    /// checks it): remove edges with [`disconnect_events`](Self::disconnect_events),
    /// which drops the mark too (editing `events` by hand does not), or drop
    /// a mark alone with [`unrequire_resolution`](Self::unrequire_resolution).
    pub fn require_resolution(&mut self, at: EventIn, from: EventOut, resolution: Resolution) {
        self.required_resolution.insert((at, from), resolution);
    }

    /// Drops the resolution mark on `from → at`, if any.
    pub fn unrequire_resolution(&mut self, at: EventIn, from: EventOut) {
        self.required_resolution.remove(&(at, from));
    }

    /// Removes the event edge `from → at` (direct or feedback), and its
    /// resolution mark with it — so a disconnect can never leave a stale mark
    /// that fails every later [`validate`](Self::validate). Returns whether an
    /// edge was removed.
    pub fn disconnect_events(&mut self, at: EventIn, from: EventOut) -> bool {
        let Some(sources) = self.events.get_mut(&at) else {
            return false;
        };
        let before = sources.len();
        sources.retain(|e| e.from() != from);
        let removed = sources.len() != before;
        if sources.is_empty() {
            self.events.remove(&at);
        }
        self.required_resolution.remove(&(at, from));
        removed
    }

    /// Drives param port `at` from `from` as well, shaped by `shaping`: one
    /// more offset in its sum, in source order ([`ParamFrom`]'s `Ord`), so
    /// two specs with the same wiring compare equal whatever order it was
    /// made in. A source already listed at `at` has its shaping replaced.
    ///
    /// The port must be one its node declares
    /// ([`Shape::params`](crate::Shape::params)), or `compile` refuses it
    /// ([`CompileError::UnknownParam`](crate::CompileError::UnknownParam)).
    /// Until a range is set ([`set_param_range`](Self::set_param_range)) the
    /// sum is not clamped.
    ///
    /// # How a modulated param is computed
    ///
    /// Each modulated param becomes one fused step of its node's op,
    /// computing, per frame:
    ///
    /// ```text
    ///   value[i] = clamp(base_ramp[i] + Σ shape_j(source_j[i]), range)
    /// ```
    ///
    /// - **The base** is the node's own control — a `Param<U>` it hands out in
    ///   its `Controls` — read once per block through
    ///   [`Node::param_base`](crate::Node::param_base) and ramped linearly
    ///   across the block, landing exactly on the new value at its last frame,
    ///   so a fader move under modulation does not zipper.
    /// - **The offsets** come from audio outputs ([`ParamFrom::Audio`], one
    ///   value per frame) or from [`ParamRamp`](crate::ParamRamp) events
    ///   ([`ParamFrom::Events`], a sample-accurate ramp per source), each
    ///   through its own [`ParamShaping`]: the identity, or a
    ///   [`ShapeLut`](crate::ShapeLut) (the depth · polarity · curve table
    ///   `tutti_mod::shape` bakes). They are summed in source order and clamped
    ///   once to the port's [`ParamRange`].
    /// - **An unconnected param resolves to its base, never to 0.** A port with
    ///   no source this block reads
    ///   [`ParamInput::Base`](crate::ParamInput::Base), and the node uses its
    ///   own control, exactly as if nothing could modulate it: the fast path
    ///   costs one branch, and nothing is copied. A port can be connected and
    ///   disconnected by any commit.
    /// - **Connecting or disconnecting is declicked.** When a port's sources
    ///   change (a new source, one gone, a new shaping), its output crossfades
    ///   from where it was — the last value it delivered, or the base — to the
    ///   new value over [`PARAM_DECLICK`](crate::PARAM_DECLICK) frames. Nothing
    ///   else is smoothed: a step in a modulator lands on its frame (the
    ///   sample-accuracy contract). A unit's **first** block is not a change: a
    ///   unit placed by a commit (an insert, a hard replace, a fork, a
    ///   re-prepare's resume) starts at its modulated value, as its audio
    ///   starts at its first frame.
    /// - **A source's state is the source's, not its slot's.** An event
    ///   source's ramp (the value it holds, and a ramp under way) is kept by
    ///   [`ParamFrom`], so adding or removing *another* source, or reshaping
    ///   this one, does not reset it. A PDC delay that appears on an audio
    ///   source (the node's arrival moved) starts full of the source's last
    ///   value, and one that grows is padded with it, so the port holds rather
    ///   than dropping to 0 for the delay's length.
    /// - **A crossfade's base.** A [`replace`](crate::Editor::replace) with a
    ///   fade keeps the key's param state, so both units hear one modulation;
    ///   the base is the incoming unit's control, ramped over one block like
    ///   any control move, not over the fade. Ramping it over the fade would
    ///   hold the incoming unit off its own control for the fade's length, and
    ///   the audio crossfade already covers the swap.
    /// - **PDC.** A param source is aligned to the node's arrival like any of
    ///   its inputs: a source that arrives earlier is delayed
    ///   ([`DelayKey::ParamAudio`](crate::DelayKey::ParamAudio),
    ///   [`DelayKey::ParamEvent`](crate::DelayKey::ParamEvent)), and a later
    ///   one raises the node's arrival.
    pub fn connect_param(&mut self, at: ParamIn, from: ParamFrom, shaping: ParamShaping) {
        let m = self.params.entry(at).or_default();
        match m.sources.binary_search_by(|s| s.from.cmp(&from)) {
            Ok(i) => m.sources[i].shaping = shaping,
            Err(i) => m.sources.insert(i, ParamSource { from, shaping }),
        }
    }

    /// Stops driving param port `at` from `from`. Returns whether it was a
    /// source. The port keeps its range; with no source left it reads its
    /// base.
    pub fn disconnect_param(&mut self, at: ParamIn, from: ParamFrom) -> bool {
        let Some(m) = self.params.get_mut(&at) else {
            return false;
        };
        let before = m.sources.len();
        m.sources.retain(|s| s.from != from);
        before != m.sources.len()
    }

    /// Clamps param port `at`'s modulated value to `range` — the param's own
    /// range, so no stack of modulators drives it past what the node
    /// accepts. A range change recompiles; it does not restart the port's
    /// sources. A NaN bound is refused by [`validate`](Self::validate)
    /// ([`GraphInvalid::BadParamRange`]).
    pub fn set_param_range(&mut self, at: ParamIn, range: ParamRange) {
        self.params.entry(at).or_default().range = range;
    }

    /// Checks everything that does not need the nodes' [`Shape`](crate::Shape)s.
    ///
    /// The `Topology` half runs `Topology::validate` unchanged; the event
    /// half checks that every event edge names nodes that exist and that no
    /// source is listed twice at one sink. Event port *ranges* need the
    /// shapes and are checked by `compile`, as is a cycle that runs through
    /// an event edge.
    ///
    /// # Errors
    ///
    /// Every fatal fault found, as a list of [`GraphInvalid`] (never just
    /// the first). An unconnected input is not a fault: it reads silence.
    pub fn validate(&self) -> Result<ValidGraph, Vec<GraphInvalid>> {
        let mut errs: Vec<GraphInvalid> = Vec::new();
        let valid = match self.topology.validate() {
            Ok(v) => Some(v),
            Err(e) => {
                errs.extend(
                    e.into_iter()
                        .filter(Invalid::is_fatal)
                        .map(GraphInvalid::Topology),
                );
                None
            }
        };

        let nodes = &self.topology.nodes;
        for (&at, sources) in &self.events {
            if !nodes.contains_key(&at.node) {
                errs.push(GraphInvalid::UnknownEventNode {
                    at,
                    missing: at.node,
                });
            }
            let mut seen = BTreeSet::new();
            for edge in sources {
                let from = edge.from();
                if !nodes.contains_key(&from.node) {
                    errs.push(GraphInvalid::UnknownEventNode {
                        at,
                        missing: from.node,
                    });
                }
                if !seen.insert(from) {
                    errs.push(GraphInvalid::DuplicateEventSource { at, from });
                }
            }
        }
        for &key in self.generations.keys() {
            if !nodes.contains_key(&key) {
                errs.push(GraphInvalid::UnknownGeneration { node: key });
            }
        }
        for (&at, m) in &self.params {
            for node in std::iter::once(at.node).chain(m.sources.iter().map(|s| s.from.node())) {
                if !nodes.contains_key(&node) {
                    errs.push(GraphInvalid::UnknownParamNode { at, missing: node });
                }
            }
            if m.sources.windows(2).any(|w| w[0].from >= w[1].from) {
                errs.push(GraphInvalid::UnsortedParamSources { at });
            }
            if m.sources.len() > MAX_PARAM_SOURCES {
                errs.push(GraphInvalid::TooManyParamSources {
                    at,
                    count: m.sources.len(),
                });
            }
            if m.range.min.is_nan() || m.range.max.is_nan() {
                errs.push(GraphInvalid::BadParamRange { at, range: m.range });
            }
        }
        for &(at, from) in self.required_resolution.keys() {
            let wired = self
                .events
                .get(&at)
                .is_some_and(|v| v.iter().any(|e| e.from() == from));
            if !wired {
                errs.push(GraphInvalid::RequirementWithoutEdge { at, from });
            }
        }

        match valid {
            Some(valid) if errs.is_empty() => Ok(ValidGraph {
                valid,
                events: self.events.clone(),
                generations: self.generations.clone(),
                required_resolution: self.required_resolution.clone(),
                params: self
                    .params
                    .iter()
                    .filter(|(_, m)| !m.sources.is_empty())
                    .map(|(&at, m)| (at, m.clone()))
                    .collect(),
            }),
            _ => Err(errs),
        }
    }
}

/// Why a [`GraphSpec`] is malformed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphInvalid {
    /// The embedded `Topology` is invalid. Only fatal faults are carried; an
    /// unconnected audio input reads silence and is not one.
    Topology(Invalid),
    /// An event edge names a node that is not in the topology.
    UnknownEventNode {
        /// The sink port whose edge names it.
        at: EventIn,
        /// The key that is not there.
        missing: NodeKey,
    },
    /// The same source is listed twice at one sink. Rejected rather than
    /// delivering every event twice: it is always an editing mistake, and the
    /// per-edge PDC state is keyed by `(sink, source)`.
    DuplicateEventSource {
        /// The sink port.
        at: EventIn,
        /// The repeated source.
        from: EventOut,
    },
    /// A generation is recorded for a node that does not exist.
    UnknownGeneration {
        /// The key.
        node: NodeKey,
    },
    /// A resolution requirement names an event edge that is not in the
    /// graph — a stale mark, which would otherwise pass unchecked.
    RequirementWithoutEdge {
        /// The sink port.
        at: EventIn,
        /// The source port.
        from: EventOut,
    },
    /// A param edge names a node that is not in the topology.
    UnknownParamNode {
        /// The param port whose entry names it.
        at: ParamIn,
        /// The key that is not there.
        missing: NodeKey,
    },
    /// A param port's sources are not in strict source order: one is listed
    /// twice, or the list was edited by hand out of order.
    /// [`GraphSpec::connect_param`] keeps it right.
    UnsortedParamSources {
        /// The param port.
        at: ParamIn,
    },
    /// A param port has more than [`MAX_PARAM_SOURCES`](crate::MAX_PARAM_SOURCES)
    /// sources.
    TooManyParamSources {
        /// The param port.
        at: ParamIn,
        /// How many it has.
        count: usize,
    },
    /// A param port's range has a NaN bound. Refused here rather than
    /// ordered where it is applied: a NaN bound clamps every value to NaN or
    /// to the other bound, and `f32::clamp` panics on it — on the audio
    /// thread. An infinite bound is fine (no clamp on that side).
    BadParamRange {
        /// The param port.
        at: ParamIn,
        /// Its range.
        range: ParamRange,
    },
}

/// A [`GraphSpec`] that passed [`validate`](GraphSpec::validate).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ValidGraph {
    valid: Valid,
    events: BTreeMap<EventIn, Vec<EventEdge>>,
    generations: BTreeMap<NodeKey, u32>,
    required_resolution: BTreeMap<(EventIn, EventOut), Resolution>,
    params: BTreeMap<ParamIn, ParamMod>,
}

impl ValidGraph {
    /// The checked topology.
    pub fn topology(&self) -> &Topology {
        self.valid.get()
    }

    /// The checked event edges.
    pub fn events(&self) -> &BTreeMap<EventIn, Vec<EventEdge>> {
        &self.events
    }

    /// The generation of `node`: 0 unless one was set.
    pub fn generation(&self, node: NodeKey) -> u32 {
        self.generations.get(&node).copied().unwrap_or(0)
    }

    /// The checked resolution requirements, keyed `(sink, source)`.
    pub fn required_resolution(&self) -> &BTreeMap<(EventIn, EventOut), Resolution> {
        &self.required_resolution
    }

    /// The checked modulated params: only ports with at least one source.
    pub fn params(&self) -> &BTreeMap<ParamIn, ParamMod> {
        &self.params
    }
}

/// The latency folds over the whole spec: the topology's audio ports, plus
/// each node's event and param-modulation sources, which the compiler counts
/// toward a node's arrival. So `tutti_types::latency::plan(&spec)` is the
/// compensation `compile` gives the spec's plan (`tests/compile_passes.rs`
/// pins it); over `spec.topology` alone, a node fed events by a latent node
/// would arrive early.
impl LatencyGraph for GraphSpec {
    type Node = NodeKey;

    fn nodes(&self) -> impl Iterator<Item = NodeKey> {
        LatencyGraph::nodes(&self.topology)
    }

    fn latency(&self, node: NodeKey) -> Samples {
        LatencyGraph::latency(&self.topology, node)
    }

    fn inputs(&self, node: NodeKey) -> impl Iterator<Item = Feed<NodeKey>> {
        LatencyGraph::inputs(&self.topology, node)
    }

    /// Direct event sources (a feedback edge carries last block's events, so
    /// nothing along this block's path) and param-modulation sources.
    fn other_sources(&self, node: NodeKey) -> impl Iterator<Item = NodeKey> {
        let ports = EventIn { node, port: 0 }..=EventIn {
            node,
            port: u16::MAX,
        };
        let events = self
            .events
            .range(ports)
            .flat_map(|(_, sources)| sources)
            .filter_map(|e| match *e {
                EventEdge::Direct(from) => Some(from.node),
                EventEdge::Feedback { .. } => None,
            });
        let params = self
            .params
            .iter()
            .filter(move |(at, _)| at.node == node)
            .flat_map(|(_, m)| m.sources.iter().map(|s| s.from.node()));
        events.chain(params)
    }

    fn outputs(&self) -> impl Iterator<Item = Option<NodeKey>> {
        LatencyGraph::outputs(&self.topology)
    }
}
