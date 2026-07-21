//! Browser file preview — ECS bridge for tutti-sampler's `Auditioner`.
//!
//! The `Auditioner` handles in-memory / streaming mode selection
//! internally. This module provides:
//! - `PreviewFile` / `StopPreview` messages
//! - Systems that forward messages to the `Auditioner` resource and swap the
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
//! `Arc<parking_lot::Mutex>` + `Arc<atomic>`, so it derives `Clone` as a
//! cheap shared-handle copy and is held as a `Res<Auditioner>` directly.
//! The poll/stop systems and the decode task all operate on the *same*
//! underlying state, so a `stop()` issued on the main thread cancels a
//! preview started on a task. Cloning the resource for the task is the only
//! thing that crosses the thread boundary; `preview` itself takes `&self`.
//!
//! [`AsyncComputeTaskPool`]: bevy_tasks::AsyncComputeTaskPool

/// Path-backed streaming-sample asset + its loader (long files the auditioner
/// streams from disk). Bevy `Asset` glue — gated.
#[cfg(feature = "bevy")]
pub mod streaming_sample;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[cfg(feature = "bevy")]
use bevy_ecs::prelude::Resource;
use dashmap::DashMap;
use smol::channel::Sender;

use tutti_core::{AtomicF32, Linear, Ratio, Wave};

use crate::butler::{control, ButlerCommand, ChannelPlan, LruCache};
use crate::Sampler;
use crate::{SamplerUnit, StreamingSamplerUnit};

/// Reserved channel index for auditioner streaming.
/// Uses a high value to avoid collision with track channels (0, 1, 2...).
const AUDITIONER_CHANNEL: usize = usize::MAX - 1;

enum PreviewMode {
    InMemory(SamplerUnit),
    Streaming,
}

/// Low-latency file preview player.
///
/// Built via [`Sampler::auditioner`](crate::Sampler::auditioner) and held as a
/// Bevy [`Resource`]. Only one preview plays at a time — calling
/// [`preview`](Self::preview) while another file is playing stops the current
/// preview first.
///
/// # Mode selection
///
/// Preview is just free-running playback: the same tier policy the timeline
/// uses ([`crate::tiering`]) picks in-RAM vs streaming. Files at or under
/// [`crate::tiering::IN_MEMORY_SECS`] (or already in the LRU cache) play
/// **in-memory** — decoded once and played from RAM (placement `None`, so no
/// transport gate) with automatic sample-rate conversion. Longer files
/// **stream from disk** through a reserved internal channel, driven by the
/// very same butler control the timeline path uses.
///
/// Holds exactly the butler handles the auditioner drives (command channel,
/// channel plans, decode cache) plus its own playback state — no back-pointer
/// to the whole `Sampler`. All fields are `Arc`/`Sender`/atomics, so `Clone`
/// is a cheap shared-handle copy: cloning the resource into an off-thread
/// decode task operates on the *same* underlying state.
#[cfg_attr(feature = "bevy", derive(Resource))]
#[derive(Clone)]
pub struct Auditioner {
    butler_tx: Sender<ButlerCommand>,
    butler_plans: Arc<DashMap<usize, ChannelPlan>>,
    butler_cache: Arc<LruCache>,
    mode: Arc<parking_lot::Mutex<Option<PreviewMode>>>,
    current_path: Arc<parking_lot::Mutex<Option<PathBuf>>>,
    playing: Arc<AtomicBool>,
    gain: Arc<AtomicF32>,
    speed: Arc<AtomicF32>,
    session_sample_rate: f64,
}

impl Auditioner {
    pub(crate) fn new(sampler: &Sampler) -> Self {
        Self {
            butler_tx: sampler.butler_sender(),
            butler_plans: sampler.butler_plans(),
            butler_cache: sampler.butler_cache(),
            mode: Arc::new(parking_lot::Mutex::new(None)),
            current_path: Arc::new(parking_lot::Mutex::new(None)),
            playing: Arc::new(AtomicBool::new(false)),
            gain: Arc::new(AtomicF32::new(1.0)),
            speed: Arc::new(AtomicF32::new(1.0)),
            session_sample_rate: sampler.sample_rate(),
        }
    }

