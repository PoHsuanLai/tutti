//! Start renders, and report them when they finish.
//!
//! Two systems and no policy. [`start_exports`] gathers each new
//! [`ExportRequest`]'s fork on the main thread and hands the rest (building
//! the fork, the hook's edit, the render) to a task on the shared compute
//! pool; [`poll_exports`] drives those tasks, drops on the main thread what
//! they hand back ([`ExportDrops`]), and triggers [`ExportDone`].
//!
//! The render itself is synchronous and `Send` (tutti-export spawns no threads
//! by design — "a host that wants a render off the main thread already owns a
//! task pool"). So this module is the *whole* of what Bevy adds: a place to run
//! it, and a way to hear about it.

use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Mutex, PoisonError};

use bevy_ecs::prelude::*;
use bevy_tasks::AsyncComputeTaskPool;

use tutti_core::transport::OfflineTransport;
use tutti_export::{render_normalized_to_file, render_to_buffers, render_to_file};
use tutti_types::NodeKey;

use crate::export::request::{
    ExportDone, ExportError, ExportInFlight, ExportNode, ExportOutput, ExportRequest, ExportSource,
    ExportTarget, PreparedGraph,
};
use crate::graph::resources::ExportRefused;
use crate::graph::{AudioConfig, AudioGraphRes};
use tutti_session::PendingExport;

/// Starts the oldest pending [`ExportRequest`], if nothing is already running.
///
/// Here, on the main thread, the fork is **gathered** (`Editor::fork_gather`):
/// everything it reads of the live graph and its nodes is read now, because
/// the graph resource cannot cross into a `'static` task and because the fork
/// is the graph at the moment the export starts (a later live edit does not
/// reach it). A fork source that must build on the main thread (an
/// in-process VST2 plugin) builds here too. The slow part of a hosted
/// plugin's fork (a `plugin-server` launch and its state load) is the
/// task's: it builds the fork on the compute pool, then runs the hook's edit
/// and the render, so this frame does not wait for it. Only one export starts
/// per frame; see [`ExportInFlight`].
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
    // starting happens.
    if world
        .query_filtered::<Entity, With<ExportInFlight>>()
        .iter(world)
        .next()
        .is_some()
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

    // Snapshot the node bindings before the resource borrows: the source
    // entity resolves to its node here, and a failure names its node by
    // entity and `Name`. An exclusive system cannot hold a `Query` across the
    // `world.resource` borrows below.
    let nodes = NodeNames::of(world);

    // Fallible rather than `world.resource::<_>()`: both come from
    // `engine::build_into`, while the `engine_ready` gate on this system only
    // reads `AudioEngineState` — a value a host can insert alone. An export with
    // no engine to render from is a reportable failure, and this request is
    // already holding the channel to report it on, so it does not silently
    // vanish the way an early `return` would.
    let prepared = match (
        world.get_resource::<AudioGraphRes>(),
        world.get_resource::<AudioConfig>(),
    ) {
        (Some(graph), Some(_)) => prepare_graph(graph, &nodes, &request),
        // Distinguished from the above so the reason is the real one: an export
        // requested against a world with no engine is a different failure than a
        // target that cannot produce audio, and reporting the wrong one sends
        // whoever reads it looking at the wrong thing.
        _ => Err(invalid("no audio engine to export from")),
    };

    let (pending, ctx) = match prepared {
        Ok(prepared) => prepared,
        Err(e) => {
            world.trigger(ExportDone {
                entity,
                result: Err(nodes.name(e)),
            });
            return;
        }
    };

    let ExportRequest {
        target,
        config: mut export_config,
        clock,
        prepare,
        latency_from_graph,
        tail_from_graph,
        ..
    } = request;
    let clock = clock.render_clock();

    // The hook's look at the world, now, in the frame the export starts and
    // with the world still readable; its edit runs on the built fork. An
    // isolated copy is born empty, so a sampler-fed tap that skips this
    // renders silence.
    let edit = prepare.as_ref().map(|prepare| prepare(world));

    // Everything the task holds goes back to the main thread to drop, on
    // every path: finished, failed, or cancelled before or after it ran. A
    // forked in-process VST2 plugin, built in the gather above, closes its
    // instance when dropped, which VST2 confines to the main thread.
    let drops = world.get_resource_or_init::<ExportDrops>().sender();
    let pending = ToMain::new(pending, drops.clone());
    // The edit too: until it runs (on the pool, once the fork is built) it
    // is the caller's state, and a failed build or a cancelled task drops it
    // unrun.
    let edit = edit.map(|edit| ToMain::new(edit, drops.clone()));
    let task = AsyncComputeTaskPool::get().spawn(async move {
        // The fork's build: a hosted plugin's `plugin-server` launch and
        // state load, here on the pool.
        let mut graph = match pending.take().build() {
            Ok(graph) => ToMain::new(graph, drops),
            Err((e, remains)) => {
                drop(ToMain::new(remains, drops));
                return Err(nodes.name(e));
            }
        };
        if let Some(edit) = edit {
            (edit.take())(PreparedGraph {
                graph: &mut graph,
                ctx: &ctx,
            });
        }
        // A fork's editor holds whatever the edit left, uncommitted: send
        // it, and apply it here so the render's first block already runs it.
        if let Err(e) = graph.commit() {
            return Err(ExportError::Render(invalid(format!(
                "the prepare hook left the forked graph uncommittable: {e}"
            ))));
        }

        // The graph's own figures, asked of the graph that is rendered —
        // after the edit, which may have changed it.
        if latency_from_graph {
            export_config.render.latency = graph.reported_latency();
        }
        if let Some(cap) = tail_from_graph {
            // `resolve`, not `samples().unwrap_or(cap)`: a graph with a node
            // that never said (`Tail::Unknown`, the default) is spent at what
            // the others reported, not at the cap — which would append
            // silence.
            export_config.render.tail = graph.reported_tail().resolve(cap);
        }

        let rendered = match target {
            ExportTarget::File {
                path,
                normalize: None,
            } => render_to_file(&mut *graph, &export_config, clock.as_ref(), &path)
                .map(ExportOutput::File),
            ExportTarget::File {
                path,
                normalize: Some(normalize),
            } => render_normalized_to_file(
                &mut *graph,
                &export_config,
                clock.as_ref(),
                normalize,
                &path,
            )
            .map(ExportOutput::File),
            ExportTarget::Buffers => render_to_buffers(&mut *graph, &export_config, clock.as_ref())
                .map(ExportOutput::Buffers),
        };
        // `graph` goes to the main thread as it drops, before the result
        // is ready: `poll_exports` drops it there before reporting.
        rendered.map_err(|e| nodes.name(e))
    });

    world.entity_mut(entity).insert(ExportInFlight::new(task));
}

