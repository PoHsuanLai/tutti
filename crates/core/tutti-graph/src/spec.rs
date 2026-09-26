//! The graph value this crate compiles: a [`Topology`] plus event edges and
//! unit generations.
//!
//! # Why a wrapper and not an extension of `tutti_types::graph`
//!
//! Doc 013 §1 proposes giving `InPort`/`OutPort` a kind. Doing that in
//! `tutti-types` is a breaking change: both are plain structs built by literal
//! (`InPort { node, port }`) across `bevy-tutti` and the tests, and adding a
//! field breaks every one of them. Event edges also have a different *shape*
//! from audio edges — owner decision 6 allows fan-in on event ports, so an
//! event sink holds a list of sources where an audio sink holds exactly one —
//! so the two maps cannot share `Topology::edges`' type anyway.
//!
//! [`GraphSpec`] therefore **embeds** the `Topology` unchanged (audio stays one
//! source per port, keyed on the sink — fan-in still unrepresentable there) and
//! adds what is new beside it, with distinct port types ([`EventIn`],
//! [`EventOut`]) so an audio port cannot be used as an event port by accident.
//! Every existing `Topology` user keeps compiling; when the adapter flips
//! (doc 013 Phase 3) the fields can move down if that still reads better.

use std::collections::{BTreeMap, BTreeSet};

use tutti_types::graph::{Invalid, Valid};
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

/// The graph value `compile` takes.
///
/// See the `spec` module's docs (`src/spec.rs`) for why this wraps `Topology` rather than
/// extending it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct GraphSpec {
    /// Nodes, audio edges, global outputs and global input width — unchanged.
    pub topology: Topology,
    /// Event edges, keyed on the sink port: every source of the port (an
    /// event input may have several — owner decision 6). An empty `Vec` is
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
    /// unit" — a rebind or a replacement (doc 013 §1, `NodeSpec.gen`). Absent
    /// means generation 0.
    pub generations: BTreeMap<NodeKey, u32>,
    /// Event edges that require their sink to honour offsets at least this
    /// finely, keyed `(sink, source)` — see
    /// [`require_resolution`](Self::require_resolution).
    pub required_resolution: BTreeMap<(EventIn, EventOut), Resolution>,
    /// Modulated params, keyed on the param port (design doc 013 item 6;
    /// see the `param` module docs, `src/param.rs`): each one's range and
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

    /// Add one event source to `at`, in source order (see
    /// [`events`](Self::events)). A source already listed at `at` is added
    /// again, and [`validate`](Self::validate) refuses the duplicate.
    pub fn connect_events(&mut self, at: EventIn, edge: EventEdge) {
        let sources = self.events.entry(at).or_default();
        let i = sources.partition_point(|e| e.from() <= edge.from());
        sources.insert(i, edge);
    }

    /// Mark the event edge `from → at` as requiring its sink to honour event
    /// offsets at least as finely as `resolution` (doc 013 §6 item 5).
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

    /// Drop the resolution mark on `from → at`, if any.
    pub fn unrequire_resolution(&mut self, at: EventIn, from: EventOut) {
        self.required_resolution.remove(&(at, from));
    }

    /// Remove the event edge `from → at` (direct or feedback), and its
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

    /// Drive param port `at` from `from` as well, shaped by `shaping`: one
    /// more offset in its sum, in source order ([`ParamFrom`]'s `Ord`), so
    /// two specs with the same wiring compare equal whatever order it was
    /// made in. A source already listed at `at` has its shaping replaced.
    ///
    /// The port must be one its node declares
    /// ([`Shape::params`](crate::Shape::params)), or `compile` refuses it
    /// ([`CompileError::UnknownParam`](crate::CompileError::UnknownParam)).
    /// Until a range is set ([`set_param_range`](Self::set_param_range)) the
    /// sum is not clamped.
    pub fn connect_param(&mut self, at: ParamIn, from: ParamFrom, shaping: ParamShaping) {
        let m = self.params.entry(at).or_default();
        match m.sources.binary_search_by(|s| s.from.cmp(&from)) {
            Ok(i) => m.sources[i].shaping = shaping,
            Err(i) => m.sources.insert(i, ParamSource { from, shaping }),
        }
    }

    /// Stop driving param port `at` from `from`. Returns whether it was a
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

    /// Clamp param port `at`'s modulated value to `range` — the param's own
    /// range, so no stack of modulators drives it past what the node
    /// accepts. A range change recompiles; it does not restart the port's
    /// sources.
    pub fn set_param_range(&mut self, at: ParamIn, range: ParamRange) {
        self.params.entry(at).or_default().range = range;
    }

    /// Check everything that does not need the nodes' [`Shape`](crate::Shape)s.
    ///
    /// The `Topology` half runs `Topology::validate` unchanged (so an audio
    /// cycle is reported exactly as it is today); the event half checks that
    /// every event edge names nodes that exist and that no source is listed
    /// twice at one sink. Event port *ranges* need the shapes and are checked
    /// by `compile`, as is a cycle that runs through an event edge.
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
