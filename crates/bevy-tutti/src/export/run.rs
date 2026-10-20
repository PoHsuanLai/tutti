//! Start renders, and report them when they finish.
//!
//! Two systems and no policy. [`start_exports`] turns each new
//! [`ExportRequest`] into the session's (its source entity resolved to its
//! node, its `prepare` hook's look at the world taken) and starts it
//! (`Session::export`: the fork gathered on the main thread, the plugins
//! told `Offline`), then hands the job (building the fork, the hook's edit,
//! the render) to a task on the shared compute pool; [`poll_exports`]
//! drives those tasks, finishes each on the main thread
//! (`Session::finish_export`: what the render held dropped there, the
//! plugins told `Realtime`) and triggers [`ExportDone`].
//!
//! The render itself is synchronous and `Send` (tutti-export spawns no threads
//! by design — "a host that wants a render off the main thread already owns a
//! task pool"). So this module is the *whole* of what Bevy adds: a place to run
//! it, and a way to hear about it.

use std::collections::HashMap;

use bevy_ecs::prelude::*;
use bevy_tasks::AsyncComputeTaskPool;

use tutti_types::NodeKey;

use crate::export::request::{
    ExportDone, ExportError, ExportInFlight, ExportNode, ExportRequest, ExportSource,
};
use crate::graph::{AudioConfig, AudioGraphRes};

/// Starts the oldest pending [`ExportRequest`], if nothing is already running.
///
/// Here, on the main thread, the session gathers the fork
/// (`Session::export`): everything it reads of the live graph and its nodes
/// is read now, because the fork is the graph at the moment the export
/// starts (a later live edit does not reach it). A fork source that must
/// build on the main thread (an in-process VST2 plugin) builds here too.
/// The slow part of a hosted plugin's fork (a `plugin-server` launch and
/// its state load) is the job's: it builds the fork on the compute pool,
/// then runs the hook's edit and the render, so this frame does not wait
/// for it. Only one export starts per frame; see [`ExportInFlight`].
///
/// Exclusive because a request's `prepare` hook is handed `&World`: preparing a
/// render reads arbitrary app state (which clips exist, their decoded audio),
/// and no fixed `SystemParam` list here could anticipate what a host needs to
/// read. The work is main-thread-bound regardless.
pub fn start_exports(world: &mut World) {
    // One render at a time, enforced here rather than left to callers.
    //
    // The obvious alternative — telling callers to gate their spawn system on
    // `not(any_with_component::<ExportInFlight>)` — cannot work: a run
    // condition sees only the state left by the previous frame, so N requests
    // spawned in one frame all pass the gate and all start together. That is
    // the burst the limit exists to prevent, so the check belongs where the
    // starting happens. The session's own token is asked too: a job a host
    // started on the session directly, or one cancelled whose task is still
    // finishing its render, holds it.
    if world
        .query_filtered::<Entity, With<ExportInFlight>>()
        .iter(world)
        .next()
        .is_some()
        || world
            .get_resource::<AudioGraphRes>()
            .is_some_and(|graph| graph.read().export_in_flight())
    {
        return;
    }

    // Exactly one request per frame — `next()`, not a loop. This is the half
    // that serializes a *batch* spawned in one frame; the check above is the
    // half that stops the next frame starting on top of a still-running render.
    // Each start copies the live graph on this thread, so neither half is
    // redundant.
    //
    // No `Added<>` — the component's *presence* is the pending flag, and taking
    // it below is what marks a request started. One spawned while the engine was
    // down must still be picked up on a later frame.
    let Some(entity) = world
        .query_filtered::<Entity, (With<ExportRequest>, Without<ExportInFlight>)>()
        .iter(world)
        .next()
    else {
        return;
    };

    // Take the request out; from here the entity is either in flight or has
    // reported a failure, never still pending.
    let Some(request) = world.entity_mut(entity).take::<ExportRequest>() else {
        return;
    };

    // Snapshot the node bindings first: the source entity resolves to its
    // node here, and a failure names its node by entity and `Name`. An
    // exclusive system cannot hold a `Query` across the resource borrows
    // below.
    let nodes = NodeNames::of(world);

    // The hook's look at the world, now, in the frame the export starts and
    // with the world still readable; its edit runs on the built fork. An
    // isolated copy is born empty, so a sampler-fed tap that skips this
    // renders silence.
    let edit = request.prepare.as_ref().map(|prepare| prepare(world));

    let started = start(world, &nodes, request, edit);
    let job = match started {
        Ok(job) => job,
        Err(e) => {
            world.trigger(ExportDone {
                entity,
                result: Err(e),
            });
            return;
        }
    };
    let task = AsyncComputeTaskPool::get().spawn(async move { job.run() });
    world
        .entity_mut(entity)
        .insert(ExportInFlight::new(task, nodes));
}

