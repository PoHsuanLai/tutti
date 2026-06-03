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

use tutti_core::NodeId;

use tutti_core::ecs::{engine_ready, AudioEmitter, GraphDirty, GraphReconcileSystems, TuttiGraphRes, Volume};

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
#[allow(clippy::type_complexity, reason = "Bevy queries are tuple-shaped by design")]
pub fn spatial_audio_sync_system(
    mut graph: ResMut<TuttiGraphRes>,
    mut dirty: ResMut<GraphDirty>,
    listener_query: Query<&bevy_transform::components::GlobalTransform, With<AudioListener>>,
    mut emitter_query: Query<(
        &bevy_transform::components::GlobalTransform,
        &AudioEmitter,
        &mut SpatialAudio,
        Option<&mut Volume>,
    )>,
) {
    let listener_tf = listener_query.single().ok();

    let mut edited = false;

    for (emitter_tf, emitter, mut spatial, maybe_volume) in emitter_query.iter_mut() {
        if spatial.panner_node_id.is_none() {
            let emitter_node = emitter.node_id;
            let Ok(panner) = crate::SpatialPannerNode::stereo() else {
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

        if let Some(panner) = graph.0.node::<crate::SpatialPannerNode>(panner_id) {
            panner.set_position(azimuth, elevation);
        }

        let gain = compute_attenuation(
            distance,
            spatial.attenuation,
            spatial.ref_distance,
            spatial.max_distance,
        );
        // Distance attenuation is applied through the generic `Volume` param,
        // not by reaching into the concrete emitter unit. The emitter's own
        // leaf reconciler (e.g. `reconcile_sampler_volume`) sees the
        // `Changed<Volume>` and writes the gain into its unit — identical end
        // behavior, and tutti-units stays decoupled from tutti-sampler.
        if let Some(mut volume) = maybe_volume {
            if (volume.0 - gain).abs() > f32::EPSILON {
                volume.0 = gain;
            }
        }
    }

    // Stage only; the Commit-phase `commit_graph` coalesces (this system is
    // anchored before that phase).
    if edited {
        dirty.0 = true;
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
/// Expects an emitter (`AudioEmitter`) to already exist on each `SpatialAudio`
/// entity — supplied by whatever leaf produces the source node (e.g. the
/// sampler's playback plugin). The sync system runs in
/// [`GraphReconcileSystems::Params`] so it lands after the spawn phase (the
/// node exists) and before the commit phase. Distance attenuation is written
/// through the `Volume` param; the emitter's own leaf reconciler applies it.
pub struct TuttiSpatialPlugin;

impl Plugin for TuttiSpatialPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<AudioListener>()
            .register_type::<AttenuationModel>();
        app.add_systems(
            Update,
            spatial_audio_sync_system
                .in_set(GraphReconcileSystems::Params)
                .run_if(engine_ready),
        );
    }
}