/// Gather the graph this request renders, plus the offline context its nodes
/// are rebound onto.
///
/// Refuses, with the reason, when the requested node has no outputs — there is
/// nothing to render from it — and when a node the copy needs cannot be
/// forked (named by the caller, [`NodeNames::name`]).
fn prepare_graph(
    graph: &AudioGraphRes,
    nodes: &NodeNames,
    request: &ExportRequest,
) -> Result<(PendingExport, OfflineTransport), tutti_export::Error> {
    let node = match request.source {
        ExportSource::Master => None,
        // Resolved here rather than stored: see `ExportSource::Node`.
        ExportSource::Node(entity) => Some(nodes.node(entity).ok_or_else(|| invalid(NOT_A_NODE))?),
    };

    // The timeline every transport-aware node in the copy is re-seated on:
    // the request's clock, which is the same object the renderer advances
    // (`ExportClock`), or a timeline stopped at beat 0 for a frozen one.
    // Never manufactured here from anything else: a default rolling timeline
    // nothing advances is how a tap on a 90 BPM project once rendered its
    // voices against a 120 BPM playhead stuck at beat 0.
    let ctx: OfflineTransport = request.clock.offline();

    // Every node of the fork is isolated, rebound onto `ctx` and reset — see
    // `AudioGraphRes::export`.
    match graph.export(node, &ctx, request.config.render.sample_rate) {
        Ok(graph) => Ok((graph, ctx)),
        Err(ExportRefused::NoOutputs) => Err(invalid(NO_OUTPUTS)),
        Err(ExportRefused::GraphHasNoOutputs) => Err(invalid(GRAPH_HAS_NO_OUTPUTS)),
        Err(ExportRefused::Render(e)) => Err(e),
    }
}

/// Why a node export found nothing to render: its node has no audio outputs.
const NO_OUTPUTS: &str = "export target node has no audio outputs";

/// Why a node export found nothing to render: the entity is not bound to a
/// graph node.
const NOT_A_NODE: &str = "export target entity is not a graph node";

/// Why an export found nothing to render: the graph has no global outputs.
const GRAPH_HAS_NO_OUTPUTS: &str = "the graph has no outputs to export";

fn invalid(reason: impl Into<String>) -> tutti_export::Error {
    tutti_export::Error::InvalidConfig(reason.into())
}

