//! Sidechain wiring as a Bevy [`Relationship`].
//!
//! [`SidechainOf`] is a one-to-one relationship from a *source* entity
//! (the audio that drives the sidechain) to a *target* entity (the
//! compressor / gate / whatever exposes a sidechain input on port 1).
//! [`SidechainSources`] is its automatic relationship-target counterpart
//! on the target side.
//!
//! The reconcile system [`reconcile_sidechain_links`] runs in
//! [`GraphReconcileSystems::Spawn`]: on `Added<SidechainOf>` it looks up
//! both entities' [`AudioNode`] and calls
//! `graph.connect(src_node, 0, target_node, 1)`. On
//! `RemovedComponents<SidechainOf>` it disconnects the same port.
//!
//! Pure graph-op binding — no DAW vocabulary. The DAW concept of "this
//! compressor's sidechain follows this kick drum's bus" is built on
//! top of this primitive in dawai/mixer.

use bevy_ecs::prelude::*;

use tutti::core::ecs::AudioNode;

use super::reconcile::GraphDirty;
use crate::resources::TuttiGraphRes;

/// "This entity's audio drives `0`'s sidechain input (port 1)."
///
/// Insert on the *source* entity. The target side automatically grows a
/// [`SidechainSources`] component listing every source pointing at it.
#[derive(Component, Debug, Clone, Copy)]
#[relationship(relationship_target = SidechainSources)]
pub struct SidechainOf(pub Entity);

impl SidechainOf {
    /// The target entity.
    #[inline]
    pub fn target(self) -> Entity {
        self.0
    }
}

/// Auto-maintained list of every entity sidechained into this one.
///
/// Bevy's relationship infrastructure keeps this in sync with
/// [`SidechainOf`]. Read it on the target side to discover sources;
/// don't insert it manually.
#[derive(Component, Debug, Default)]
#[relationship_target(relationship = SidechainOf)]
pub struct SidechainSources(Vec<Entity>);

impl SidechainSources {
    pub fn iter(&self) -> impl Iterator<Item = Entity> + '_ {
        self.0.iter().copied()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }
}

/// Reconciles sidechain wiring into graph operations.
///
/// `Added<SidechainOf>`: looks up `(src_node, target_node)` from each side's
/// `AudioNode` component and calls `graph.connect(src_node, 0, target_node, 1)`.
/// `RemovedComponents<SidechainOf>`: disconnects port 1 on the target.
///
/// We track `(src_entity, target_entity)` pairs in a [`Local`] map keyed by
/// source entity so we know which target to disconnect from when the
/// component disappears (the despawn path can't read the removed value).
pub fn reconcile_sidechain_links(
    mut tracked: Local<std::collections::HashMap<Entity, Entity>>,
    graph: Option<ResMut<TuttiGraphRes>>,
    mut dirty: ResMut<GraphDirty>,
    added: Query<(Entity, &SidechainOf), Added<SidechainOf>>,
    nodes: Query<&AudioNode>,
    mut removed: RemovedComponents<SidechainOf>,
) {
    let Some(mut graph) = graph else {
        for entity in removed.read() {
            tracked.remove(&entity);
        }
        return;
    };

    for (src_entity, link) in added.iter() {
        let target_entity = link.0;
        let Ok(src_node) = nodes.get(src_entity) else {
            bevy_log::warn!(
                "SidechainOf: source {:?} has no AudioNode; skipping connect",
                src_entity
            );
            continue;
        };
        let Ok(target_node) = nodes.get(target_entity) else {
            bevy_log::warn!(
                "SidechainOf: target {:?} has no AudioNode; skipping connect",
                target_entity
            );
            continue;
        };
        // Bare oscillators / generators have no input port 1; calling
        // connect on them panics inside fundsp's Net. Skip with a warning
        // so misconfigured wiring is loud but not fatal.
        if graph.0.inputs(target_node.0) < 2 {
            bevy_log::warn!(
                "SidechainOf: target {:?} has only {} inputs (needs >= 2 for a sidechain port); skipping connect",
                target_entity,
                graph.0.inputs(target_node.0)
            );
            continue;
        }
        graph.0.connect(src_node.0, 0, target_node.0, 1);
        tracked.insert(src_entity, target_entity);
        dirty.0 = true;
    }

    for src_entity in removed.read() {
        let Some(target_entity) = tracked.remove(&src_entity) else {
            continue;
        };
        let Ok(target_node) = nodes.get(target_entity) else {
            // Target despawned along with the link; nothing to disconnect.
            continue;
        };
        if graph.0.inputs(target_node.0) < 2 {
            // We never connected (target had no sidechain input); nothing to undo.
            continue;
        }
        graph.0.disconnect(target_node.0, 1);
        dirty.0 = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::reconcile::GraphReconcileSystems;
    use bevy_app::App;
    use tutti::TuttiEngine;

    fn test_app() -> App {
        let engine = TuttiEngine::builder().inputs(0).outputs(2).build().expect("build engine");
        let TuttiEngine { graph, .. } = engine;

        let mut app = App::new();
        app.insert_resource(crate::resources::TuttiGraphRes(graph));
        app.init_resource::<GraphDirty>();
        app.configure_sets(
            bevy_app::Update,
            (
                GraphReconcileSystems::Spawn,
                GraphReconcileSystems::Params,
                GraphReconcileSystems::Despawn,
                GraphReconcileSystems::Commit,
            )
                .chain(),
        );
        app.add_systems(
            bevy_app::Update,
            reconcile_sidechain_links.in_set(GraphReconcileSystems::Spawn),
        );
        app
    }

    #[test]
    fn sidechain_link_grows_relationship_target() {
        // We exercise only Bevy's relationship machinery here; the actual
        // graph.connect call is verified by the integration example, which
        // spawns a node that *has* an input port 1 (e.g. a compressor).
        // Bare oscillators have zero inputs, so connecting to port 1 panics.
        let mut app = test_app();

        let src = app.world_mut().spawn_empty().id();
        let target = app.world_mut().spawn_empty().id();
        app.world_mut().entity_mut(src).insert(SidechainOf(target));
        app.update();

        let sources = app.world().get::<SidechainSources>(target).expect("SidechainSources");
        assert_eq!(sources.len(), 1);
        assert_eq!(sources.iter().next(), Some(src));
    }
}
