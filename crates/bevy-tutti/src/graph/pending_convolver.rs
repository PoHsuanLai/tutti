//! Deferred convolver load — bridges `bevy_asset` and entity-as-node.
//!
//! [`PendingConvolverLoad`] works like [`PendingSamplerLoad`] but for
//! convolution reverb: insert it on an effect entity with an IR file
//! path, and [`promote_pending_convolvers`] builds a
//! `StereoConvolverNode` once the asset is ready.

use bevy_asset::{AssetServer, Assets, Handle};
use bevy_ecs::prelude::*;

use crate::core::ecs::{AudioNode, NodeKind};
use crate::core::WaveAsset;
use crate::units::StereoConvolverNode;

use super::reconcile::GraphDirty;
use crate::resources::TuttiGraphRes;

/// Insert on an effect entity to request convolution reverb construction
/// once the IR file is loaded. The system handles `AssetServer::load()`
/// internally — callers only provide the path string.
#[derive(Component, Debug, Clone)]
pub struct PendingConvolverLoad {
    pub ir_path: String,
    pub mix: f32,
    wave: Option<Handle<WaveAsset>>,
}

impl PendingConvolverLoad {
    pub fn new(ir_path: impl Into<String>, mix: f32) -> Self {
        Self {
            ir_path: ir_path.into(),
            mix,
            wave: None,
        }
    }
}

/// Kicks off asset loading for pending convolvers that haven't started yet.
/// Runs each frame; idempotent — once `wave` is `Some`, it's skipped.
pub fn start_convolver_loads(
    asset_server: Option<Res<AssetServer>>,
    mut pending: Query<&mut PendingConvolverLoad>,
) {
    let Some(asset_server) = asset_server else {
        return;
    };
    for mut load in pending.iter_mut() {
        if load.wave.is_none() {
            load.wave = Some(asset_server.load::<WaveAsset>(&load.ir_path));
        }
    }
}

/// Promotes [`PendingConvolverLoad`] entities whose IR asset has finished
/// loading. Extracts samples from the `Wave`, builds a
/// `StereoConvolverNode`, adds it to the graph, and replaces the pending
/// component with `(AudioNode, NodeKind::ConvolutionReverb, WetMix)`.
pub fn promote_pending_convolvers(
    mut commands: Commands,
    audio_assets: Res<Assets<WaveAsset>>,
    graph: Option<ResMut<TuttiGraphRes>>,
    mut dirty: ResMut<GraphDirty>,
    pending: Query<(Entity, &PendingConvolverLoad)>,
) {
    let Some(mut graph) = graph else { return };

    for (entity, load) in pending.iter() {
        let Some(handle) = &load.wave else { continue };
        let Some(asset) = audio_assets.get(handle) else {
            continue;
        };
        let wave = &asset.0;
        let len = wave.len();
        if len == 0 {
            bevy_log::warn!(
                "[convolver] IR '{}' has zero length; skipping",
                load.ir_path
            );
            commands.entity(entity).remove::<PendingConvolverLoad>();
            continue;
        }

        let ir_l: Vec<f32> = (0..len).map(|i| wave.at(0, i)).collect();
        let ir_r: Vec<f32> = if wave.channels() > 1 {
            (0..len).map(|i| wave.at(1, i)).collect()
        } else {
            ir_l.clone()
        };

        let node = StereoConvolverNode::stereo(&ir_l, &ir_r, 512);
        node.set_mix(load.mix);
        let id = graph.0.add(node);
        dirty.0 = true;

        // `ConvolutionReverbNode` B7 marker rides alongside the NodeKind so the
        // convolver param reconciler can filter on `With<ConvolutionReverbNode>`.
        commands
            .entity(entity)
            .remove::<PendingConvolverLoad>()
            .insert((
                crate::core::ecs::ConvolutionReverbNode,
                AudioNode(id),
                NodeKind::ConvolutionReverb,
                crate::core::ecs::WetMix(load.mix),
            ));
    }
}
