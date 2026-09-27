//! Turning an entity + a param address into a live accumulator.
//!
//! Resolution asks what was captured when the entity's node went in, never
//! the graph (see [`CapturedControls`](crate::graph::CapturedControls)):
//!
//! - a node whose controls are a `tutti_graph::ParamSet` (every param-bearing
//!   node in the engine: the SVF, the delay, the chorus, the synth, …) is
//!   captured by address ([`CapturedControls::for_params`](crate::graph::CapturedControls::for_params)):
//!   its [`ModParamsHandle`] answers on each param the set holds, with an
//!   accumulator over the node's own cell;
//! - a sink no node owns — a hosted plugin's per-block param target, or any
//!   accumulator a host evaluates at its own rate — is supplied by the host
//!   through [`ModTargetRegistry::insert_target`].
//!
//! (A registry of `AudioUnit` types, each asked for a `ModParams` clone of
//! the unit at insert, answered the first case until every node was a
//! graph node with a `ParamSet`; it went with the `Legacy` adapter.)

use bevy_ecs::prelude::*;
use bevy_ecs::system::SystemParam;
use std::collections::HashMap;
use std::sync::Arc;

use tutti_core::AudioNode;
use tutti_mod::{ModBus, ModParams, ModTarget};
use tutti_types::ParamAddr;

use crate::modulation::components::ParamRange;

/// A unit's control-rate params, held apart from the graph.
type SharedModParams = Arc<dyn ModParams + Send + Sync>;

/// The sinks a host supplies directly, for params no node's `ParamSet`
/// holds. Empty by default; [`insert_target`](Self::insert_target) adds one
/// already-built sink.
#[derive(Resource, Default)]
pub struct ModTargetRegistry {
    /// Sinks the host built itself, keyed by the param they serve. Consulted
    /// before the node resolvers — see [`insert_target`](Self::insert_target).
    supplied: HashMap<(Entity, ParamAddr), Arc<dyn ModTarget>>,
}

impl ModTargetRegistry {
    /// Supply an already-built sink for `(entity, param)`, replacing any
    /// previous one.
    ///
    /// A node's params resolve through its `ParamSet`, each with an
    /// [`AtomicTarget`](tutti_mod::AtomicTarget) over its cell. A sink that no
    /// node owns has no set to capture it from and is otherwise unreachable:
    /// a plugin's per-block param target, or any accumulator a host evaluates
    /// at its own rate.
    ///
    /// This is also the only way to reach a sink that accepts **curve** layers,
    /// since `AtomicTarget` declines them (it collapses at a fixed beat, so a
    /// curve stored there would never move). Registering one is what makes
    /// [`ModDelivery::PerBlock`](crate::modulation::ModDelivery) more than a
    /// request that always falls back.
    ///
    /// The entity need not carry an [`AudioNode`] — it need not be in the graph
    /// at all. The registry holds an `Arc`, so the host keeps its own handle and
    /// both see one accumulator.
    pub fn insert_target(
        &mut self,
        entity: Entity,
        param: ParamAddr,
        target: Arc<dyn ModTarget>,
    ) -> &mut Self {
        self.supplied.insert((entity, param), target);
        self
    }

    /// Drop a supplied sink. No-op if none was registered.
    ///
    /// Resolution falls back to the node path afterwards, so removing a
    /// supplied sink for a param a node also exposes silently reverts to the
    /// node's own accumulator rather than un-modulating the param.
    pub fn remove_target(
        &mut self,
        entity: Entity,
        param: ParamAddr,
    ) -> Option<Arc<dyn ModTarget>> {
        self.supplied.remove(&(entity, param))
    }

    /// A host-supplied sink for this param, if one was registered.
    fn supplied(&self, entity: Entity, param: ParamAddr) -> Option<Arc<dyn ModTarget>> {
        self.supplied.get(&(entity, param)).map(Arc::clone)
    }
}

/// An entity's node, as [`ModParams`] — captured when the node was inserted.
///
/// Written by the node-insertion paths (a node's `ParamSet`, through
/// [`CapturedControls::for_params`](crate::graph::CapturedControls::for_params)),
/// and read by [`ModTargetResolver`]. A host that binds [`AudioNode`] itself
/// attaches one with [`CapturedControls`](crate::graph::CapturedControls) or
/// [`of`](Self::of).
///
/// It never processes audio; it exists to mint accumulators over the cells
/// it shares with the running node.
#[derive(Component, Clone)]
pub struct ModParamsHandle {
    node: tutti_core::dsp::NodeId,
    params: SharedModParams,
}

impl ModParamsHandle {
    /// Captured params for the node that became `node`.
    ///
    /// `node` is what makes a leftover handle inert: resolution skips one whose
    /// node is not the entity's current [`AudioNode`].
    pub fn new(node: tutti_core::dsp::NodeId, params: Arc<dyn ModParams + Send + Sync>) -> Self {
        Self { node, params }
    }

    /// Capture a typed `unit` directly, for a caller that has the concrete type
    /// in hand (a host's own `ModParams` type).
    pub fn of<T: ModParams + Clone + Send + Sync + 'static>(
        node: tutti_core::dsp::NodeId,
        unit: &T,
    ) -> Self {
        Self::new(node, Arc::new(unit.clone()))
    }

    /// The graph node these params were captured from.
    pub fn node(&self) -> tutti_core::dsp::NodeId {
        self.node
    }

    /// The captured params.
    pub fn params(&self) -> &(dyn ModParams + Send + Sync) {
        &*self.params
    }
}

