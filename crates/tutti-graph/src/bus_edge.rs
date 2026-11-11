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
    /// [`Unassigned`](tutti_types::Speaker::Unassigned) bus of its width: it folds
    /// only to a bus of the same width and kind.
    Global,
}

/// How a bus edge maps its source's channels onto its sink's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BusFold {
    /// By speaker: the [`Fold`] between the two declared topologies
    /// ([`Fold::between`]), which never guesses. A bus that names no speaker
    /// meets another only at its width.
    Speakers,
    /// By index, asked for by name: sink channel `c` is source channel `c`, a
    /// wider sink is zero-filled and a narrower one refused
    /// ([`Fold::by_index`]), whatever either side names.
    ByIndex,
}

/// One source of a sink bus and how it folds: an entry of
/// [`GraphSpec::bus_edges`] or [`GraphSpec::root_sources`], kept in `source`
/// order, once per source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BusFeed {
    /// What feeds the bus.
    pub source: BusSource,
    /// How its channels map onto the bus's.
    pub fold: BusFold,
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
    /// A bus edge into the root, and the root's topology is not declared
    /// ([`GraphSpec::root`] is `None`).
    RootUndeclared,
    /// A bus edge into the root, and the source cannot be folded to the
    /// root's declared topology.
    RootFold {
        /// The source bus.
        from: BusSource,
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
            Self::RootUndeclared => write!(f, "the root's topology is not declared"),
            Self::RootFold { from, error } => {
                write!(f, "bus edge {from:?} -> root cannot be folded: {error}")
            }
            Self::Overlaps { id } => write!(f, "a bus edge's lane {id:?} is also a plain edge"),
            Self::GainOnLane { id } => {
                write!(f, "a gain is set on the lane {id:?} of a bus edge")
            }
        }
    }
}

impl std::error::Error for BusError {}

/// Why the lanes of one bus edge could not be made: a bus error of its own,
/// or the fold, which the caller names with its sink.
enum LaneError {
    Bus(BusError),
    Fold(FoldError),
}

