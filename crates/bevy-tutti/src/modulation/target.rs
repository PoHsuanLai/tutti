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
}

impl ModTargetResolver<'_, '_> {
    /// The accumulator for `param` on `entity`, or `None` if the entity has no
    /// graph node yet, or its node type was never registered, or that node type
    /// does not expose this param.
    ///
    /// All three are ordinary — a node materialises a frame after its entity,
    /// and a param a node does not expose is a routing mistake the user can see
    /// and fix — so this returns `None` rather than logging. A route that cannot
    /// resolve is skipped and retried on the next rebuild.
    pub fn resolve(
        &self,
        entity: Entity,
        param: ParamAddr,
        range: &ParamRange,
    ) -> Option<Arc<dyn ModTarget>> {
        let node = self.nodes.get(entity).ok()?;
        self.registry.resolve(&self.graph, node.0, param, range)
    }

    pub fn bus(&self) -> Arc<ModBus> {
        Arc::clone(&self.bus.0)
    }
}
