//! Wiring bus to bus (doc 013, "Buses and coercion between declared layouts",
//! B2): [`BusRef`], [`BusSource`], the spec's `bus_edges`, and the pure
//! function that **lowers** a bus edge to weighted lanes.
//!
//! A bus edge says "this output bus feeds that input bus". Its meaning is a
//! [`Fold`] between the two declared topologies, and the fold is a weighted
//! fan-in, so [`GraphSpec::lowered`] replaces every bus edge with per-port
//! edges: for each destination channel `c` and each source channel `k` the
//! fold sends to it with gain `g`, the edge `(sink port c) <- (source port k)`
//! carrying weight `g`. The compiler and the reference interpreter never see
//! a bus edge: they take the lowered spec, so the one sum, the one
//! determinism rule and the one PDC rule serve a fold.
//!
//! # What a lowered lane is
//!
//! - A lane of gain exactly `1.0` is a **plain edge**: no entry in
//!   [`GraphSpec::gains`], so an identity fold (equal topologies, a mono
//!   source into stereo, a select) is the hand-wired graph, and its plan is
//!   the hand-wired plan's.
//! - Any other gain is a **static weight**: an [`EdgeGain`] with that base,
//!   the default range and no source, in the *lowered* spec only. It is not
//!   the user's: it is never in the spec the author edits, it has no knob
//!   ([`Editor::edge_gain_cell`](crate::Editor::edge_gain_cell) is `None`),
//!   and [`Editor::set_edge_gain`](crate::Editor::set_edge_gain) refuses the
//!   lane ([`GraphInvalid::GainOnBusLane`](crate::GraphInvalid::GainOnBusLane)), as does a hand-written entry in
//!   `gains` at a lane's [`EdgeId`] ([`BusError::GainOnLane`]). A bus edge
//!   carries no gain of its own in this change.
//! - A lane at the `(sink port, source)` of an explicit per-port edge is
//!   refused ([`BusError::Overlaps`]): a duplicate by another name.
//!
//! The lowering is a pure function of the spec's `bus_edges` and topology and
//! the nodes' declared buses, in sorted order: the same inputs give the same
//! edges and weights, bit for bit.

use std::collections::BTreeMap;

use tutti_types::{Amplitude, BusTopology, ChannelTopology, Fold, FoldError};

use crate::gain::EdgeGain;
use crate::spec::GraphSpec;
use crate::{Edge, EdgeId, InPort, NodeKey, OutPort, Shapes, Sink, Source};

/// One bus of one node: bus `bus` of `node`'s declared
/// [`Shape::buses_in`](crate::Shape::buses_in) (as a sink) or
/// [`Shape::buses_out`](crate::Shape::buses_out) (as a source).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BusRef {
    /// The node.
    pub node: NodeKey,
    /// Which of its buses on that side, in port order.
    pub bus: u8,
}

/// What feeds a sink bus.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BusSource {
    /// An output bus of a node.
    Node(BusRef),
    /// The graph's own global input, as one
    /// [`Discrete`](tutti_types::Speaker::Discrete) bus of its width: it folds
    /// only to a bus of the same width and kind.
    Global,
}

/// Which side of a node a bus was asked for on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BusSide {
    /// An output bus: a source's.
    Output,
    /// An input bus: a sink's.
    Input,
}

/// Why a bus edge cannot be declared or lowered.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BusError {
    /// A bus edge names a node the graph does not have (no shape for it).
    NoSuchNode {
        /// The node.
        node: NodeKey,
    },
    /// The node declares no bus at that index on that side.
    NoSuchBus {
        /// The bus asked for.
        bus: BusRef,
        /// The side of the node it was asked on: a sink bus is an input, a
        /// source bus an output.
        side: BusSide,
    },
    /// The global input is wider than a bus holds
    /// ([`MAX_BUS_CHANNELS`](tutti_types::MAX_BUS_CHANNELS)).
    GlobalTooWide {
        /// The global input's width.
        width: usize,
    },
    /// The two topologies cannot be folded: a channel has no place, a width
    /// disagrees, or a side is empty. Never a silent drop.
    Fold {
        /// The source bus.
        from: BusSource,
        /// The sink bus.
        to: BusRef,
        /// Why.
        error: FoldError,
    },
    /// A lowered lane lands on an edge the spec already has.
    Overlaps {
        /// The lane.
        id: EdgeId,
    },
    /// The spec's `gains` holds an entry at a lane's edge: a weight the author
    /// set where the bus edge sets its own.
    GainOnLane {
        /// The lane.
        id: EdgeId,
    },
}