impl From<BusError> for LaneError {
    fn from(e: BusError) -> Self {
        Self::Bus(e)
    }
}

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
    /// Adds `from` to the sources of sink bus `to`, folded by speaker
    /// ([`BusFold::Speakers`]), in source order. Returns whether it was new.
    /// Nothing is checked: the bus edge meets the nodes' buses when it is
    /// [`lowered`](Self::lowered) (at a commit), and
    /// [`Editor::connect_bus`](crate::Editor::connect_bus) checks it at the
    /// call.
    pub fn connect_bus(&mut self, from: BusSource, to: BusRef) -> bool {
        self.feed_bus(from, to, BusFold::Speakers)
    }

    /// As [`connect_bus`](Self::connect_bus), channel by index
    /// ([`BusFold::ByIndex`]).
    pub fn connect_bus_by_index(&mut self, from: BusSource, to: BusRef) -> bool {
        self.feed_bus(from, to, BusFold::ByIndex)
    }

    fn feed_bus(&mut self, from: BusSource, to: BusRef, fold: BusFold) -> bool {
        insert_feed(self.bus_edges.entry(to).or_default(), from, fold)
    }

    /// Removes `from` from the sources of sink bus `to`, and the key with the
    /// last one. Returns whether it was there. Its lanes go with it at the
    /// next lowering: the author's own edges and gains are not touched.
    pub fn disconnect_bus(&mut self, from: BusSource, to: BusRef) -> bool {
        let Some(sources) = self.bus_edges.get_mut(&to) else {
            return false;
        };
        let Some(i) = sources.iter().position(|f| f.source == from) else {
            return false;
        };
        sources.remove(i);
        if sources.is_empty() {
            self.bus_edges.remove(&to);
        }
        true
    }

    /// Adds `from` to the bus edges into the root, folded by speaker, in
    /// source order. Returns whether it was new. As
    /// [`connect_bus`](Self::connect_bus), nothing is checked until the
    /// lowering.
    pub fn connect_bus_to_root(&mut self, from: BusSource) -> bool {
        insert_feed(&mut self.root_sources, from, BusFold::Speakers)
    }

    /// As [`connect_bus_to_root`](Self::connect_bus_to_root), channel by
    /// index ([`BusFold::ByIndex`]).
    pub fn connect_bus_to_root_by_index(&mut self, from: BusSource) -> bool {
        insert_feed(&mut self.root_sources, from, BusFold::ByIndex)
    }

    /// Removes `from` from the bus edges into the root. Returns whether it
    /// was there.
    pub fn disconnect_bus_from_root(&mut self, from: BusSource) -> bool {
        match self.root_sources.iter().position(|f| f.source == from) {
            Some(i) => {
                self.root_sources.remove(i);
                true
            }
            None => false,
        }
    }

    /// Whether the spec holds any bus edge, to a node or to the root.
    #[must_use]
    pub fn has_bus_edges(&self) -> bool {
        !self.bus_edges.is_empty() || !self.root_sources.is_empty()
    }

    /// The lanes of the bus edge `from -> to` against `shapes`: `(edge,
    /// weight)` in sink-port then source order. A pure function of the two
    /// buses' topologies (and the global input's width).
    fn bus_lanes(
        &self,
        shapes: &Shapes,
        from: BusSource,
        to: BusRef,
        fold: BusFold,
    ) -> Result<Vec<(EdgeId, f32)>, BusError> {
        let sink_shape = shapes
            .get(&to.node)
            .ok_or(BusError::NoSuchNode { node: to.node })?;
        let no_sink = BusError::NoSuchBus {
            bus: to,
            side: BusSide::Input,
        };
        let sink_bus = sink_shape
            .buses_in()
            .get(usize::from(to.bus))
            .ok_or(no_sink.clone())?;
        let sink_first = sink_shape
            .buses_in()
            .first_port(usize::from(to.bus))
            .ok_or(no_sink)?;
        self.fold_lanes(shapes, from, sink_bus.topology, fold, |c| {
            Sink::Port(InPort {
                node: to.node,
                port: (sink_first + c) as u16,
            })
        })
        .map_err(|error| match error {
            LaneError::Bus(e) => e,
            LaneError::Fold(error) => BusError::Fold { from, to, error },
        })
    }

    /// The lanes of the bus edge `from -> root`: the same fold, onto the
    /// output channels.
    fn root_lanes(
        &self,
        shapes: &Shapes,
        from: BusSource,
        fold: BusFold,
    ) -> Result<Vec<(EdgeId, f32)>, BusError> {
        // The engine's roots (`MAX_ROOT_CHANNELS`, 8) all fit a bus (16), so
        // a root with no `BusTopology` is one never declared.
        let root = self.root.ok_or(BusError::RootUndeclared)?;
        self.fold_lanes(shapes, from, root, fold, |c| Sink::Output(c as u16))
            .map_err(|error| match error {
                LaneError::Bus(e) => e,
                LaneError::Fold(error) => BusError::RootFold { from, error },
            })
    }

    /// The lanes of `from` folded to `sink`'s topology, each channel `c` at
    /// `at(c)`.
    fn fold_lanes(
        &self,
        shapes: &Shapes,
        from: BusSource,
        sink: BusTopology,
        by: BusFold,
        at: impl Fn(usize) -> Sink,
    ) -> Result<Vec<(EdgeId, f32)>, LaneError> {
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
                    .buses_out()
                    .get(usize::from(bus.bus))
                    .ok_or(no_bus.clone())?;
                let first = shape
                    .buses_out()
                    .first_port(usize::from(bus.bus))
                    .ok_or(no_bus)?;
                (found.topology, first, Some(bus.node))
            }
            BusSource::Global => {
                let width = self.topology.inputs;
                let topology = BusTopology::unassigned(width).ok_or(BusError::GlobalTooWide {
                    width: usize::from(width.count()),
                })?;
                (topology, 0, None)
            }
        };

        let fold = match by {
            BusFold::Speakers => Fold::between(
                &ChannelTopology::from(source_topology),
                &ChannelTopology::from(sink),
            ),
            BusFold::ByIndex => Fold::by_index(source_topology.layout(), sink.layout()),
        }
        .map_err(LaneError::Fold)?;

        let mut lanes = Vec::new();
        for c in 0..sink.positions().len() {
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
                        sink: at(c),
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
        fold: BusFold,
    ) -> Result<(), BusError> {
        for (id, _) in self.bus_lanes(shapes, from, to, fold)? {
            self.check_lane(id)?;
        }
        Ok(())
    }

    /// As [`check_bus_edge`](Self::check_bus_edge), for the root.
    pub(crate) fn check_root_bus_edge(
        &self,
        shapes: &Shapes,
        from: BusSource,
        fold: BusFold,
    ) -> Result<(), BusError> {
        for (id, _) in self.root_lanes(shapes, from, fold)? {
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
        spec.root_sources.clear();
        let mut lanes = BTreeMap::new();
        let mut place = |spec: &mut GraphSpec, id: EdgeId, weight: f32| -> Result<(), BusError> {
            // Against the spec as lowered so far, so two bus edges that make
            // the same lane are caught too.
            spec.check_lane(id)?;
            match (id.sink, id.from) {
                (Sink::Port(at), from) => {
                    spec.topology.connect(at, from);
                }
                (Sink::Output(c), Edge::Direct(source)) => {
                    // The root is as wide as its declaration; a shorter
                    // output list is widened in the lowered value only.
                    let c = usize::from(c);
                    if spec.topology.outputs.len() <= c {
                        spec.topology.outputs.resize(c + 1, Vec::new());
                    }
                    spec.topology.connect_output(c, source);
                }
                (Sink::Output(_), Edge::Feedback(_)) => {
                    unreachable!("a bus lane is a direct edge");
                }
            }
            if weight.to_bits() != 1.0f32.to_bits() {
                spec.gains.insert(id, EdgeGain::new(Amplitude::new(weight)));
            }
            lanes.insert(id, Amplitude::new(weight));
            Ok(())
        };
        for (&to, sources) in &self.bus_edges {
            for &BusFeed { source: from, fold } in sources {
                for (id, weight) in self.bus_lanes(shapes, from, to, fold)? {
                    place(&mut spec, id, weight)?;
                }
            }
        }
        for &BusFeed { source: from, fold } in &self.root_sources {
            for (id, weight) in self.root_lanes(shapes, from, fold)? {
                place(&mut spec, id, weight)?;
            }
        }
        Ok(Lowering { spec, lanes })
    }
}

/// Adds `from` with `fold` to a source list kept in source order, once per
/// source. Returns whether it was new.
fn insert_feed(sources: &mut Vec<BusFeed>, from: BusSource, fold: BusFold) -> bool {
    match sources.binary_search_by_key(&from, |f| f.source) {
        Ok(_) => false,
        Err(i) => {
            sources.insert(i, BusFeed { source: from, fold });
            true
        }
    }
}
