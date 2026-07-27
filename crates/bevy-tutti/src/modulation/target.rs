//! Turning an entity + a param address into a live accumulator.
//!
//! This is the one step the adapter cannot do generically, and the reason is
//! worth stating: [`ModParams`](tutti_mod::ModParams) is implemented on concrete
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
//! ```rust,ignore
//! app.world_mut()
//!     .resource_mut::<ModTargetRegistry>()
//!     .register::<tutti_units::Compressor>()
//!     .register::<tutti_units::ChorusNode>();
//! ```

use bevy_ecs::prelude::*;
use bevy_ecs::system::SystemParam;
use std::sync::Arc;

use tutti_core::AudioNode;
use tutti_mod::{ModBus, ModParams, ModTarget};
use tutti_types::ParamAddr;

use crate::graph::AudioGraphRes;
use crate::modulation::components::ParamRange;

/// One node type's resolver: given the graph and a node, hand back an
/// accumulator for `param` if this node is a `T` that exposes it.
type ResolveFn =
    fn(&AudioGraphRes, tutti_core::NodeId, ParamAddr, &ParamRange) -> Option<Arc<dyn ModTarget>>;

/// The node types this app can modulate.
///
/// Empty by default — an engine that knows every node type would be an engine
/// that owns a DAW's vocabulary. [`register`](Self::register) adds one.
#[derive(Resource, Default)]
pub struct ModTargetRegistry {
    resolvers: Vec<ResolveFn>,
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

    fn resolve(
        &self,
        graph: &AudioGraphRes,
        node: tutti_core::NodeId,
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
    graph: Res<'w, AudioGraphRes>,
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
    /// Two kinds of target, tried in order:
    ///
    /// 1. **A modulation source's own rate** — the cascade case. It is tried
    ///    first because it is the cheap, exact one: a source entity carries a
    ///    [`ModRateCell`](crate::modulation::ModRateCell) and no `AudioNode`, so
    ///    the graph path below could never have served it. The accumulator
    ///    mirrors straight into that cell, which is what makes an LFO able to
    ///    drive another LFO's rate.
    /// 2. **A graph node's param** — the ordinary case, resolved by downcast
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
        if let Some(target) = self.resolve_rate(entity, param, range) {
            return Some(target);
        }
        let node = self.nodes.get(entity).ok()?;
        self.registry.resolve(&self.graph, node.0, param, range)
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

    pub fn bus(&self) -> Arc<ModBus> {
        Arc::clone(&self.bus.0)
    }
}
