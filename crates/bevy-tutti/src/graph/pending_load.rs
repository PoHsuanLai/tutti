//! Deferred sampler load — bridges `bevy_asset` and entity-as-node.
//!
//! [`PlayAudio`](crate::playback::PlayAudio) is a one-shot lifecycle
//! trigger: it spawns a sampler node, pipes it to output, and (optionally)
//! cleans up when the wave finishes. That's the right shape for fire-and-
//! forget SFX, but a DAW track wants a long-lived entity-as-node — one
//! that survives across plays, can be reconfigured (loop range, gain,
//! speed) via the parameter components in [`crate::core::ecs`], and is
//! eventually removed by despawning the entity.
//!
//! [`PendingSamplerLoad`] is exactly that: insert it on a fresh entity
//! together with a [`Handle<WaveAsset>`](bevy_asset::Handle) and the
//! sampler settings; once the asset finishes loading, the
//! [`promote_pending_samplers`] system swaps the pending component for
//! `AudioNode(id)` + [`NodeKind::Sampler`] + the typed parameter
//! components, just as if you had called `commands.spawn_audio_node`
//! synchronously with the wave on hand.
//!
//! The in-flight imports themselves are tracked by [`WaveImportQueue`].
//! It mirrors what dawai's `graph_sync` was doing — a `Vec` of
//! `(path, Entity, <in-flight job>)` polled each frame — but lifted up so
//! every Bevy app that wants the same shape doesn't reinvent it. Callers
//! that already have a `Handle<WaveAsset>` from `AssetServer::load(...)`
//! never need to touch the queue at all.
//!
//! The decode+peaks job runs on Bevy's [`AsyncComputeTaskPool`] (one
//! [`Task`] per import), draining via [`crate::task::poll_task`] — the
//! same one-shot non-RT convention documented in [`crate::task`]. (The
//! sampler butler/streaming thread is RT disk streaming and stays a tutti
//! thread; this queue is only the one-shot import/decode of the wave plus
//! its level-0 peaks.)
//!
//! [`AsyncComputeTaskPool`]: bevy_tasks::AsyncComputeTaskPool

use std::sync::Arc;

use bevy_asset::{Assets, Handle};
use bevy_ecs::prelude::*;
use bevy_tasks::{AsyncComputeTaskPool, Task};

use crate::core::ecs::{AudioNode, NodeKind, SamplerLooping, SamplerNode, SamplerSpeed, Volume};
use crate::core::{Wave, WaveAsset};
use crate::sampler::SamplerUnit;
use crate::task::poll_task;

use super::reconcile::GraphDirty;
use crate::resources::TuttiGraphRes;

/// Level-0 waveform peaks (256 samples per peak, min/max pairs) — the same
/// shape tutti-sampler's `PeakData` alias names. Not re-exported publicly
/// from tutti-sampler, so spelled concretely here.
type PeakData = Vec<(f32, f32)>;

/// Output of a finished wave import: the decoded wave (shared `Arc`, ready
/// to hand to [`SamplerUnit`]) and its final level-0 peaks. Matches what
/// the old `ImportHandle::progress() → ImportStatus::Complete` produced.
type WaveLoad = Result<(Arc<Wave>, PeakData), String>;

/// "When this asset is loaded, build a `SamplerUnit` and add it to the graph."
///
/// Insert on a new entity to defer sampler creation until the wave is
/// available. Once the asset resolves, [`promote_pending_samplers`]
/// constructs the unit, calls `graph.add(...)`, and replaces this
/// component with `(AudioNode(id), NodeKind::Sampler, Volume(gain),
/// SamplerSpeed(speed), SamplerLooping(looping))`.
///
/// Hosts are free to attach additional components on the same entity
/// (a track marker, a `SendTo` relationship, …). Those are preserved
/// across promotion — only [`PendingSamplerLoad`] itself is removed.
#[derive(Component, Debug, Clone)]
pub struct PendingSamplerLoad {
    pub wave: Handle<WaveAsset>,
    pub gain: f32,
    pub speed: f32,
    pub looping: bool,
}

impl PendingSamplerLoad {
    /// Default settings: unity gain, normal speed, one-shot (no looping).
    pub fn new(wave: Handle<WaveAsset>) -> Self {
        Self {
            wave,
            gain: 1.0,
            speed: 1.0,
            looping: false,
        }
    }

    pub fn gain(mut self, gain: f32) -> Self {
        self.gain = gain;
        self
    }

    pub fn speed(mut self, speed: f32) -> Self {
        self.speed = speed;
        self
    }

    pub fn looping(mut self, looping: bool) -> Self {
        self.looping = looping;
        self
    }
}

/// Tracks in-flight host-initiated wave loads — decode + level-0 peaks run
/// on the [`AsyncComputeTaskPool`] — initiated outside the Bevy asset system
/// (e.g. when the host already has a path string but wants the same
/// `WaveAsset` end state).
///
/// Each entry is `(path, Entity, Task<WaveLoad>)` — `path` is informational
/// (used for de-duplication and logging), `Entity` is where the resulting
/// `Handle<WaveAsset>` will be applied. [`poll_wave_imports`] drains the
/// queue: completed imports become `Handle<WaveAsset>` insertions on the
/// tracked entity (the entity should already carry a [`PendingSamplerLoad`]
/// or anything else that consumes a wave handle).
///
/// [`AsyncComputeTaskPool`]: bevy_tasks::AsyncComputeTaskPool
#[derive(Resource, Default)]
pub struct WaveImportQueue {
    pub imports: Vec<(String, Entity, Task<WaveLoad>)>,
}

