//! `AudioVolume` parameter sync to tutti's `SamplerUnit::set_gain`.

use bevy_ecs::prelude::*;
use bevy_reflect::prelude::*;

#[cfg(feature = "sampler")]
use crate::sampler::SamplerUnit;

#[cfg(feature = "sampler")]
use crate::resources::TuttiGraphRes;

#[cfg(feature = "sampler")]
use super::emitter::AudioEmitter;

/// Volume control component. Synced to the tutti graph node by `audio_parameter_sync_system`.
#[derive(Component, Debug, Clone, Copy, PartialEq, Reflect)]
#[reflect(Component, Default, Clone)]
pub struct AudioVolume(pub f32);

impl Default for AudioVolume {
    fn default() -> Self {
        Self(1.0)
    }
}

/// Syncs `AudioVolume` component changes to the tutti graph node's gain.
#[cfg(feature = "sampler")]
pub fn audio_parameter_sync_system(
    mut graph: ResMut<TuttiGraphRes>,
    mut dirty: ResMut<crate::graph::GraphDirty>,
    query: Query<(&AudioEmitter, &AudioVolume), Changed<AudioVolume>>,
) {
    let mut edited = false;
    for (emitter, volume) in query.iter() {
        if let Some(sampler) = graph.0.node_mut::<SamplerUnit>(emitter.node_id) {
            sampler.set_gain(volume.0);
            edited = true;
        }
    }

    // Stage only; the Commit-phase `commit_graph` coalesces (this system is
    // anchored before that phase).
    if edited {
        dirty.0 = true;
    }
}