/// The session's request for `request`, started.
fn start(
    world: &mut World,
    nodes: &NodeNames,
    request: ExportRequest,
    edit: Option<tutti_session::PrepareEdit>,
) -> Result<tutti_session::ExportJob, ExportError> {
    // Fallible rather than `world.resource::<_>()`: both come from
    // `engine::build_into`, while the `engine_ready` gate on this system only
    // reads `AudioEngineState` — a value a host can insert alone. An export
    // with no engine to render from is a reportable failure, and distinct
    // from a target that cannot produce audio.
    if world.get_resource::<AudioConfig>().is_none() {
        return Err(invalid(NO_ENGINE).into());
    }
    let Some(mut graph) = world.get_resource_mut::<AudioGraphRes>() else {
        return Err(invalid(NO_ENGINE).into());
    };
    let source = match request.source {
        ExportSource::Master => tutti_session::ExportSource::Mix,
        // Resolved here rather than stored: see `ExportSource::Node`.
        ExportSource::Node(entity) => tutti_session::ExportSource::Node(
            nodes.node(entity).ok_or_else(|| invalid(NOT_A_NODE))?,
        ),
    };
    let mut session_request =
        tutti_session::ExportRequest::new(source, request.target, request.config, request.clock);
    session_request.latency_from_graph = request.latency_from_graph;
    session_request.tail_from_graph = request.tail_from_graph;
    if let Some(edit) = edit {
        session_request = session_request.with_edit(edit);
    }
    graph
        .session()
        .export(session_request)
        .map_err(|e| nodes.session_error(e))
}

/// Why a node export found nothing to render: the entity is not bound to a
/// graph node.
const NOT_A_NODE: &str = "export target entity is not a graph node";

/// Why an export could not start: the world has no audio engine.
const NO_ENGINE: &str = "no audio engine to export from";

fn invalid(reason: impl Into<String>) -> tutti_export::Error {
    tutti_export::Error::InvalidConfig(reason.into())
}

/// Every entity bound to a graph node, by entity and by node key — what
/// resolves an [`ExportSource::Node`] to its node, and what names a node an
/// export failed on. Taken on the main thread when the render starts, and
/// kept with the render ([`ExportInFlight`]): a fork can fail mid-render,
/// long after the world was last readable.
#[derive(Default)]
pub(crate) struct NodeNames {
    nodes: HashMap<Entity, tutti_core::AudioNode>,
    by_key: HashMap<NodeKey, (Entity, Option<String>)>,
}

impl NodeNames {
    fn of(world: &mut World) -> Self {
        let mut nodes = HashMap::new();
        let mut by_key = HashMap::new();
        for (entity, node, name) in world
            .query::<(Entity, &tutti_core::AudioNode, Option<&Name>)>()
            .iter(world)
        {
            nodes.insert(entity, *node);
            by_key.insert(node.key(), (entity, name.map(|n| n.as_str().to_owned())));
        }
        Self { nodes, by_key }
    }

    fn node(&self, entity: Entity) -> Option<tutti_core::AudioNode> {
        self.nodes.get(&entity).copied()
    }

    fn export_node(&self, key: NodeKey) -> ExportNode {
        let (entity, name) = self
            .by_key
            .get(&key)
            .map_or((None, None), |(e, n)| (Some(*e), n.clone()));
        ExportNode { entity, name, key }
    }

    /// `e`, with the node it is about named by entity and `Name`.
    fn name(&self, e: tutti_export::Error) -> ExportError {
        match e {
            tutti_export::Error::NotForkable { key } => ExportError::NotForkable {
                node: self.export_node(key),
            },
            tutti_export::Error::Fork(tutti_graph::ForkError::Source { key, cause }) => {
                ExportError::ForkSource {
                    node: self.export_node(key),
                    cause,
                }
            }
            tutti_export::Error::ForkFailed { key, kind, cause } => ExportError::ForkFailed {
                node: self.export_node(key),
                kind,
                cause,
            },
            other => ExportError::Render(other),
        }
    }

    /// A session's export error as the adapter reports it: the renderer's
    /// own named as [`name`](Self::name) names it, anything else (another
    /// export in flight) as an invalid request, with its message.
    fn session_error(&self, e: tutti_session::Error) -> ExportError {
        match e {
            tutti_session::Error::Export(e) => self.name(e),
            other => ExportError::Render(invalid(other.to_string())),
        }
    }
}

