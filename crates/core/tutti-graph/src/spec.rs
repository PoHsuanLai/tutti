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
    /// Event edges, keyed on the sink port. The `Vec` is the **merge order**:
    /// events at equal offsets are delivered in this order (owner decision 6).
    /// An empty `Vec` is the same as an absent key.
    pub events: BTreeMap<EventIn, Vec<EventEdge>>,
    /// Unit generation per node. Bumping it is how a value says "same key, new
    /// unit" — a rebind or a replacement (doc 013 §1, `NodeSpec.gen`). Absent
    /// means generation 0.
    pub generations: BTreeMap<NodeKey, u32>,
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

    /// Append one event source to `at`'s merge list.
    pub fn connect_events(&mut self, at: EventIn, edge: EventEdge) {
        self.events.entry(at).or_default().push(edge);
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

        match valid {
            Some(valid) if errs.is_empty() => Ok(ValidGraph {
                valid,
                events: self.events.clone(),
                generations: self.generations.clone(),
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
}

/// A [`GraphSpec`] that passed [`validate`](GraphSpec::validate).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ValidGraph {
    valid: Valid,
    events: BTreeMap<EventIn, Vec<EventEdge>>,
    generations: BTreeMap<NodeKey, u32>,
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
}
