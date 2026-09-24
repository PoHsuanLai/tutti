//! Turning an entity + a param address into a live accumulator.
//!
//! This is the one step the adapter cannot do generically, and the reason is
//! worth stating: [`ModParams`] is implemented on concrete
//! node types (`Compressor`, `ChorusNode`, `PolySynth`, …), and reaching one
//! through the graph needs [`node_as::<T>`](tutti_core::dsp::Net::node_as) —
//! which takes a concrete `T`. There is no `&dyn ModParams` to recover from a
//! `&dyn AudioUnit`, so no amount of Bevy plumbing can dispatch it.
//!
//! So the host supplies the dispatch. [`ModTargetRegistry`] holds a list of
//! resolvers; each knows how to try one node type. Registering the node types an
//! app actually uses is a few lines, and the alternative — a match over every
//! node type in the engine — is the DAW vocabulary this crate exists to stay out
//! of.
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
//!     .register::<tutti_nodes::ChorusNode>();
//! ```

use bevy_ecs::prelude::*;
use bevy_ecs::system::SystemParam;
use std::collections::HashMap;
use std::sync::Arc;

use tutti_core::AudioNode;
use tutti_mod::{ModBus, ModParams, ModTarget};
use tutti_types::ParamAddr;

use crate::graph::AudioGraphRes;
use crate::modulation::components::ParamRange;

/// One node type's resolver: given the graph and a node, hand back an
/// accumulator for `param` if this node is a `T` that exposes it.
type ResolveFn = fn(
    &AudioGraphRes,
    tutti_core::dsp::NodeId,
    ParamAddr,
    &ParamRange,
) -> Option<Arc<dyn ModTarget>>;

/// The node types this app can modulate, plus any sinks it supplies directly.
///
/// Empty by default — an engine that knows every node type would be an engine
/// that owns a DAW's vocabulary. [`register`](Self::register) adds a node type;
/// [`insert_target`](Self::insert_target) adds one already-built sink.
#[derive(Resource, Default)]
pub struct ModTargetRegistry {
    resolvers: Vec<ResolveFn>,
    /// Sinks the host built itself, keyed by the param they serve. Consulted
    /// before the node resolvers — see [`insert_target`](Self::insert_target).
    supplied: HashMap<(Entity, ParamAddr), Arc<dyn ModTarget>>,
}

impl ModTargetRegistry {
    /// Teach the registry to resolve params on node type `T`.
    ///
    /// Resolvers are tried in registration order and the first match wins;
    /// since each is a downcast to a distinct concrete type, order only decides
    /// which of two *equally valid* answers is taken, and there are none.
    pub fn register<T: ModParams + tutti_core::AudioUnit + 'static>(&mut self) -> &mut Self {
        self.resolvers.push(|graph, node, param, range| {
            graph
                .0
                .node_as::<T>(node)?
                .mod_target(param, range.base, range.min, range.max)
        });
        self
    }

    /// Supply an already-built sink for `(entity, param)`, replacing any
    /// previous one.
    ///
    /// [`register`](Self::register) asks a *node type* for its accumulator,
    /// which is how a native param resolves — and every native node answers with
    /// an [`AtomicTarget`](tutti_mod::AtomicTarget). A sink that no `AudioUnit`
    /// owns has no node to be downcast from and is otherwise unreachable: a
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

    fn resolve(
        &self,
        graph: &AudioGraphRes,
        node: tutti_core::dsp::NodeId,
        param: ParamAddr,
        range: &ParamRange,
    ) -> Option<Arc<dyn ModTarget>> {
        self.resolvers
            .iter()
            .find_map(|r| r(graph, node, param, range))
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
    /// `Option` because a `SystemParam` validates before its system runs: a hard
    /// `Res` here panics the schedule for every caller, whatever their own
    /// signature says. The graph is `build_into`'s, while `engine_ready` only
    /// reads `AudioEngineState`.
    ///
    /// Note this degrades rather than disables: [`resolve`](Self::resolve)
    /// checks host-supplied targets and a source's own rate *before* the graph,
    /// and neither needs one. Only the node-downcast tier goes quiet.
    graph: Option<Res<'w, AudioGraphRes>>,
    registry: Res<'w, ModTargetRegistry>,
    bus: Res<'w, ModBusRes>,
    nodes: Query<'w, 's, &'static AudioNode>,
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
    /// 3. **A graph node's param** — the ordinary case, resolved by downcast
    ///    through the registry.
    ///
    /// `None` covers several ordinary situations — the entity has no graph node
    /// yet (a node materialises a frame after its entity), its node type was
    /// never registered, or that type does not expose this param. A route that
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
        let graph = self.graph.as_ref()?;
        let node = self.nodes.get(entity).ok()?;
        self.registry.resolve(graph, node.0, param, range)
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
