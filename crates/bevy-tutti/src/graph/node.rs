//! The graph-node handle an entity carries.

use bevy_ecs::prelude::Component;
use tutti_types::NodeKey;

/// Component identity for a graph node.
///
/// Wraps the [`NodeKey`] the node was inserted at. Inserted by host helpers (e.g.
/// `Commands::spawn_audio_node`) immediately after the underlying node is
/// added to the graph; removing this component (or despawning the entity)
/// is the signal for the host to remove the node and `commit()`.
///
/// Plain `Copy`. Not `Reflect`: a key is a runtime handle, not scene data. A
/// host that needs scene serialization for these entities maps `AudioNode`
/// to and from a stable id of its own at serialization time.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AudioNode(pub NodeKey);

impl AudioNode {
    /// Returns the graph key this handle names.
    #[inline]
    #[must_use]
    pub fn key(self) -> NodeKey {
        self.0
    }
}

impl From<NodeKey> for AudioNode {
    #[inline]
    fn from(key: NodeKey) -> Self {
        Self(key)
    }
}

impl From<AudioNode> for NodeKey {
    #[inline]
    fn from(node: AudioNode) -> Self {
        node.0
    }
}
