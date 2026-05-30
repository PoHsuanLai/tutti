//! Offline render of a single graph node over a beat range.
//!
//! Spectral (and any "what does this point in the graph actually sound like"
//! consumer) needs the audio *at a tap point*, post-everything-upstream — not
//! a clip's raw source file. This module renders exactly that: spawn an entity
//! with [`StartRegionRender`] naming a `NodeId` and a beat range; the start
//! system clones the live net, repoints its output bus at that node (via
//! [`TuttiGraph::clone_net_isolated`]), and renders it offline on a worker
//! thread. When the render finishes, the result lands on the same entity as a
//! [`RegionRenderComplete`] component carrying the PCM.
//!
//! Mirrors the `StartExport` / `ExportInProgress` poll pattern in
//! [`crate::export`]; the only differences are the node isolation and the
//! `to_buffers` (in-memory) terminal instead of `to_file`.

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;
use std::sync::Arc;

use tutti::NodeId;

use crate::resources::{AudioConfig, TuttiGraphRes};

/// Trigger component: spawn an entity with this to render `target`'s output
/// over `[start_beat, start_beat + len_beats]` at `tempo`.
///
/// The start system consumes this and replaces it with
/// [`RegionRenderInProgress`]; when the worker finishes, that becomes
/// [`RegionRenderComplete`].
#[derive(Component, Debug, Clone)]
pub struct StartRegionRender {
    pub target: NodeId,
    pub start_beat: f64,
    pub len_beats: f64,
    pub tempo: f64,
}

/// In-flight region render. Holds the upstream handle the poll system drains
/// each frame. Not `Reflect`: the export `Handle` is foreign to `bevy_reflect`.
#[derive(Component)]
pub struct RegionRenderInProgress {
    handle: tutti::export::Handle<tutti::export::Rendered>,
    target: NodeId,
    start_beat: f64,
    len_beats: f64,
    tempo: f64,
}

/// Rendered PCM for a region, attached to the requesting entity on completion.
///
/// `samples_*` are the isolated target's stereo output. Consumers (spectral)
/// copy these into their own `RenderedRegion` keyed by target; the `Arc` keeps
/// the buffers cheap to share with the analysis + re-inject stages.
#[derive(Component, Debug, Clone)]
pub struct RegionRenderComplete {
    pub samples_l: Arc<[f32]>,
    pub samples_r: Arc<[f32]>,
    pub sample_rate: f64,
    pub target: NodeId,
    pub start_beat: f64,
    pub len_beats: f64,
    pub tempo: f64,
}

/// Attached instead of [`RegionRenderComplete`] when the render fails (e.g. the
/// target has no outputs, or the worker panicked).
#[derive(Component, Debug, Clone)]
pub struct RegionRenderFailed {
    pub target: NodeId,
    pub error: String,
}

pub fn start_region_render_system(
    mut commands: Commands,
    graph: Option<Res<TuttiGraphRes>>,
    config: Option<Res<AudioConfig>>,
    query: Query<(Entity, &StartRegionRender), Added<StartRegionRender>>,
) {
    let Some(graph) = graph else { return };
    let Some(config) = config else { return };

    for (entity, start) in query.iter() {
        let mut ecmd = commands.entity(entity);
        ecmd.remove::<StartRegionRender>();

        let Some(net) = graph.0.clone_net_isolated(start.target) else {
            ecmd.insert(RegionRenderFailed {
                target: start.target,
                error: "target node has no outputs".into(),
            });
            continue;
        };

        let handle = tutti::export::Export::graph(net, config.sample_rate)
            .start_beat(start.start_beat)
            .duration_beats(start.len_beats, start.tempo)
            .to_buffers()
            .spawn();

        ecmd.insert(RegionRenderInProgress {
            handle,
            target: start.target,
            start_beat: start.start_beat,
            len_beats: start.len_beats,
            tempo: start.tempo,
        });
    }
}

pub fn region_render_poll_system(
    mut commands: Commands,
    mut query: Query<(Entity, &mut RegionRenderInProgress)>,
) {
    for (entity, mut render) in query.iter_mut() {
        match render.handle.poll() {
            tutti::export::State::Done(rendered) => {
                let complete = RegionRenderComplete {
                    samples_l: Arc::from(rendered.left),
                    samples_r: Arc::from(rendered.right),
                    sample_rate: rendered.sample_rate,
                    target: render.target,
                    start_beat: render.start_beat,
                    len_beats: render.len_beats,
                    tempo: render.tempo,
                };
                commands
                    .entity(entity)
                    .remove::<RegionRenderInProgress>()
                    .insert(complete);
            }
            tutti::export::State::Failed(error) => {
                let target = render.target;
                commands
                    .entity(entity)
                    .remove::<RegionRenderInProgress>()
                    .insert(RegionRenderFailed {
                        target,
                        error: error.to_string(),
                    });
            }
            tutti::export::State::Running { .. } | tutti::export::State::Pending => {}
        }
    }
}

/// Bevy plugin: offline per-node region render.
pub struct TuttiRegionRenderPlugin;

impl Plugin for TuttiRegionRenderPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, (start_region_render_system, region_render_poll_system));
    }
}