impl std::fmt::Display for BusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSuchNode { node } => write!(f, "no node {} for a bus edge", node.0),
            Self::NoSuchBus { bus, side } => write!(
                f,
                "node {} declares no {side:?} bus {}",
                bus.node.0, bus.bus
            ),
            Self::GlobalTooWide { width } => {
                write!(f, "the global input ({width} channels) is wider than a bus")
            }
            Self::Fold { from, to, error } => {
                write!(f, "bus edge {from:?} -> {to:?} cannot be folded: {error}")
            }
            Self::Overlaps { id } => write!(f, "a bus edge's lane {id:?} is also a plain edge"),
            Self::GainOnLane { id } => {
                write!(f, "a gain is set on the lane {id:?} of a bus edge")
            }
        }
    }
}

impl std::error::Error for BusError {}

/// A spec with its bus edges lowered, and what the lowering made.
#[derive(Clone, Debug, PartialEq)]
pub struct Lowering {
    /// The spec with every bus edge replaced by its lanes: per-port edges in
    /// the topology and static weights in `gains`, and no `bus_edges`. This is
    /// what [`validate`](GraphSpec::validate) and [`compile`](fn@crate::compile)
    /// take.
    pub spec: GraphSpec,
    /// Every lane a bus edge produced, plain or weighted, with its weight
    /// (`1.0` for a plain one), in edge order.
    pub lanes: BTreeMap<EdgeId, Amplitude>,
}

impl Lowering {
    /// The lanes with a weight other than `1.0`: the ones that became weighted
    /// lanes of their sink's sum.
    #[must_use]
    pub fn weights(&self) -> BTreeMap<EdgeId, Amplitude> {
        self.lanes
            .iter()
            .filter(|(_, w)| w.get().to_bits() != 1.0f32.to_bits())
            .map(|(&id, &w)| (id, w))
            .collect()
    }
}

impl GraphSpec {
    /// Adds `from` to the sources of sink bus `to`, in source order. Returns
    /// whether it was new. Nothing is checked: the bus edge meets the nodes'
    /// buses when it is [`lowered`](Self::lowered) (at a commit), and
    /// [`Editor::connect_bus`](crate::Editor::connect_bus) checks it at the
    /// call.
    pub fn connect_bus(&mut self, from: BusSource, to: BusRef) -> bool {
        let sources = self.bus_edges.entry(to).or_default();
        match sources.binary_search(&from) {
            Ok(_) => false,
            Err(i) => {
                sources.insert(i, from);
                true
            }
        }
    }

    /// Removes `from` from the sources of sink bus `to`, and the key with the
    /// last one. Returns whether it was there. Its lanes go with it at the
    /// next lowering: the author's own edges and gains are not touched.
    pub fn disconnect_bus(&mut self, from: BusSource, to: BusRef) -> bool {
        let Some(sources) = self.bus_edges.get_mut(&to) else {
            return false;
        };
        let Some(i) = sources.iter().position(|s| *s == from) else {
            return false;
        };
        sources.remove(i);
        if sources.is_empty() {
            self.bus_edges.remove(&to);
        }
        true
    }