    /// Preview a file.
    ///
    /// Stops any current preview first. Short or already-cached files
    /// play in-memory; longer files stream from disk via butler. Tier
    /// selection uses the shared [`crate::tiering`] policy — the same one the
    /// timeline path applies.
    pub fn preview(&self, file_path: &Path) -> crate::Result<()> {
        self.stop();

        let path = file_path.to_path_buf();
        let cache = &self.butler_cache;

        if let Some(wave) = cache.get(&path) {
            self.start_in_memory(wave, &path);
        } else {
            let wave = Arc::new(
                Wave::load(file_path).map_err(|e| crate::Error::SampleNotFound(e.to_string()))?,
            );
            cache.insert(path.clone(), wave.clone());

            if crate::tiering::should_stream(wave.len(), wave.sample_rate()) {
                self.start_streaming(&path);
            } else {
                self.start_in_memory(wave, &path);
            }
        }

        Ok(())
    }

    fn start_in_memory(&self, wave: Arc<Wave>, path: &Path) {
        // placement None => free-running (no transport/beat gate).
        let mut unit = SamplerUnit::with_config(
            wave,
            crate::SamplerUnitConfig {
                gain: self.gain(),
                speed: self.speed(),
                ..Default::default()
            },
        );
        unit.set_session_sample_rate(self.session_sample_rate);
        unit.trigger();

        self.enter(PreviewMode::InMemory(unit), path);
    }

    fn start_streaming(&self, path: &Path) {
        control::stream(&self.butler_tx, AUDITIONER_CHANNEL, path.to_path_buf(), 0);

        let speed = self.speed().get();
        if speed != 1.0 {
            self.set_stream_speed(speed);
        }

        self.enter(PreviewMode::Streaming, path);
    }

    /// Set varispeed on the reserved auditioner streaming channel — the same
    /// butler control the timeline's `Sampler::set_clip_stream_speed` uses.
    fn set_stream_speed(&self, speed: f32) {
        control::set_varispeed(&self.butler_tx, AUDITIONER_CHANNEL, speed.abs(), speed < 0.0);
    }

    /// Install a new preview mode and mark playing.
    fn enter(&self, mode: PreviewMode, path: &Path) {
        *self.mode.lock() = Some(mode);
        *self.current_path.lock() = Some(path.to_path_buf());
        self.playing.store(true, Ordering::Release);
    }

    /// Stop the current preview, if any.
    pub fn stop(&self) {
        if let Some(mode) = self.mode.lock().take() {
            match mode {
                PreviewMode::InMemory(unit) => unit.stop(),
                PreviewMode::Streaming => {
                    control::stop_stream(&self.butler_tx, AUDITIONER_CHANNEL);
                }
            }
        }
        *self.current_path.lock() = None;
        self.playing.store(false, Ordering::Release);
    }

    /// `true` if a preview is currently playing.
    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Acquire)
    }

    /// Set linear playback gain. Clamped to non-negative.
    pub fn set_gain(&self, gain: Linear) {
        self.gain.store(gain.get().max(0.0), Ordering::Release);
    }

    /// Current linear playback gain.
    pub fn gain(&self) -> Linear {
        Linear::new(self.gain.load(Ordering::Acquire))
    }

    /// Set playback speed. Clamped to `[0.25, 4.0]`.
    pub fn set_speed(&self, speed: Ratio) {
        let clamped = speed.get().clamp(0.25, 4.0);
        self.speed.store(clamped, Ordering::Release);
        let mode = self.mode.lock();
        if let Some(PreviewMode::Streaming) = mode.as_ref() {
            self.set_stream_speed(clamped);
        }
    }

    /// Current playback speed.
    pub fn speed(&self) -> Ratio {
        Ratio::new(self.speed.load(Ordering::Acquire))
    }

    /// Path of the file currently being previewed, if any.
    pub fn current_path(&self) -> Option<PathBuf> {
        self.current_path.lock().clone()
    }

    /// Duration of the current preview, in seconds.
    pub fn duration(&self) -> Option<f64> {
        let mode = self.mode.lock();
        match mode.as_ref()? {
            PreviewMode::InMemory(unit) => Some(unit.duration_seconds()),
            PreviewMode::Streaming => {
                let path = self.current_path.lock();
                let path = path.as_ref()?;
                let wave = self.butler_cache.get(path)?;
                Some(wave.duration())
            }
        }
    }

    /// Clone of the in-memory `SamplerUnit` for graph integration.
    /// `None` if the current preview is streaming from disk.
    pub fn in_memory_unit(&self) -> Option<SamplerUnit> {
        let mode = self.mode.lock();
        match mode.as_ref()? {
            PreviewMode::InMemory(unit) => Some(unit.clone()),
            PreviewMode::Streaming => None,
        }
    }

    /// `StreamingSamplerUnit` for graph integration when the preview is
    /// streaming from disk. `None` if the current preview is in-memory.
    pub fn streaming_unit(&self) -> Option<StreamingSamplerUnit> {
        let mode = self.mode.lock();
        match mode.as_ref()? {
            PreviewMode::Streaming => {
                // Same 3-line consumer handoff the timeline path uses via
                // `Sampler::take_clip_reader`; here it stays un-gated (free-running).
                let (unit, _rt_state) =
                    control::take_streaming_unit(&self.butler_plans, AUDITIONER_CHANNEL)?;
                Some(unit)
            }
            PreviewMode::InMemory(_) => None,
        }
    }
}

