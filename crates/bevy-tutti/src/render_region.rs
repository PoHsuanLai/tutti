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
//!
//! ## Clip population is a downstream hole
//!
//! The cloned net's clip readers ([`TrackClipReaderUnit`]) start **empty** —
//! clips live only in the live audio-thread reader (fed by a command channel),
//! never in the staged graph this clone comes from. So the render runs in
//! three ordered steps ([`RegionRenderSystems`]): this crate clones + isolates
//! + rebinds the transport (`Prepare`), a clip-aware downstream crate fills the
//! readers from ECS (`Populate`), then this crate hands the net to the worker
//! (`Spawn`). bevy-tutti stays clip-vocabulary-free; only the middle step knows
//! what a clip is.

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;
use std::sync::Arc;

use tutti::NodeId;
use tutti::core::dsp::Net;
use tutti::core::{OfflineTransport, OfflineTransportConfig, SampleRate, TransportReader};
use tutti::sampler::SamplerUnit;

use crate::resources::{AudioConfig, TuttiGraphRes};
use crate::track_clip_reader::TrackClipReaderUnit;

/// Ordering anchor for the three-step region render. A clip-aware downstream
/// crate schedules its clip-population system in [`Self::Populate`]; this crate
/// owns the surrounding `Prepare` (clone + isolate + rebind) and `Spawn`
/// (hand the filled net to the worker) steps. The set is `.chain()`ed so the
/// empty-but-isolated net is always filled before it reaches the worker.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub enum RegionRenderSystems {
    Prepare,
    Populate,
    Spawn,
}

/// Rebind every transport-aware unit in `net` to `transport`.
///
/// Walks each node and, where it's a clip reader or a bare in-memory sampler,
/// points it at the offline transport so the cloned net plays against the
/// render's timeline rather than the (undriven) live one.
fn rebind_net_transport(net: &mut Net, transport: &Arc<dyn TransportReader>) {
    let ids: Vec<NodeId> = net.ids().copied().collect();
    let mut readers = 0;
    let mut samplers = 0;
    for id in ids {
        let unit = net.node_mut(id).as_any_mut();
        if let Some(reader) = unit.downcast_mut::<TrackClipReaderUnit>() {
            reader.rebind_transport(transport.clone());
            // The clone shares the live command `Receiver`; sever it so this
            // offline reader can't steal commands from the audio thread. Its
            // clips are filled synchronously in the `Populate` step instead.
            reader.detach_commands();
            readers += 1;
        } else if let Some(sampler) = unit.downcast_mut::<SamplerUnit>() {
            sampler.replace_transport(transport.clone());
            samplers += 1;
        }
    }
    bevy_log::debug!("[regionrender]   rebind walk: {readers} reader(s), {samplers} bare sampler(s)");
}

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

/// Carrier between [`prepare_region_render_system`] and
/// [`spawn_region_render_system`]: the isolated, transport-rebound clone of the
/// live net, parked on the request entity while the `Populate` step fills its
/// clip readers from ECS. Not `Reflect`: `Net` is foreign to `bevy_reflect`.
#[derive(Component)]
pub struct RegionRenderNet {
    net: Net,
    /// The offline transport the clip readers (and the export) are bound to.
    /// Public so the `Populate` step binds freshly-built samplers to the same
    /// timeline.
    pub transport: Arc<OfflineTransport>,
    pub target: NodeId,
    pub start_beat: f64,
    pub len_beats: f64,
    pub tempo: f64,
}

impl RegionRenderNet {
    /// Mutable access to a node in the cloned net, for the `Populate` step to
    /// downcast its clip readers and insert clips.
    pub fn node_mut(&mut self, node: NodeId) -> &mut dyn tutti::core::AudioUnit {
        self.net.node_mut(node)
    }
}

/// Step 1 (`Prepare`): clone + isolate the live net at `target`, build the
/// offline transport, rebind every transport-aware unit (and detach the clip
/// readers' command channels), then park it as a [`RegionRenderNet`]. Clip
/// population happens downstream in `Populate`; the worker spawns in `Spawn`.
pub fn prepare_region_render_system(
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

        let Some(mut net) = graph.0.clone_net_isolated(start.target) else {
            ecmd.insert(RegionRenderFailed {
                target: start.target,
                error: "target node has no outputs".into(),
            });
            continue;
        };

        // The offline transport the render advances. The clone's samplers still
        // point at the live transport (which the offline driver never drives),
        // so we rebind every transport-aware unit to this one — without it the
        // clip samplers read a stale playhead and render silence. The export is
        // handed the same transport in `Spawn`, tying both ends together.
        let timeline = Arc::new(OfflineTransport::new(&OfflineTransportConfig {
            start_beat: start.start_beat,
            tempo: start.tempo.into(),
            sample_rate: SampleRate(config.sample_rate),
            loop_range: None,
        }));
        let reader_transport: Arc<dyn TransportReader> = timeline.clone();
        rebind_net_transport(&mut net, &reader_transport);

        ecmd.insert(RegionRenderNet {
            net,
            transport: timeline,
            target: start.target,
            start_beat: start.start_beat,
            len_beats: start.len_beats,
            tempo: start.tempo,
        });
    }
}

/// Step 3 (`Spawn`): the clip readers are now populated; hand the net to the
/// offline export worker and swap [`RegionRenderNet`] for
/// [`RegionRenderInProgress`].
pub fn spawn_region_render_system(
    mut commands: Commands,
    config: Option<Res<AudioConfig>>,
    mut query: Query<(Entity, &mut RegionRenderNet), Added<RegionRenderNet>>,
) {
    let Some(config) = config else { return };

    for (entity, mut render) in query.iter_mut() {
        // Move the net out of the component (it goes to the worker by value).
        let net = std::mem::replace(&mut render.net, Net::new(0, 0));
        let timeline = render.transport.clone();

        let handle = tutti::export::Export::graph(net, config.sample_rate)
            .start_beat(render.start_beat)
            .duration_beats(render.len_beats, render.tempo)
            .transport(timeline)
            .to_buffers()
            .spawn();

        commands
            .entity(entity)
            .remove::<RegionRenderNet>()
            .insert(RegionRenderInProgress {
                handle,
                target: render.target,
                start_beat: render.start_beat,
                len_beats: render.len_beats,
                tempo: render.tempo,
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
        use RegionRenderSystems::{Populate, Prepare, Spawn};
        app.configure_sets(Update, (Prepare, Populate, Spawn).chain())
            .add_systems(
                Update,
                (
                    prepare_region_render_system.in_set(Prepare),
                    spawn_region_render_system.in_set(Spawn),
                    region_render_poll_system,
                ),
            );
        // `Populate` is intentionally left empty here — a clip-aware downstream
        // crate (dawai-spectral) fills the cloned net's readers in that slot.
    }
}