/// Drives in-flight exports; finishes the ones whose job returned
/// (`Session::finish_export`) and triggers [`ExportDone`] on them.
///
/// **Pinned to the main thread** ([`NonSendMarker`](bevy_ecs::system::NonSendMarker)):
/// `finish_export` drops the rendered graph (or, failed, whatever of it was
/// built) there. A forked in-process VST2 plugin closes its instance when
/// dropped, which VST2 confines to the main thread.
///
/// Ungated on the engine: a render still in flight when the engine tears
/// down is still reported. With no graph to finish it on, it reports a
/// failure, and what it held drops here.
pub fn poll_exports(
    mut commands: Commands,
    mut in_flight: Query<(Entity, &mut ExportInFlight)>,
    mut graph: Option<ResMut<AudioGraphRes>>,
    _main: bevy_ecs::system::NonSendMarker,
) {
    // What a cancelled export held, sent back as its task went: dropped
    // here, on the main thread, every frame (the commit's `Phase::Collect`
    // does the same, but an app may export without the reconcile systems).
    if let Some(graph) = graph.as_mut() {
        graph
            .bypass_change_detection()
            .session()
            .drop_export_leftovers();
    }
    for (entity, mut export) in in_flight.iter_mut() {
        let Some((rendered, names)) = export.poll() else {
            continue;
        };
        let result = match graph.as_mut() {
            Some(graph) => graph
                .bypass_change_detection()
                .session()
                .finish_export(rendered)
                .map_err(|e| names.session_error(e)),
            None => {
                drop(rendered);
                Err(invalid(NO_ENGINE).into())
            }
        };
        commands
            .entity(entity)
            .remove::<ExportInFlight>()
            .trigger(move |entity: Entity| ExportDone { entity, result });
    }
}