/// Every entity bound to a graph node, by entity and by node key — what
/// resolves an [`ExportSource::Node`] to its node, and what names a node an
/// export failed on. Taken on the main thread when the render starts, and
/// moved into the render task (a fork can fail mid-render, long after the
/// world was last readable).
struct NodeNames {
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
}

/// Drives in-flight exports; triggers [`ExportDone`] on the ones that
/// finished.
///
/// **Pinned to the main thread** ([`NonSendMarker`](bevy_ecs::system::NonSendMarker)):
/// every export hands its graph (or, failed or cancelled, whatever of it
/// was built) back through [`ExportDrops`], and it is dropped here, before
/// the export reports. A forked in-process VST2 plugin closes its instance
/// when dropped, which VST2 confines to the main thread.
pub fn poll_exports(
    mut commands: Commands,
    mut in_flight: Query<(Entity, &mut ExportInFlight)>,
    drops: Option<Res<ExportDrops>>,
    _main: bevy_ecs::system::NonSendMarker,
) {
    let mut done = Vec::new();
    for (entity, mut export) in in_flight.iter_mut() {
        if let Some(result) = export.poll() {
            done.push((entity, result));
        }
    }
    // After the polls: a task sends its graph before its result is ready.
    if let Some(drops) = drops {
        drops.drop_here();
    }
    for (entity, result) in done {
        commands
            .entity(entity)
            .remove::<ExportInFlight>()
            .trigger(move |entity: Entity| ExportDone { entity, result });
    }
}

/// What exports hand back to the **main thread** to drop: a rendered graph,
/// what a failed fork build had made, whatever a cancelled export held.
/// [`poll_exports`] drops it every frame.
///
/// A forked in-process VST2 plugin is built on the main thread (its fork
/// source is pinned: `tutti_graph::ForkBuild::pinned`) and must close there
/// too, but its export builds, renders and may be cancelled on the compute
/// pool; whatever the task holds is wrapped so that wherever it is dropped
/// it is sent here instead. Inserted by [`ExportPlugin`](super::ExportPlugin)
/// (and by [`start_exports`] if missing). Something sent once it is gone
/// drops where it is.
#[derive(Resource)]
pub struct ExportDrops {
    send: Sender<Box<dyn Send>>,
    recv: Mutex<Receiver<Box<dyn Send>>>,
}

impl Default for ExportDrops {
    fn default() -> Self {
        let (send, recv) = channel();
        Self {
            send,
            recv: Mutex::new(recv),
        }
    }
}

impl ExportDrops {
    fn sender(&self) -> Sender<Box<dyn Send>> {
        self.send.clone()
    }

    /// Drop, on this thread, everything sent so far.
    fn drop_here(&self) {
        let recv = self.recv.lock().unwrap_or_else(PoisonError::into_inner);
        while let Ok(value) = recv.try_recv() {
            drop(value);
        }
    }
}

/// `T`, sent to [`ExportDrops`] wherever it is dropped, unless
/// [`take`](Self::take)n first.
struct ToMain<T: Send + 'static> {
    value: Option<T>,
    to: Sender<Box<dyn Send>>,
}

impl<T: Send + 'static> ToMain<T> {
    fn new(value: T, to: Sender<Box<dyn Send>>) -> Self {
        Self {
            value: Some(value),
            to,
        }
    }

    /// The value, no longer sent anywhere: its new owner drops it.
    fn take(mut self) -> T {
        self.value.take().expect("taken once")
    }
}

impl<T: Send + 'static> std::ops::Deref for ToMain<T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.value.as_ref().expect("not taken")
    }
}

impl<T: Send + 'static> std::ops::DerefMut for ToMain<T> {
    fn deref_mut(&mut self) -> &mut T {
        self.value.as_mut().expect("not taken")
    }
}

impl<T: Send + 'static> Drop for ToMain<T> {
    fn drop(&mut self) {
        if let Some(value) = self.value.take() {
            // A receiver already gone (the world is being torn down) hands
            // the value back, and it drops here.
            let _ = self.to.send(Box::new(value));
        }
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
    use crate::export::{ExportClock, ExportPlugin};
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
    /// renders on the compute pool: the task sends it to [`ExportDrops`] as
    /// it drops, and `poll_exports` (main-thread-pinned) drops it there. A
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
    /// Mutation (run, with `bevy_tasks/multi_threaded`): `ToMain`'s drop
    /// dropping the graph where it is instead of sending it to `ExportDrops`
    /// (what rendering it by value in the task did) → the copy's drop is
    /// recorded on a pool thread → fails. Mutation (run, threaded):
    /// `poll_exports` not draining `ExportDrops` → the copy is never dropped
    /// → fails.
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
