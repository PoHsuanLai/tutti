//! Turning an entity + a param address into a live accumulator.
//!
//! This is the one step the adapter cannot do generically, and the reason is
//! worth stating: [`ModParams`] is implemented on concrete node types
//! (`Compressor`, `ModDelayNode`, `PolySynth`, …). There is no `&dyn ModParams`
//! to recover from a `&dyn AudioUnit`, so no amount of Bevy plumbing can
//! dispatch it.
//!
//! So the host supplies the dispatch. [`ModTargetRegistry`] holds a list of
//! captures; each knows how to try one node type. Registering the node types an
//! app actually uses is a few lines, and the alternative — a match over every
//! node type in the engine — is the DAW vocabulary this crate exists to stay out
//! of.
//!
//! # Captured at insert, not resolved from the graph
//!
//! The registry runs **once per unit, before the unit goes into the graph** (see
//! [`CapturedControls`](crate::graph::CapturedControls)). A registered type's
//! capture keeps a clone of the unit as its [`ModParams`], stored on the entity
//! as a [`ModParamsHandle`]; resolution asks that, and never the graph.
//!
//! The clone is sound for the same reason a graph that clones its nodes on
//! commit is: every [`ModParams`] impl answers with an accumulator over the
//! node's *shared* param atomic, so the clone's accumulator writes the cell the
//! running node reads. Register types before spawning them — a node inserted
//! while its type was unregistered has no handle and is not modulatable.
//!
//! ```rust
//! use bevy_app::prelude::*;
//! use bevy_tutti::modulation::{ModTargetRegistry, TuttiModulationPlugin};
//!
//! let mut app = App::new();
//! app.add_plugins(TuttiModulationPlugin);
//! // One line per node type this app modulates. Forgetting one is silent: the
//! // route stays well-formed, the inspector shows the knob, nothing moves.
//! app.world_mut()
//!     .resource_mut::<ModTargetRegistry>()
//!     .register::<tutti_nodes::CompressorNode>()
//!     .register::<tutti_nodes::ModDelayNode>();
//! ```

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

/// One node type's capture: given an owned unit, keep it as [`ModParams`] if it
/// is a `T`.
type CaptureFn = fn(&dyn tutti_core::AudioUnit) -> Option<SharedModParams>;

/// The node types this app can modulate, plus any sinks it supplies directly.
///
/// Empty by default — an engine that knows every node type would be an engine
/// that owns a DAW's vocabulary. [`register`](Self::register) adds a node type;
/// [`insert_target`](Self::insert_target) adds one already-built sink.
#[derive(Resource, Default)]
pub struct ModTargetRegistry {
    captures: Vec<CaptureFn>,
    /// Sinks the host built itself, keyed by the param they serve. Consulted
    /// before the node resolvers — see [`insert_target`](Self::insert_target).
    supplied: HashMap<(Entity, ParamAddr), Arc<dyn ModTarget>>,
}

impl ModTargetRegistry {
    /// Teach the registry to resolve params on node type `T`.
    ///
    /// Captures are tried in registration order and the first match wins;
    /// since each matches one distinct concrete type, order only decides which
    /// of two *equally valid* answers is taken, and there are none.
    ///
    /// `Clone` because the capture keeps a clone of the unit — see the module
    /// docs. Takes effect for units inserted after this call.
    pub fn register<T: ModParams + tutti_core::AudioUnit + Clone + 'static>(
        &mut self,
    ) -> &mut Self {
        self.captures.push(|unit| {
            let node = unit.as_any().downcast_ref::<T>()?;
            Some(Arc::new(node.clone()) as SharedModParams)
        });
        self
    }

    /// `unit`'s control-rate params, if its type was registered.
    ///
    /// Run on the owned unit **before** it enters the graph; the answer goes on
    /// the entity as a [`ModParamsHandle`].
    pub fn capture(&self, unit: &dyn tutti_core::AudioUnit) -> Option<SharedModParams> {
        self.captures.iter().find_map(|c| c(unit))
    }

    /// Supply an already-built sink for `(entity, param)`, replacing any
    /// previous one.
    ///
    /// [`register`](Self::register) asks a *node type* for its accumulator,
    /// which is how a native param resolves — and every native node answers with
    /// an [`AtomicTarget`](tutti_mod::AtomicTarget). A sink that no `AudioUnit`
    /// owns has no node to capture it from and is otherwise unreachable: a
    /// plugin's per-block param target, or any accumulator a host evaluates at
    /// its own rate.
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
/// Written by the node-insertion paths from [`ModTargetRegistry::capture`], and
/// read by [`ModTargetResolver`]. A host that binds [`AudioNode`] itself after
/// pushing a unit by hand attaches one with
/// [`CapturedControls`](crate::graph::CapturedControls) or [`of`](Self::of).
///
/// Holds a clone of the unit. It never processes audio; it exists to mint
/// accumulators over the cells it shares with the running node.
#[derive(Component, Clone)]
pub struct ModParamsHandle {
    node: tutti_core::dsp::NodeId,
    params: SharedModParams,
}

impl ModParamsHandle {
    /// Captured params for the unit that became `node`.
    ///
    /// `node` is what makes a leftover handle inert: resolution skips one whose
    /// node is not the entity's current [`AudioNode`].
    pub fn new(node: tutti_core::dsp::NodeId, params: Arc<dyn ModParams + Send + Sync>) -> Self {
        Self { node, params }
    }

    /// Capture a typed `unit` directly, for a caller that has the concrete type
    /// in hand and no registry to consult.
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
    /// yet (a node materialises a frame after its entity), its node type was not
    /// registered when the node was inserted, or that type does not expose this
    /// param. A route that
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