/// Exports from an engine as a host builds one (`build_on`, the device-free
/// `build_into`), over a manual stream: the metronome in the graph, wired to
/// the master, and a host node that walks each block's beat. Every other
/// export test builds a bare `headless` graph, which has neither — and on
/// the graph an engine-built node (the beat clock, since deleted) was once
/// inserted with no fork source, so every engine-built graph refused a
/// master export as not forkable.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::build::build_on;
    use crate::export::{ExportClock, ExportOutput, ExportPlugin, ExportTarget};
    use crate::graph::{
        EngineNodes, GraphReconcilePlugin, MasterSources, MetronomeRes, SpawnAudioNode,
        TransportRes,
    };
    use crate::{AudioEngineState, TuttiPlugin};
    use bevy_app::App;
    use std::sync::{Arc, Mutex};
    use tutti_core::transport::{MetronomeMode, OfflineTimeline, OfflineTimelineConfig};
    use tutti_core::{Beat, Bpm, ChannelLayout, MotionEvent, SampleRate};
    use tutti_cpal::{ManualStreamDriver, OutputSpec};
    use tutti_export::{ExportConfig, RenderConfig};

    const RATE: f64 = 48_000.0;

    /// Each frame's beat from its block's `Env` (`Env::for_each_beat`): whole
    /// beats on output 0, the fraction on output 1. A host's node, forked by
    /// clone.
    #[derive(Clone)]
    struct BeatWalk;

    impl tutti_graph::Node for BeatWalk {
        fn shape(&self) -> tutti_graph::Shape {
            tutti_graph::Shape::audio(ChannelLayout::EMPTY, ChannelLayout::STEREO)
                .with_tail(tutti_core::Tail::Unbounded)
        }
        fn prepare(&mut self, _: &tutti_graph::Prepare) {}
        fn process(
            &mut self,
            cx: &tutti_graph::Cx<'_>,
            mut io: tutti_graph::Io<'_>,
        ) -> tutti_graph::Status {
            cx.env.for_each_beat(|i, beat| {
                io.output(0)[i] = beat.floor().get() as f32;
                io.output(1)[i] = beat.fract().get() as f32;
            });
            tutti_graph::Status::Modified
        }
        fn reset(&mut self) {}
    }

    /// An engine at 48 kHz, the metronome always on and wired to the master,
    /// a [`BeatWalk`] beside it, the live transport rolling. Returns the app
    /// and the click's and the walker's entities.
    fn engine_app() -> (App, Entity, Entity, tutti_cpal::ManualStream) {
        let mut app = App::new();
        let plugin = TuttiPlugin::default();
        let (driver, stream) = ManualStreamDriver::new();
        build_on(&plugin, &mut app, |builder| {
            builder.build_device_on(
                OutputSpec::new(
                    SampleRate(RATE),
                    ChannelLayout::STEREO,
                    tutti_cpal::cpal::SampleFormat::F32,
                ),
                driver,
            )
        })
        .expect("builds with no device");
        app.insert_resource(AudioEngineState::Running);
        app.add_plugins((
            bevy_app::TaskPoolPlugin::default(),
            GraphReconcilePlugin,
            ExportPlugin,
        ));
        let click = app.world().resource::<EngineNodes>().click;
        let walker = app
            .world_mut()
            .commands()
            .spawn_audio_node(tutti_graph::ForkByClone(BeatWalk))
            .id();
        app.world_mut().flush();
        app.insert_resource(MasterSources::from(click));
        app.world()
            .resource::<MetronomeRes>()
            .0
            .set_mode(MetronomeMode::Always);
        let _ = app
            .world()
            .resource::<TransportRes>()
            .motion
            .try_send(MotionEvent::Play);
        for _ in 0..4 {
            app.update();
            stream.render_block(256).expect("the stream is open");
        }
        (app, click, walker, stream)
    }

    /// A host node that records the thread it is dropped on: a stand-in for
    /// a forked in-process VST2 plugin, whose drop closes its instance and
    /// belongs on the main thread. Forked by clone, so the export's copy
    /// shares the record.
    #[derive(Clone)]
    struct DropWitness(Arc<Mutex<Vec<std::thread::ThreadId>>>);

    impl Drop for DropWitness {
        fn drop(&mut self) {
            self.0
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(std::thread::current().id());
        }
    }

    impl tutti_graph::Node for DropWitness {
        fn shape(&self) -> tutti_graph::Shape {
            tutti_graph::Shape::audio(ChannelLayout::EMPTY, ChannelLayout::STEREO)
        }
        fn prepare(&mut self, _: &tutti_graph::Prepare) {}
        fn process(
            &mut self,
            _: &tutti_graph::Cx<'_>,
            _: tutti_graph::Io<'_>,
        ) -> tutti_graph::Status {
            tutti_graph::Status::Silent
        }
        fn reset(&mut self) {}
    }

    /// **The rendered graph is dropped on the main thread**, though it
    /// renders on the compute pool: the job returns it, and `poll_exports`
    /// (main-thread-pinned) drops it there (`Session::finish_export`). A
    /// forked in-process VST2 plugin closes its instance when dropped, which
    /// VST2 confines to the main thread.
    ///
    /// **It bites only with a threaded pool**, so CI runs it threaded: the
    /// `dark features` job (and `just test-features`) runs it, with
    /// `export_fork.rs`'s `off_thread` tests, under
    /// `--features full,bevy_tasks/multi_threaded`. This crate's own feature
    /// set leaves `multi_threaded` off (an app with Bevy's default features
    /// turns it on), and then a task runs inside `spawn`, on the main thread,
    /// so in an ordinary run a pool-side drop is a main-thread drop too and
    /// this passes either way. (Turning the feature on for every test was
    /// tried: `export_surface`'s
    /// `a_batch_spawned_in_one_frame_starts_one_at_a_time` assumes a render
    /// finishes within the frame that polls it, which a threaded pool does
    /// not promise.)
    ///
    /// Mutation (run, with `bevy_tasks/multi_threaded`): `ExportJob::run`
    /// dropping the rendered graph before it returns (`graph.take()`
    /// dropped on the worker) → the copy's drop is recorded on a pool thread
    /// → fails. (The session's `export_leftovers_drop_on_the_main_thread`
    /// pins the same over a plain thread, with no Bevy.)
    #[test]
    fn the_rendered_graph_is_dropped_on_the_main_thread() {
        let (mut app, _click, _walker, _stream) = engine_app();
        let drops: Arc<Mutex<Vec<std::thread::ThreadId>>> = Arc::default();
        let witness = app
            .world_mut()
            .commands()
            .spawn_audio_node(tutti_graph::ForkByClone(DropWitness(Arc::clone(&drops))))
            .id();
        app.world_mut().flush();
        for _ in 0..2 {
            app.update();
        }
        drops.lock().unwrap().clear();
        let config = ExportConfig {
            render: RenderConfig {
                sample_rate: SampleRate(RATE),
                duration_seconds: 0.1,
                ..Default::default()
            },
            ..Default::default()
        };
        let clock = ExportClock::timeline(Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
            sample_rate: SampleRate(RATE),
            ..Default::default()
        })));
        let _ = export(
            &mut app,
            ExportRequest::new(
                ExportSource::Node(witness),
                ExportTarget::Buffers,
                config,
                clock,
            ),
        );
        let main = std::thread::current().id();
        let drops = drops.lock().unwrap().clone();
        assert!(!drops.is_empty(), "the export's copy was never dropped");
        assert!(
            drops.iter().all(|&t| t == main),
            "a copy was dropped off the main thread: {drops:?} (main {main:?})"
        );
    }

    /// Spawn `request`, tick until it reports, return the left channel.
    fn export(app: &mut App, request: ExportRequest) -> Vec<f32> {
        let slot: Arc<Mutex<Option<Result<Vec<f32>, String>>>> = Arc::default();
        let seen = Arc::clone(&slot);
        app.world_mut()
            .spawn(request)
            .observe(move |done: On<ExportDone>| {
                *seen.lock().unwrap() = Some(match &done.result {
                    Ok(ExportOutput::Buffers(r)) => Ok(r.planes[0].clone()),
                    Ok(other) => Err(format!("{other:?}")),
                    Err(e) => Err(e.to_string()),
                });
            });
        for _ in 0..4000 {
            app.update();
            if let Some(got) = slot.lock().unwrap().take() {
                return got.unwrap_or_else(|e| panic!("the export failed: {e}"));
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        panic!("the export never reported");
    }

    /// **An engine-built graph exports — the master, the click, and a node
    /// beside them — and a forked node's beat is the render's.**
    /// The render's timeline is 120 BPM from beat 0.25 at 48 kHz, so the
    /// walker's whole-beat port (the export's left channel; a node export
    /// clamps the right channel onto its last port, the fraction) reads 0
    /// until frame 18 000, 1 until 42 000, then 2 — to a frame: the walk
    /// accumulates `beats_per_sample` (1/24 000 of a beat, which binary does
    /// not hold) frame by frame, and lands on beat 2 one frame late (a
    /// known offline-timeline rounding). The live
    /// transport, rolling since the build, is at another beat: a forked
    /// node that read it would not step there: the fork reads the render's
    /// `Env`.
    ///
    /// What the click itself renders is not asserted. The fork is cloned from
    /// the click's shadow, taken at insert, whose metronome mode is the one it
    /// had then (`Off` here):
    /// `MetronomeRes` writes the live node's settings cell, which the shadow
    /// detached from: metronome volume and mode are live-only, and a click is
    /// not part of an export. Its session flags are frozen at the
    /// fork from the live transport, and tutti-core's
    /// `isolate_snapshots_settings_and_session_flags` row pins that.
    ///
    /// Mutation (run): the export rendering its fork under a stopped
    /// transport (`GraphSource::fill` through `FrozenClock`) → the walker
    /// reads beat 0 throughout → fails.
    #[test]
    fn an_engine_built_graph_exports_and_its_beat_is_the_renders() {
        let (mut app, click, walker, _stream) = engine_app();
        let timeline = || {
            ExportClock::timeline(Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
                start_beat: Beat(0.25),
                tempo: Bpm(120.0),
                sample_rate: SampleRate(RATE),
                loop_range: None,
            })))
        };
        let config = ExportConfig {
            render: RenderConfig {
                sample_rate: SampleRate(RATE),
                duration_seconds: 1.0,
                ..Default::default()
            },
            ..Default::default()
        };
        for source in [ExportSource::Master, ExportSource::Node(click)] {
            let left = export(
                &mut app,
                ExportRequest::new(source, ExportTarget::Buffers, config, timeline()),
            );
            assert_eq!(left.len(), RATE as usize, "{source:?}");
        }
        let beats = export(
            &mut app,
            ExportRequest::new(
                ExportSource::Node(walker),
                ExportTarget::Buffers,
                config,
                timeline(),
            ),
        );
        let steps: Vec<(usize, f32)> = std::iter::once((0, beats[0]))
            .chain(
                beats
                    .windows(2)
                    .enumerate()
                    .filter(|(_, w)| w[0] != w[1])
                    .map(|(i, w)| (i + 1, w[1])),
            )
            .collect();
        let want = [(0, 0.0), (18_000, 1.0), (42_000, 2.0)];
        assert!(
            steps.len() == want.len()
                && steps
                    .iter()
                    .zip(want)
                    .all(|(&(at, beat), (want_at, want_beat))| {
                        beat == want_beat && at.abs_diff(want_at) <= 1
                    }),
            "the walker's whole beats on the render's timeline: {steps:?}, want {want:?}"
        );
    }
}