// Bevy ECS surface — the preview/stop messages, the graph-node tracker, the
// off-thread decode plugin, and its systems. The `Auditioner` engine above is
// Bevy-free; a non-Bevy host calls `Sampler::auditioner()` and drives
// `preview`/`stop` on it directly, wiring `in_memory_unit()`/`streaming_unit()`
// into its own graph.
#[cfg(feature = "bevy")]
pub use ecs::*;

#[cfg(feature = "bevy")]
mod ecs {
    use super::*;
    use bevy_app::{App, Plugin, Update};
    use bevy_ecs::message::{Message, MessageReader};
    use bevy_ecs::prelude::*;
    use bevy_ecs::schedule::IntoScheduleConfigs;
    use bevy_log::{info, warn};
    use bevy_tasks::{block_on, futures_lite::future, AsyncComputeTaskPool, Task};
    use tutti_core::graph::{engine_ready, AudioGraphRes, GraphDirty};

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

/// Build the auditioner resource from the sampler. The engine calls this
/// during construction and inserts the returned `Auditioner` directly.
pub fn init_auditioner(sampler: &Sampler) -> Auditioner {
    sampler.auditioner()
}

/// Kick off the off-thread decode for the latest `PreviewFile` request.
///
/// Removes the previous preview node immediately (so the old sound stops
/// without waiting for the new decode), then spawns a compute task that
/// runs `Auditioner::preview`. The actual graph insertion happens in
/// `poll_preview_task` once the decode finishes.
fn handle_preview_requests(
    mut events: MessageReader<PreviewFile>,
    auditioner: Res<Auditioner>,
    mut graph: ResMut<AudioGraphRes>,
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

    let aud = auditioner.clone();
    let path = event.0.clone();
    let task_path = path.clone();
    let task = AsyncComputeTaskPool::get().spawn(async move { aud.preview(&task_path) });
    commands.insert_resource(PreviewInFlight { task, path });
}

/// Drain a finished decode task and swap the prepared unit into the graph.
fn poll_preview_task(
    auditioner: Res<Auditioner>,
    in_flight: Option<ResMut<PreviewInFlight>>,
    mut graph: ResMut<AudioGraphRes>,
    mut node: ResMut<AuditionerNode>,
    mut dirty: ResMut<GraphDirty>,
    mut commands: Commands,
) {
    let Some(mut in_flight) = in_flight else {
        return;
    };

    let Some(result) = block_on(future::poll_once(&mut in_flight.task)) else {
        return;
    };

    let path = in_flight.path.clone();
    commands.remove_resource::<PreviewInFlight>();

    match result {
        Ok(()) => {
            if let Some(unit) = auditioner.in_memory_unit() {
                let id = graph.0.add(unit);
                graph.0.pipe_output(id);
                node.0 = Some(id);
                dirty.0 = true;
            } else if let Some(unit) = auditioner.streaming_unit() {
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
    auditioner: Res<Auditioner>,
    in_flight: Option<Res<PreviewInFlight>>,
    mut graph: ResMut<AudioGraphRes>,
    mut node: ResMut<AuditionerNode>,
    mut dirty: ResMut<GraphDirty>,
    mut commands: Commands,
) {
    for _ in events.read() {
        // Cancel any pending decode so its result can't re-add a node.
        if in_flight.is_some() {
            commands.remove_resource::<PreviewInFlight>();
        }
        auditioner.stop();
        if let Some(old_id) = node.0.take() {
            if graph.0.contains(old_id) {
                graph.0.remove(old_id);
                dirty.0 = true;
            }
        }
    }
}
}
