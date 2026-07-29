//! Taking a node back out of the graph.

use bevy_ecs::prelude::*;

use tutti_core::node::AudioNode;

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
/// [`Net::remove`](tutti_core::dsp::Net::remove) replaces every connection to
/// and from the unit with zeros, so a sink still declaring this entity as a
/// source is left silent rather than dangling.
pub fn reconcile_node_despawn(
    remove: On<Remove, AudioNode>,
    nodes: Query<&AudioNode>,
    graph: Option<ResMut<AudioGraphRes>>,
    mut dirty: ResMut<GraphDirty>,
) {
    let entity = remove.event_target();
    let Ok(node) = nodes.get(entity) else { return };
    let Some(mut graph) = graph else { return };
    if graph.0.contains(node.0) {
        graph.0.remove(node.0);
        dirty.0 = true;
    }
}
