//! Offline graph export: `StartExport` trigger → file via tutti's exporter.

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;
use bevy_reflect::prelude::*;

use crate::resources::{AudioConfig, TuttiGraphRes};

/// Trigger component: spawn an entity with this to start an offline export.
///
/// The `export_start_system` processes entities with `Added<StartExport>`,
/// builds a `GraphExport`, calls `.to_file(path).spawn()`, and replaces
/// this component with `ExportInProgress`.
///
/// Not `Reflect`: `AudioFormat` / `Normalize` are foreign types from
/// `tutti-export`.
#[derive(Component, Debug, Clone)]
pub struct StartExport {
    pub path: std::path::PathBuf,
    pub duration_seconds: Option<f64>,
    pub duration_beats: Option<(f64, f64)>,
    pub format: Option<tutti::export::AudioFormat>,
    pub normalization: Option<tutti::export::Normalize>,
}

impl StartExport {
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self {
            path: path.into(),
            duration_seconds: None,
            duration_beats: None,
            format: None,
            normalization: None,
        }
    }

    pub fn duration_seconds(mut self, seconds: f64) -> Self {
        self.duration_seconds = Some(seconds);
        self
    }

    pub fn duration_beats(mut self, beats: f64, tempo: f64) -> Self {
        self.duration_beats = Some((beats, tempo));
        self
    }

    pub fn format(mut self, format: tutti::export::AudioFormat) -> Self {
        self.format = Some(format);
        self
    }

    pub fn normalization(mut self, mode: tutti::export::Normalize) -> Self {
        self.normalization = Some(mode);
        self
    }
}

/// In-flight offline export. Holds the upstream handle that the
/// `export_poll_system` polls each frame.
///
/// Not `Reflect`: the export `Handle` is foreign to `bevy_reflect`.
#[derive(Component)]
pub struct ExportInProgress {
    pub(crate) handle: tutti::export::Handle<tutti::export::Written>,
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
    graph: Option<Res<TuttiGraphRes>>,
    config: Option<Res<AudioConfig>>,
    query: Query<(Entity, &StartExport), Added<StartExport>>,
) {
    let Some(graph) = graph else { return };
    let Some(config) = config else { return };

    for (entity, start) in query.iter() {
        let net = graph.0.clone_net();
        let mut builder = tutti::export::Export::graph(net, config.sample_rate);

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

        let handle = builder.to_file(&start.path).spawn();

        bevy_log::info!("Export started: {}", start.path.display());

        commands
            .entity(entity)
            .remove::<StartExport>()
            .insert(ExportInProgress { handle });
    }
}

pub fn export_poll_system(
    mut commands: Commands,
    mut query: Query<(Entity, &mut ExportInProgress)>,
) {
    for (entity, mut export) in query.iter_mut() {
        match export.handle.poll() {
            tutti::export::State::Done(_written) => {
                bevy_log::info!("Export complete (entity {entity:?})");
                commands
                    .entity(entity)
                    .remove::<ExportInProgress>()
                    .insert(ExportComplete);
            }
            tutti::export::State::Failed(error) => {
                bevy_log::error!("Export failed (entity {entity:?}): {error}");
                commands
                    .entity(entity)
                    .remove::<ExportInProgress>()
                    .insert(ExportFailed {
                        error: error.to_string(),
                    });
            }
            tutti::export::State::Running { .. } | tutti::export::State::Pending => {}
        }
    }
}

/// Bevy plugin: offline graph export.
pub struct TuttiExportPlugin;

impl Plugin for TuttiExportPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<ExportComplete>()
            .register_type::<ExportFailed>();
        app.add_systems(Update, (export_start_system, export_poll_system));
    }
}
