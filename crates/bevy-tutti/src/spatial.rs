//! 3D spatial audio: lazy panner-node insertion + transform-driven panning.
//!
//! Each entity carrying [`SpatialAudio`] gets a `SpatialPannerNode` lazily
//! created in tutti's graph. Each frame, [`spatial_audio_sync_system`]
//! computes listener-relative azimuth/elevation/distance and applies
//! position + attenuation gain.

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;
use bevy_log::warn;
use bevy_reflect::prelude::*;
use bevy_transform::components::GlobalTransform;

use tutti::sampler::SamplerUnit;
use tutti::NodeId;

use crate::playback::{audio_cleanup_system, audio_playback_system, AudioEmitter};
use crate::resources::TuttiGraphRes;

/// Marks an entity as the audio listener (typically the camera).
///
/// Only one listener should exist at a time. Spatial audio positions
/// are computed relative to this entity's `GlobalTransform`.
#[derive(Component, Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Reflect)]
#[reflect(Component, Default)]
#[require(GlobalTransform)]
pub struct AudioListener;

/// Enables 3D spatial audio for an emitter entity.
///
/// Requires `GlobalTransform` on the same entity (auto-inserted via
/// `#[require]`). [`AudioEmitter`] is also expected on the same entity, but
/// is not auto-required because it has no meaningful `Default` (its
/// `node_id` is filled in by the playback system once the wave loads).
/// Spawn `SpatialAudio` alongside a `PlayAudio` trigger; the emitter shows
/// up on the next frame.
///
/// Not `Reflect`: `panner_node_id` wraps a foreign fundsp `NodeId`.
#[derive(Component, Debug, Clone)]
#[require(GlobalTransform)]
pub struct SpatialAudio {
    pub(crate) panner_node_id: Option<NodeId>,
    pub attenuation: AttenuationModel,
    pub max_distance: f32,
    pub ref_distance: f32,
}

impl Default for SpatialAudio {
    fn default() -> Self {
        Self {
            panner_node_id: None,
            attenuation: AttenuationModel::InverseDistance,
            max_distance: 100.0,
            ref_distance: 1.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Reflect)]
pub enum AttenuationModel {
    #[default]
    InverseDistance,
    Linear,
    Exponential,
}

/// Syncs entity `GlobalTransform` to tutti's spatial panner nodes.
///
/// Lazily creates a `SpatialPannerNode` for each emitter with `SpatialAudio`.
/// Computes listener-relative azimuth/elevation and applies distance attenuation.
pub fn spatial_audio_sync_system(
    graph: Option<ResMut<TuttiGraphRes>>,
    listener_query: Query<&bevy_transform::components::GlobalTransform, With<AudioListener>>,
    mut emitter_query: Query<(
        &bevy_transform::components::GlobalTransform,
        &AudioEmitter,
        &mut SpatialAudio,
    )>,
) {
    let Some(mut graph) = graph else { return };
    let listener_tf = listener_query.single().ok();

    let mut edited = false;

    for (emitter_tf, emitter, mut spatial) in emitter_query.iter_mut() {
        if spatial.panner_node_id.is_none() {
            let emitter_node = emitter.node_id;
            let Ok(panner) = tutti::units::SpatialPannerNode::stereo() else {
                warn!("Failed to create SpatialPannerNode");
                continue;
            };
            let panner_id = graph.0.add(panner);
            // Route: emitter → panner → master
            graph.0.connect(emitter_node, 0, panner_id, 0);
            graph.0.pipe_output(panner_id);
            edited = true;
            spatial.panner_node_id = Some(panner_id);
        }

        let Some(panner_id) = spatial.panner_node_id else {
            continue;
        };

        let (azimuth, elevation, distance) = if let Some(listener) = listener_tf {
            let relative = listener
                .affine()
                .inverse()
                .transform_point3(emitter_tf.translation());
            let az = (-relative.x).atan2(-relative.z).to_degrees();
            let el = relative
                .y
                .atan2((relative.x * relative.x + relative.z * relative.z).sqrt())
                .to_degrees();
            (az, el, relative.length())
        } else {
            let pos = emitter_tf.translation();
            let az = pos.x.atan2(pos.z).to_degrees();
            let el = pos
                .y
                .atan2((pos.x * pos.x + pos.z * pos.z).sqrt())
                .to_degrees();
            (az, el, pos.length())
        };

        if let Some(panner) = graph.0.node::<tutti::units::SpatialPannerNode>(panner_id) {
            panner.set_position(azimuth, elevation);
        }

        let gain = compute_attenuation(
            distance,
            spatial.attenuation,
            spatial.ref_distance,
            spatial.max_distance,
        );
        if let Some(sampler) = graph.0.node_mut::<SamplerUnit>(emitter.node_id) {
            sampler.set_gain(gain);
        }
    }

    if edited {
        graph.0.commit();
    }
}

fn compute_attenuation(
    distance: f32,
    model: AttenuationModel,
    ref_distance: f32,
    max_distance: f32,
) -> f32 {
    if distance >= max_distance {
        return 0.0;
    }

    match model {
        AttenuationModel::InverseDistance => {
            ref_distance / (ref_distance + (distance - ref_distance).max(0.0))
        }
        AttenuationModel::Linear => 1.0 - (distance / max_distance).clamp(0.0, 1.0),
        AttenuationModel::Exponential => (distance / ref_distance).powf(-2.0).clamp(0.0, 1.0),
    }
}

/// Bevy plugin: spatial audio panning.
///
/// Depends on [`crate::playback::TuttiPlaybackPlugin`] (the spatial system uses
/// `AudioEmitter` and the `SamplerUnit` it points at). Ordered between
/// playback and cleanup.
pub struct TuttiSpatialPlugin;

impl Plugin for TuttiSpatialPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<AudioListener>()
            .register_type::<AttenuationModel>();
        app.add_systems(
            Update,
            spatial_audio_sync_system
                .after(audio_playback_system)
                .before(audio_cleanup_system),
        );
    }
}
