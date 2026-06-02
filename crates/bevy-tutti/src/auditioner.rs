//! Browser file preview — ECS bridge for tutti-sampler's `Auditioner`.
//!
//! The `Auditioner` handles in-memory / streaming mode selection
//! internally. This module provides:
//! - `AuditionerRes` resource (wraps the auditioner instance)
//! - `PreviewFile` / `StopPreview` messages
//! - Systems that forward messages to the auditioner and swap the
//!   preview unit into the graph for audio output

use std::path::PathBuf;
use std::sync::Arc;

use bevy_app::{App, Plugin, Update};
use bevy_ecs::message::{Message, MessageReader};
use bevy_ecs::prelude::*;
use bevy_log::{info, warn};
use crate::sampler::Sampler;

use crate::graph::GraphDirty;
use crate::resources::TuttiGraphRes;

/// Wraps the tutti-sampler `Auditioner` as a Bevy resource.
#[derive(Resource)]
pub struct AuditionerRes(pub crate::sampler::preview::Auditioner);

/// Request to preview an audio file. The auditioner stops any current
/// preview before starting the new one.
#[derive(Message)]
pub struct PreviewFile(pub PathBuf);

/// Request to stop the current preview.
#[derive(Message)]
pub struct StopPreview;

/// Tracks the auditioner's graph node so we can swap/remove it.
#[derive(Resource, Default)]
pub struct AuditionerNode(pub Option<crate::NodeId>);

pub struct TuttiAuditionerPlugin;

impl Plugin for TuttiAuditionerPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<PreviewFile>()
            .add_message::<StopPreview>()
            .init_resource::<AuditionerNode>()
            .add_systems(Update, (handle_preview_requests, handle_stop_preview));
    }
}

/// Initialize the `AuditionerRes` from an existing `SamplerRes`.
pub fn init_auditioner(sampler: &Arc<Sampler>) -> AuditionerRes {
    AuditionerRes(sampler.auditioner())
}

fn handle_preview_requests(
    mut events: MessageReader<PreviewFile>,
    auditioner: Option<Res<AuditionerRes>>,
    mut graph: ResMut<TuttiGraphRes>,
    mut node: ResMut<AuditionerNode>,
    mut dirty: ResMut<GraphDirty>,
) {
    let Some(auditioner) = auditioner else { return };

    for event in events.read() {
        if let Some(old_id) = node.0.take() {
            if graph.0.contains(old_id) {
                graph.0.remove(old_id);
                dirty.0 = true;
            }
        }

        match auditioner.0.preview(&event.0) {
            Ok(()) => {
                if let Some(unit) = auditioner.0.in_memory_unit() {
                    let id = graph.0.add(unit);
                    graph.0.pipe_output(id);
                    node.0 = Some(id);
                    dirty.0 = true;
                } else if let Some(unit) = auditioner.0.streaming_unit() {
                    let id = graph.0.add(unit);
                    graph.0.pipe_output(id);
                    node.0 = Some(id);
                    dirty.0 = true;
                }
                info!("[auditioner] preview: {}", event.0.display());
            }
            Err(e) => {
                warn!("[auditioner] preview failed: {e}");
            }
        }
    }
}

fn handle_stop_preview(
    mut events: MessageReader<StopPreview>,
    auditioner: Option<Res<AuditionerRes>>,
    mut graph: ResMut<TuttiGraphRes>,
    mut node: ResMut<AuditionerNode>,
    mut dirty: ResMut<GraphDirty>,
) {
    let Some(auditioner) = auditioner else { return };

    for _ in events.read() {
        auditioner.0.stop();
        if let Some(old_id) = node.0.take() {
            if graph.0.contains(old_id) {
                graph.0.remove(old_id);
                dirty.0 = true;
            }
        }
    }
}