    /// The lanes of the bus edge `from -> to` against `shapes`: `(edge,
    /// weight)` in sink-port then source order. A pure function of the two
    /// buses' topologies (and the global input's width).
    fn bus_lanes(
        &self,
        shapes: &Shapes,
        from: BusSource,
        to: BusRef,
    ) -> Result<Vec<(EdgeId, f32)>, BusError> {
        let sink_shape = shapes
            .get(&to.node)
            .ok_or(BusError::NoSuchNode { node: to.node })?;
        let no_sink = BusError::NoSuchBus {
            bus: to,
            side: BusSide::Input,
        };
        let sink_bus = sink_shape
            .buses_in
            .get(usize::from(to.bus))
            .ok_or(no_sink.clone())?;
        let sink_first = sink_shape
            .buses_in
            .first_port(usize::from(to.bus))
            .ok_or(no_sink)?;

        let (source_topology, source_first, source_node) = match from {
            BusSource::Node(bus) => {
                let shape = shapes
                    .get(&bus.node)
                    .ok_or(BusError::NoSuchNode { node: bus.node })?;
                let no_bus = BusError::NoSuchBus {
                    bus,
                    side: BusSide::Output,
                };
                let found = shape
                    .buses_out
                    .get(usize::from(bus.bus))
                    .ok_or(no_bus.clone())?;
                let first = shape
                    .buses_out
                    .first_port(usize::from(bus.bus))
                    .ok_or(no_bus)?;
                (found.topology, first, Some(bus.node))
            }
            BusSource::Global => {
                let width = self.topology.inputs;
                let topology = BusTopology::discrete(width).ok_or(BusError::GlobalTooWide {
                    width: usize::from(width.count()),
                })?;
                (topology, 0, None)
            }
        };

        let fold = Fold::between(
            &ChannelTopology::from(source_topology),
            &ChannelTopology::from(sink_bus.topology),
        )
        .map_err(|error| BusError::Fold { from, to, error })?;

        let mut lanes = Vec::new();
        for c in 0..sink_bus.width() {
            let at = InPort {
                node: to.node,
                port: (sink_first + c) as u16,
            };
            for (k, gain) in fold.contributions(c) {
                let source = match source_node {
                    Some(node) => Source::Node(OutPort {
                        node,
                        port: (source_first + k) as u16,
                    }),
                    None => Source::Global(k as u16),
                };
                lanes.push((
                    EdgeId {
                        sink: Sink::Port(at),
                        from: Edge::Direct(source),
                    },
                    gain,
                ));
            }
        }
        Ok(lanes)
    }

    /// Checks the bus edge `from -> to` against `shapes` and the spec as it
    /// stands, without storing it: the buses exist, the topologies fold, and
    /// no lane is an edge the topology already has.
    pub(crate) fn check_bus_edge(
        &self,
        shapes: &Shapes,
        from: BusSource,
        to: BusRef,
    ) -> Result<(), BusError> {
        for (id, _) in self.bus_lanes(shapes, from, to)? {
            self.check_lane(id)?;
        }
        Ok(())
    }

    /// A lane may not be an edge the topology has, nor carry an entry the
    /// author set.
    fn check_lane(&self, id: EdgeId) -> Result<(), BusError> {
        if self.has_edge(id) {
            return Err(BusError::Overlaps { id });
        }
        if self.gains.contains_key(&id) {
            return Err(BusError::GainOnLane { id });
        }
        Ok(())
    }

    /// This spec with every bus edge lowered to weighted lanes (see the
    /// module docs): a pure function of the spec and `shapes`, which are
    /// the nodes' declared buses. The spec itself is unchanged.
    ///
    /// # Errors
    ///
    /// [`BusError`] for the first bus edge (in sink then source order) whose
    /// buses are missing, whose topologies do not fold, or whose lane is an
    /// edge or a gain the spec already has. Nothing is dropped.
    pub fn lowered(&self, shapes: &Shapes) -> Result<Lowering, BusError> {
        let mut spec = self.clone();
        spec.bus_edges.clear();
        let mut lanes = BTreeMap::new();
        for (&to, sources) in &self.bus_edges {
            for &from in sources {
                for (id, weight) in self.bus_lanes(shapes, from, to)? {
                    // Against the spec as lowered so far, so two bus edges
                    // that make the same lane are caught too.
                    spec.check_lane(id)?;
                    let Sink::Port(at) = id.sink else {
                        unreachable!("a bus lane goes into a node's port");
                    };
                    spec.topology.connect(at, id.from);
                    if weight.to_bits() != 1.0f32.to_bits() {
                        spec.gains.insert(id, EdgeGain::new(Amplitude::new(weight)));
                    }
                    lanes.insert(id, Amplitude::new(weight));
                }
            }
        }
        Ok(Lowering { spec, lanes })
    }
}
