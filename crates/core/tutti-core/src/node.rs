//! The graph-node handle — plain data, always compiled.
//!
//! [`AudioNode`] wraps the [`NodeKey`] a node sits at in the graph: not DAW
//! vocabulary, just the graph handle. Under the `bevy` feature it gains
//! `#[derive(Component)]` and becomes the ECS component the reconcile hub
//! reads; without it it stays a plain newtype a non-Bevy host can carry.
//!
//! It is the *only* thing this crate puts behind that feature. DAW param
//! components (`Volume`, `Pan`, `Mute`, …) belong to the host adapter, not the
//! engine.

#[cfg(feature = "bevy")]
use bevy_ecs::prelude::Component;

use tutti_types::graph::NodeKey;

/// Component identity for a graph node.
///
/// Wraps the [`NodeKey`] the node was inserted at (fundsp's `NodeId` until
/// doc 013 Phase 5). Inserted by host helpers (e.g.
/// `Commands::spawn_audio_node`) immediately after the underlying node is
/// added to the graph; removing this component (or despawning the entity)
/// is the signal for the host to remove the node and `commit()`.
///
/// Plain `Copy`. Cheap to clone, store in queries, and hand around.
///
/// Not `Reflect`: a key is a runtime handle, not scene data. If a host needs
/// scene-serialization for these entities it should map `AudioNode` to/from
/// a stable id of its own (e.g. an asset path or document node id) at
/// serialization time.
#[cfg_attr(feature = "bevy", derive(Component))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AudioNode(pub NodeKey);

impl AudioNode {
    /// A handle at a key no earlier [`NodeKey::fresh`] returned.
    #[inline]
    pub fn fresh() -> Self {
        Self(NodeKey::fresh())
    }

    /// The graph key this handle names.
    #[inline]
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
