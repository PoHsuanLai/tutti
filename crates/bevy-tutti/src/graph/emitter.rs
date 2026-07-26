//! Audio-emitter marker components.
//!
//! [`AudioEmitter`] binds an entity to a live node in tutti's graph;
//! [`AudioPlaybackState`] tracks whether that node is playing/stopped/finished.
//! Both are leaf-agnostic value types: whoever spawns an audio node inserts
//! them, and the spatial/metering systems read them.

use bevy_ecs::prelude::*;
use bevy_reflect::prelude::*;

use tutti_core::NodeId;

/// Marks an entity as an audio emitter with a live node in tutti's graph.
///
/// Inserted by whoever adds the node to the graph
/// is processed. Remove this component (or despawn the entity) to stop
/// playback and clean up the graph node.
///
/// Not `Reflect`: the wrapped fundsp `NodeId` is foreign and not reflected
/// (matching `tutti_core::node::AudioNode`).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[require(AudioPlaybackState)]
pub struct AudioEmitter {
    pub node_id: NodeId,
}

/// Playback state for audio emitters.
///
/// Updated by `audio_cleanup_system` when a non-looping sample finishes.
#[derive(Component, Default, Debug, Clone, Copy, PartialEq, Eq, Hash, Reflect)]
#[reflect(Component, Default)]
pub enum AudioPlaybackState {
    #[default]
    Stopped,
    Playing,
    Finished,
}
