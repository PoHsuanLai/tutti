//! The graph-node handle — plain data, always compiled.
//!
//! [`AudioNode`] wraps a fundsp [`NodeId`] and IS the Net-pump's entity binding:
//! it's not DAW vocabulary, it's the graph handle. Under the `bevy` feature it
//! gains `#[derive(Component)]` and becomes the ECS component the reconcile hub
//! reads; without it it stays a plain newtype a non-Bevy host can carry.
//!
//! This is the whole of what used to be the `graph` module: the DAW param
//! components (`Volume`/`Pan`/`Mute`/`ModParam`/`LayerKey`/`PluginParam`) moved
//! OUT of the engine to `dawai_model::engine_bind::foundational`, leaving only
//! this one node handle — so the two-file `graph/` wrapper collapsed into this
//! single file. ("Engine Bevy = Net pump only.")

#[cfg(feature = "bevy")]
use bevy_ecs::prelude::Component;

use crate::dsp::NodeId;

/// Component identity for a graph node.
///
/// Wraps a fundsp [`NodeId`]. Inserted by host helpers (e.g.
/// `Commands::spawn_audio_node`) immediately after the underlying node is
/// added to the graph; removing this component (or despawning the entity)
/// is the signal for the host to remove the node and `commit()`.
///
/// Plain `Copy`. Cheap to clone, store in queries, and hand around.
///
/// Not `Reflect`: the wrapped fundsp `NodeId` is foreign and not reflected.
/// If a host needs scene-serialization for these entities it should map
/// `AudioNode` to/from a stable id of its own (e.g. an asset path or
/// document node id) at serialization time.
#[cfg_attr(feature = "bevy", derive(Component))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AudioNode(pub NodeId);

impl AudioNode {
    /// Convenience: returns the wrapped [`NodeId`].
    #[inline]
    pub fn id(self) -> NodeId {
        self.0
    }
}

impl From<NodeId> for AudioNode {
    #[inline]
    fn from(id: NodeId) -> Self {
        Self(id)
    }
}

impl From<AudioNode> for NodeId {
    #[inline]
    fn from(node: AudioNode) -> Self {
        node.0
    }
}