impl WaveImportQueue {
    /// Spawn a background decode+peaks job for `path` on the
    /// [`AsyncComputeTaskPool`] and track it against `entity`. The job is
    /// the same blocking work the old `tutti-load-wave` std::thread did —
    /// [`Wave::load_with_peaks`] — minus the incremental progress/peak
    /// channels (no consumer needs streaming peaks during a host import).
    ///
    /// [`AsyncComputeTaskPool`]: bevy_tasks::AsyncComputeTaskPool
    pub fn start(&mut self, path: String, entity: Entity) {
        let load_path = path.clone();
        let task = AsyncComputeTaskPool::get().spawn(async move {
            Wave::load_with_peaks(
                &load_path,
                |_progress| {},
                |_peaks| {},
                |_total_frames, _sample_rate| {},
                |_samples| {},
            )
            .map(|(wave, peaks)| (Arc::new(wave), peaks))
            .map_err(|e| e.to_string())
        });
        self.imports.push((path, entity, task));
    }

    pub fn is_importing(&self, path: &str) -> bool {
        self.imports.iter().any(|(p, _, _)| p == path)
    }

    pub fn clear(&mut self) {
        self.imports.clear();
    }
}

impl std::fmt::Debug for WaveImportQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WaveImportQueue")
            .field("count", &self.imports.len())
            .finish()
    }
}

/// Polls every entry in [`WaveImportQueue`]. Completed imports become
/// `Assets<WaveAsset>` entries; the resulting handle is applied to the
/// entity that started the import by *replacing* its existing
/// [`PendingSamplerLoad::wave`] handle (keeping the rest of the settings
/// intact). Failed imports are logged and dropped.
///
/// Hosts that prefer the standard `AssetServer::load(path).await`-style
/// flow don't need this system; they can populate `Handle<WaveAsset>`
/// directly and rely on [`promote_pending_samplers`] alone.
pub fn poll_wave_imports(
    mut queue: ResMut<WaveImportQueue>,
    mut audio_assets: ResMut<Assets<WaveAsset>>,
    mut pending: Query<&mut PendingSamplerLoad>,
) {
    queue.imports.retain_mut(|(path, entity, task)| {
        let Some(result) = poll_task(task) else {
            return true;
        };
        match result {
            // `peaks` is discarded here exactly as the old `ImportStatus::Complete { wave, .. }`
            // arm did — `promote_pending_samplers` only needs the `Arc<Wave>` to build the unit.
            Ok((wave, _peaks)) => {
                let bevy_handle = audio_assets.add(WaveAsset(wave));
                if let Ok(mut pending_load) = pending.get_mut(*entity) {
                    pending_load.wave = bevy_handle;
                } else {
                    bevy_log::debug!(
                        "poll_wave_imports: '{}' loaded but entity {:?} no longer has PendingSamplerLoad; dropping handle",
                        path,
                        entity
                    );
                }
            }
            Err(e) => {
                bevy_log::error!("poll_wave_imports: '{}' failed: {}", path, e);
            }
        }
        false
    });
}

/// Promotes [`PendingSamplerLoad`] entities whose asset has finished
/// loading into full entity-as-node form: builds a `SamplerUnit`, adds
/// it to the graph, and inserts `(AudioNode, NodeKind::Sampler, Volume,
/// SamplerSpeed, SamplerLooping)`. Removes [`PendingSamplerLoad`].
///
/// Entities whose handle is still loading are left alone for the next
/// frame. Marks [`GraphDirty`] when at least one promotion happens so
/// the per-frame `commit_graph` flushes the additions.
pub fn promote_pending_samplers(
    mut commands: Commands,
    audio_assets: Res<Assets<WaveAsset>>,
    graph: Option<ResMut<TuttiGraphRes>>,
    mut dirty: ResMut<GraphDirty>,
    pending: Query<(Entity, &PendingSamplerLoad)>,
) {
    let Some(mut graph) = graph else { return };

    for (entity, pending_load) in pending.iter() {
        let Some(asset) = audio_assets.get(&pending_load.wave) else {
            continue;
        };
        let wave = asset.0.clone();
        let unit = SamplerUnit::with_settings(
            wave,
            pending_load.gain,
            pending_load.speed,
            pending_load.looping,
        );
        let id = graph.0.add(unit);
        dirty.0 = true;

        // The `SamplerNode` B7 marker rides alongside `NodeKind::Sampler` so
        // the kind-matching reconcilers can filter on `With<SamplerNode>`.
        commands
            .entity(entity)
            .remove::<PendingSamplerLoad>()
            .insert((
                SamplerNode,
                AudioNode(id),
                NodeKind::Sampler,
                Volume(pending_load.gain),
                SamplerSpeed(pending_load.speed),
                SamplerLooping(pending_load.looping),
            ));
    }
}
