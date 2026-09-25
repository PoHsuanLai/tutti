//! Taking a node back out of the graph.

use bevy_ecs::prelude::*;

use tutti_core::AudioNode;

use crate::graph::{AudioGraphRes, GraphDirty};

/// Observer: removes a graph node when its `AudioNode` component is removed
/// (including via despawn).
///
/// `On<Remove, AudioNode>` fires *before* the component value is dropped, so
/// the `NodeId` is still readable off the triggered entity — no local
/// `(Entity, NodeId)` map needed. Only mutates the graph + sets `GraphDirty`;
/// the per-frame [`commit_graph`](crate::graph::commit_graph) (Commit phase)
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
    graph: Option<ResMut<AudioGraphRes>>,
    // `Option` to match `graph`. Both come from the plugin that registers this
    // observer, so this is belt-and-braces rather than a live bug — but an
    // observer has no run condition, and a half-optional signature is how the
    // sibling observer in `wire.rs` ended up able to panic.
    dirty: Option<ResMut<GraphDirty>>,
) {
    let entity = remove.event_target();
    crate::graph::capture::drop_captured(&mut commands, entity);
    let Ok(node) = nodes.get(entity) else { return };
    let Some(mut graph) = graph else { return };
    let Some(mut dirty) = dirty else { return };
    if graph.remove(*node) {
        dirty.0 = true;
    }
}
