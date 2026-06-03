//! Browser file preview — ECS bridge for tutti-sampler's `Auditioner`.
//!
//! The `Auditioner` handles in-memory / streaming mode selection
//! internally. This module provides:
//! - `AuditionerRes` resource (wraps the auditioner instance)
//! - `PreviewFile` / `StopPreview` messages
//! - Systems that forward messages to the auditioner and swap the
//!   preview unit into the graph for audio output
//!
//! # Off-thread decode (B6)
//!
//! `Auditioner::preview(path)` does a bounded-but-blocking `Wave::load`
//! for in-memory previews (files under ~10 s). That decode must not stall
//! the Bevy main thread, so `handle_preview_requests` offloads the whole
//! `preview` call to an [`AsyncComputeTaskPool`] task (B0 convention).
//! A poll system, `poll_preview_task`, then swaps the prepared unit into
//! the graph on the main thread.
//!
//! `Auditioner` is `Send + Sync` and all of its mutable state lives behind
//! `parking_lot::Mutex` + atomics, so the resource keeps a single shared
//! `Arc<Auditioner>` (exposed as `AuditionerRes.0`). The poll/stop systems
//! and the decode task all operate on the *same* auditioner instance, so a
//! `stop()` issued on the main thread cancels a preview started on a task.
//! Cloning the `Arc` for the task is the only thing that crosses the
//! thread boundary; `preview` itself takes `&self`.
//!
//! [`AsyncComputeTaskPool`]: bevy_tasks::AsyncComputeTaskPool

use std::path::PathBuf;
use std::sync::Arc;

use bevy_app::{App, Plugin, Update};
use bevy_ecs::message::{Message, MessageReader};
use bevy_ecs::prelude::*;
use bevy_ecs::schedule::IntoScheduleConfigs;
use bevy_log::{info, warn};
use bevy_tasks::{AsyncComputeTaskPool, Task};

use tutti_core::ecs::{engine_ready, GraphDirty, TuttiGraphRes};
use tutti_core::task::poll_task;

use crate::preview::Auditioner;
use crate::Sampler;

/// Wraps the tutti-sampler `Auditioner` as a Bevy resource.
///
/// Holds an `Arc<Auditioner>` so the shared instance can be cloned into
/// off-thread decode tasks; `Auditioner` methods are still reached through
/// `.0` exactly as before (via `Deref`).
#[derive(Resource)]
pub struct AuditionerRes(pub Arc<Auditioner>);

/// Request to preview an audio file. The auditioner stops any current
/// preview before starting the new one.
#[derive(Message)]
pub struct PreviewFile(pub PathBuf);

/// Request to stop the current preview.
#[derive(Message)]
pub struct StopPreview;

/// Tracks the auditioner's graph node so we can swap/remove it.
#[derive(Resource, Default)]
pub struct AuditionerNode(pub Option<tutti_core::NodeId>);

/// In-flight off-thread `preview()` decode.
///
/// The task runs `Auditioner::preview` (including the blocking `Wave::load`
/// for in-memory previews) on the compute pool and resolves to the
/// auditioner's `preview` result. The auditioner's own mutex holds the
/// prepared unit; the poll system reads it back via
/// `in_memory_unit()`/`streaming_unit()` once the task completes.
#[derive(Resource)]
struct PreviewInFlight {
    task: Task<crate::Result<()>>,
    path: PathBuf,
}

pub struct TuttiAuditionerPlugin;

impl Plugin for TuttiAuditionerPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<PreviewFile>()
            .add_message::<StopPreview>()
            .init_resource::<AuditionerNode>()
            .add_systems(
                Update,
                (
                    handle_preview_requests,
                    poll_preview_task,
                    handle_stop_preview,
                )
                    .run_if(engine_ready),
            );
    }
}

/// Initialize the `AuditionerRes` from an existing `SamplerRes`.
pub fn init_auditioner(sampler: &Arc<Sampler>) -> AuditionerRes {
    AuditionerRes(Arc::new(sampler.auditioner()))
}

/// Kick off the off-thread decode for the latest `PreviewFile` request.
///
/// Removes the previous preview node immediately (so the old sound stops
/// without waiting for the new decode), then spawns a compute task that
/// runs `Auditioner::preview`. The actual graph insertion happens in
/// `poll_preview_task` once the decode finishes.
fn handle_preview_requests(
    mut events: MessageReader<PreviewFile>,
    auditioner: Res<AuditionerRes>,
    mut graph: ResMut<TuttiGraphRes>,
    mut node: ResMut<AuditionerNode>,
    mut dirty: ResMut<GraphDirty>,
    in_flight: Option<Res<PreviewInFlight>>,
    mut commands: Commands,
) {
    // Only the most recent request matters; a newer file supersedes any
    // queued one (and the decode task in flight).
    let Some(event) = events.read().last() else {
        return;
    };

    // Drop any in-flight decode task; its result would be stale.
    if in_flight.is_some() {
        commands.remove_resource::<PreviewInFlight>();
    }

    if let Some(old_id) = node.0.take() {
        if graph.0.contains(old_id) {
            graph.0.remove(old_id);
            dirty.0 = true;
        }
    }

    let aud = Arc::clone(&auditioner.0);
    let path = event.0.clone();
    let task_path = path.clone();
    let task = AsyncComputeTaskPool::get().spawn(async move { aud.preview(&task_path) });
    commands.insert_resource(PreviewInFlight { task, path });
}

/// Drain a finished decode task and swap the prepared unit into the graph.
fn poll_preview_task(
    auditioner: Res<AuditionerRes>,
    in_flight: Option<ResMut<PreviewInFlight>>,
    mut graph: ResMut<TuttiGraphRes>,
    mut node: ResMut<AuditionerNode>,
    mut dirty: ResMut<GraphDirty>,
    mut commands: Commands,
) {
    let Some(mut in_flight) = in_flight else {
        return;
    };

    let Some(result) = poll_task(&mut in_flight.task) else {
        return;
    };

    let path = in_flight.path.clone();
    commands.remove_resource::<PreviewInFlight>();

    match result {
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
            info!("[auditioner] preview: {}", path.display());
        }
        Err(e) => {
            warn!("[auditioner] preview failed: {e}");
        }
    }
}

fn handle_stop_preview(
    mut events: MessageReader<StopPreview>,
    auditioner: Res<AuditionerRes>,
    in_flight: Option<Res<PreviewInFlight>>,
    mut graph: ResMut<TuttiGraphRes>,
    mut node: ResMut<AuditionerNode>,
    mut dirty: ResMut<GraphDirty>,
    mut commands: Commands,
) {
    for _ in events.read() {
        // Cancel any pending decode so its result can't re-add a node.
        if in_flight.is_some() {
            commands.remove_resource::<PreviewInFlight>();
        }
        auditioner.0.stop();
        if let Some(old_id) = node.0.take() {
            if graph.0.contains(old_id) {
                graph.0.remove(old_id);
                dirty.0 = true;
            }
        }
    }
}
