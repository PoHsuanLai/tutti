//! Taking a node back out of the graph.

use bevy_ecs::prelude::*;

use tutti_core::AudioNode;

use crate::graph::AudioGraphRes;

/// Observer: removes a graph node when its `AudioNode` component is removed
/// (including via despawn).
///
/// `On<Remove, AudioNode>` fires *before* the component value is dropped, so
/// the `NodeKey` is still readable off the triggered entity — no local
/// `(Entity, NodeKey)` map needed. Only mutates the graph (which marks the
/// session edited); the per-frame [`commit_graph`](crate::graph::commit_graph) (Commit phase)
/// does the actual commit.
///
/// [`AudioGraphRes::remove`] replaces every connection to and from the unit
/// with silence, so a sink still declaring this entity as a
/// source is left silent rather than dangling.
///
/// The controls captured from the unit at insertion go with it — see
/// [`capture`](crate::graph::capture). Dropped whatever the graph's state, so
/// they are released even when the node is already out of the graph.
pub fn reconcile_node_despawn(
    remove: On<Remove, AudioNode>,
    mut commands: Commands,
    nodes: Query<&AudioNode>,
    // `Option`: an observer has no run condition to hide behind.
    graph: Option<ResMut<AudioGraphRes>>,
) {
    let entity = remove.event_target();
    crate::graph::capture::drop_captured(&mut commands, entity);
    let Ok(node) = nodes.get(entity) else { return };
    let Some(mut graph) = graph else { return };
    graph.remove(*node);
}
