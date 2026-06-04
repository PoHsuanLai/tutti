//! Offline graph export: `StartExport` message → file via tutti's exporter.

use bevy_app::{App, Plugin, Update};
use bevy_ecs::message::{Message, MessageReader};
use bevy_ecs::prelude::*;
use bevy_reflect::prelude::*;
use bevy_tasks::{AsyncComputeTaskPool, Task};

use tutti_core::graph::engine_ready;
use tutti_core::graph::{AudioConfig, AudioGraphRes};
use tutti_core::task::poll_task;

/// Fire-and-forget request to start an offline export.
///
/// `export_start_system` reads each `StartExport`, builds a `GraphExport`,
/// calls `.to_file(path)`, and spawns an entity carrying `ExportInProgress`
/// to track the in-flight job.
///
/// Not `Reflect`: `AudioFormat` / `Normalize` are foreign types from
/// `tutti-export`.
///
/// Configure it the idiomatic Bevy way — `Default` plus struct-update syntax —
/// rather than builder methods:
///
/// ```rust,ignore
/// commands.write_message(StartExport {
///     path: "output.wav".into(),
///     duration_seconds: Some(10.0),
///     ..default()
/// });
/// ```
#[derive(Message, Debug, Clone, Default)]
pub struct StartExport {
    pub path: std::path::PathBuf,
    pub duration_seconds: Option<f64>,
    pub duration_beats: Option<(f64, f64)>,
    pub format: Option<crate::AudioFormat>,
    pub normalization: Option<crate::Normalize>,
}

/// In-flight offline export. Holds the `AsyncComputeTaskPool` task that
/// the `export_poll_system` drains each frame, plus a crossbeam receiver
/// for the latest `(Phase, progress)` reported by the running job.
///
/// Not `Reflect`: the `Task` and the export result type are foreign to
/// `bevy_reflect`.
#[derive(Component)]
pub struct ExportInProgress {
    pub(crate) task: Task<Result<crate::Written, crate::Error>>,
    pub(crate) progress_rx: crossbeam_channel::Receiver<(crate::Phase, f32)>,
    /// Latest progress observed by the poll system, if any.
    pub last_progress: Option<(crate::Phase, f32)>,
}

#[derive(Component, Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Reflect)]
#[reflect(Component, Default)]
pub struct ExportComplete;

#[derive(Component, Debug, Clone, Reflect)]
#[reflect(Component, Clone)]
pub struct ExportFailed {
    pub error: String,
}

pub fn export_start_system(
    mut commands: Commands,
    graph: Res<AudioGraphRes>,
    config: Res<AudioConfig>,
    mut events: MessageReader<StartExport>,
) {
    for start in events.read() {
        let net = graph.0.clone_net();
        let mut builder = crate::Export::graph(net, config.sample_rate);

        if let Some(seconds) = start.duration_seconds {
            builder = builder.duration_seconds(seconds);
        }
        if let Some((beats, tempo)) = start.duration_beats {
            builder = builder.duration_beats(beats, tempo);
        }
        if let Some(format) = start.format {
            builder = builder.format(format);
        }
        if let Some(normalization) = start.normalization {
            builder = builder.normalize(normalization);
        }

        // Build the same `Run<Written>` job, then execute its blocking
        // terminal on the AsyncComputeTaskPool instead of tutti's own
        // std::thread. `run_with` forwards each `(Phase, progress)` event
        // over a crossbeam channel so the poll system can surface it.
        let run = builder.to_file(&start.path);
        let (tx, progress_rx) = crossbeam_channel::bounded::<(crate::Phase, f32)>(64);
        let task = AsyncComputeTaskPool::get().spawn(async move {
            run.run_with(move |phase, progress| {
                let _ = tx.try_send((phase, progress));
            })
        });

        bevy_log::info!("Export started: {}", start.path.display());

        commands.spawn(ExportInProgress {
            task,
            progress_rx,
            last_progress: None,
        });
    }
}

pub fn export_poll_system(
    mut commands: Commands,
    mut query: Query<(Entity, &mut ExportInProgress)>,
) {
    for (entity, mut export) in query.iter_mut() {
        // Drain any pending progress events before checking completion.
        while let Ok(p) = export.progress_rx.try_recv() {
            export.last_progress = Some(p);
        }

        match poll_task(&mut export.task) {
            Some(Ok(_written)) => {
                bevy_log::info!("Export complete (entity {entity:?})");
                commands
                    .entity(entity)
                    .remove::<ExportInProgress>()
                    .insert(ExportComplete);
            }
            Some(Err(error)) => {
                bevy_log::error!("Export failed (entity {entity:?}): {error}");
                commands
                    .entity(entity)
                    .remove::<ExportInProgress>()
                    .insert(ExportFailed {
                        error: error.to_string(),
                    });
            }
            None => {}
        }
    }
}

/// Bevy plugin: offline graph export.
pub struct TuttiExportPlugin;

impl Plugin for TuttiExportPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<ExportComplete>()
            .register_type::<ExportFailed>();
        app.add_message::<StartExport>();
        app.add_systems(
            Update,
            (
                export_start_system.run_if(engine_ready),
                export_poll_system,
            ),
        );
    }
}