impl std::fmt::Debug for ModParamsHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModParamsHandle")
            .field("node", &self.node)
            .finish_non_exhaustive()
    }
}

/// A node's [`ParamSet`](tutti_graph::ParamSet) as its control-rate
/// modulation targets: an [`AtomicTarget`](tutti_mod::AtomicTarget)
/// mirroring into the cell the node reads, for any [`ParamAddr::Unit`] the
/// set addresses. What [`CapturedControls::for_params`](crate::graph::CapturedControls::for_params)
/// captures.
pub(crate) struct ParamSetTargets(pub(crate) tutti_graph::ParamSet);

impl ModParams for ParamSetTargets {
    fn mod_target(
        &self,
        param: ParamAddr,
        base: f32,
        min: f32,
        max: f32,
    ) -> Option<Arc<dyn ModTarget>> {
        let ParamAddr::Unit(unit) = param else {
            return None;
        };
        let cell = self.0.cell(unit)?;
        Some(Arc::new(tutti_mod::AtomicTarget::with_mirror(
            base, min, max, cell,
        )))
    }
}

/// The shared id→sink map the driver dispatches through.
///
/// A resource so the registry and the driver agree on one bus: targets are
/// inserted during a rebuild and read every frame, and a second bus would mean
/// the driver dispatching into accumulators nothing reads.
#[derive(Resource, Clone, Default)]
pub struct ModBusRes(pub Arc<ModBus>);

/// The world access [`rebuild`](super::rebuild) needs to resolve a target.
///
/// Bundled as a [`SystemParam`] because it is three resources plus a query that
/// always travel together, and threading them through the rebuild signature
/// individually buys nothing.
#[derive(SystemParam)]
pub struct ModTargetResolver<'w, 's> {
    registry: Res<'w, ModTargetRegistry>,
    bus: Res<'w, ModBusRes>,
    /// Each node's params as captured at insert, with the node they came from.
    nodes: Query<'w, 's, (&'static AudioNode, &'static ModParamsHandle)>,
    /// Source entities whose own rate is modulated. A separate query because a
    /// modulation source is *not* a graph node — see [`resolve`](Self::resolve).
    rate_cells: Query<'w, 's, &'static crate::modulation::ModRateCell>,
}

impl ModTargetResolver<'_, '_> {
    /// The accumulator for `param` on `entity`, or `None` if nothing on this
    /// entity exposes it.
    ///
    /// Three kinds of target, tried in order:
    ///
    /// 1. **A host-supplied sink** — registered with
    ///    [`insert_target`](ModTargetRegistry::insert_target). First because it
    ///    is an explicit override: a host that hands over an accumulator for a
    ///    param means that one, even if the entity's node would also answer.
    /// 2. **A modulation source's own rate** — the cascade case. A source entity
    ///    carries a [`ModRateCell`](crate::modulation::ModRateCell) and no
    ///    `AudioNode`, so the graph path below could never have served it. The
    ///    accumulator mirrors straight into that cell, which is what makes an
    ///    LFO able to drive another LFO's rate.
    /// 3. **A graph node's param** — the ordinary case, answered by the
    ///    [`ModParamsHandle`] captured when the node was inserted.
    ///
    /// `None` covers several ordinary situations — the entity has no graph node
    /// yet (a node materialises a frame after its entity), its node has no
    /// params, or its `ParamSet` does not hold this one. A route that
    /// cannot resolve is skipped and retried on the next rebuild rather than
    /// logged.
    pub fn resolve(
        &self,
        entity: Entity,
        param: ParamAddr,
        range: &ParamRange,
    ) -> Option<Arc<dyn ModTarget>> {
        if let Some(target) = self.registry.supplied(entity, param) {
            return Some(target);
        }
        if let Some(target) = self.resolve_rate(entity, param, range) {
            return Some(target);
        }
        let (node, handle) = self.nodes.get(entity).ok()?;
        if handle.node != node.0 {
            // Captured for a node this entity no longer carries.
            return None;
        }
        handle
            .params
            .mod_target(param, range.base, range.min, range.max)
    }

    /// The accumulator for a source's own rate — an [`AtomicTarget`] mirroring
    /// into the very cell the source reads its frequency from.
    ///
    /// Sharing that one cell is the entire mechanism, and getting it wrong
    /// fails silently: an accumulator pointed at any other atomic type-checks,
    /// runs, and modulates nothing. Hence
    /// [`ModRateCell::as_atomic`](crate::modulation::ModRateCell::as_atomic)
    /// rather than a fresh `Param`.
    fn resolve_rate(
        &self,
        entity: Entity,
        param: ParamAddr,
        range: &ParamRange,
    ) -> Option<Arc<dyn ModTarget>> {
        if param != ParamAddr::Unit(tutti_types::UnitParam::Rate) {
            return None;
        }
        let cell = self.rate_cells.get(entity).ok()?;
        Some(Arc::new(tutti_mod::AtomicTarget::with_mirror(
            range.base,
            range.min,
            range.max,
            cell.as_atomic(),
        )))
    }

    /// The shared [`ModBus`] a resolved target is registered on.
    ///
    /// The one bus [`ModBusRes`] holds — see it for why a second would leave the
    /// driver dispatching into accumulators nothing reads.
    pub fn bus(&self) -> Arc<ModBus> {
        Arc::clone(&self.bus.0)
    }
}
