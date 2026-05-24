//! Finished-sample detection: removes nodes from graph and (optionally) despawns entities.

use bevy_ecs::prelude::*;
use bevy_reflect::prelude::*;

#[cfg(feature = "sampler")]
use tutti::sampler::SamplerUnit;

#[cfg(feature = "sampler")]
use crate::resources::TuttiGraphRes;

#[cfg(feature = "sampler")]
use super::emitter::{AudioEmitter, AudioPlaybackState};

/// Marker component: entity will be despawned when its sample finishes playing.
#[derive(Component, Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Reflect)]
#[reflect(Component, Default)]
pub struct DespawnOnFinish;

/// Polls tutti graph for finished (non-looping) samples and updates
/// `AudioPlaybackState`. Removes graph nodes and optionally despawns entities.
#[cfg(feature = "sampler")]
pub fn audio_cleanup_system(
    mut commands: Commands,
    graph: Option<ResMut<TuttiGraphRes>>,
    mut query: Query<(
        Entity,
        &AudioEmitter,
        &mut AudioPlaybackState,
        Option<&DespawnOnFinish>,
    )>,
) {
    let Some(mut graph) = graph else { return };

    let mut edited = false;

    for (entity, emitter, mut state, despawn) in query.iter_mut() {
        if *state != AudioPlaybackState::Playing {
            continue;
        }

        let is_playing = graph
            .0
            .node::<SamplerUnit>(emitter.node_id)
            .map(|s| s.is_playing())
            .unwrap_or(false);

        if !is_playing {
            *state = AudioPlaybackState::Finished;

            if graph.0.contains(emitter.node_id) {
                graph.0.remove(emitter.node_id);
                edited = true;
            }

            if despawn.is_some() {
                commands.entity(entity).despawn();
            }
        }
    }

    if edited {
        graph.0.commit();
    }
}
