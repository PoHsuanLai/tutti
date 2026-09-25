//! Start renders, and report them when they finish.
//!
//! Two systems and no policy. [`start_exports`] turns each new
//! [`ExportRequest`] into a task on the shared compute pool;
//! [`poll_exports`] drives those tasks and triggers [`ExportDone`].
//!
//! The render itself is synchronous and `Send` (tutti-export spawns no threads
//! by design — "a host that wants a render off the main thread already owns a
//! task pool"). So this module is the *whole* of what Bevy adds: a place to run
//! it, and a way to hear about it.

use std::collections::HashMap;

use bevy_ecs::prelude::*;
use bevy_tasks::AsyncComputeTaskPool;

use tutti_core::transport::OfflineTransport;
use tutti_export::{render_normalized_to_file, render_to_buffers, render_to_file, RenderGraph};
use tutti_types::NodeKey;

use crate::export::request::{
    ExportDone, ExportError, ExportInFlight, ExportNode, ExportOutput, ExportRequest, ExportSource,
    ExportTarget, PreparedGraph,
};
use crate::graph::resources::ExportRefused;
use crate::graph::{AudioConfig, AudioGraphRes};

/// Start the oldest pending [`ExportRequest`], if nothing is already running.
///
/// The expensive part — copying the graph (a fork) —
/// happens here, on the main thread, because the copy reads the graph
/// resource and cannot cross into a `'static` task. That is also why only one
/// starts per frame; see [`ExportInFlight`].
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

    let (mut graph, ctx) = match prepared {
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

    // The caller's last look at the graph, on the main thread, with the world
    // still readable. An isolated copy is born empty, so a sampler-fed tap
    // that skips this renders silence.
    if let Some(prepare) = prepare.as_ref() {
        prepare(
            PreparedGraph {
                graph: &mut graph,
                ctx: &ctx,
            },
            world,
        );
    }
    // A fork's editor holds whatever the hook edited, uncommitted: send it,
    // and apply it here so the render's first block already runs it.
    let RenderGraph { editor, executor } = &mut graph;
    if let Err(e) = editor.commit() {
        world.trigger(ExportDone {
            entity,
            result: Err(ExportError::Render(invalid(format!(
                "the prepare hook left the forked graph uncommittable: {e}"
            )))),
        });
        return;
    }
    executor.apply_pending();
    editor.collect();

    // The graph's own figures, asked of the graph that is rendered — after
    // the hook, which may have changed it.
    if latency_from_graph {
        export_config.render.latency = graph.reported_latency();
    }
    if let Some(cap) = tail_from_graph {
        // `resolve`, not `samples().unwrap_or(cap)`: a graph with a node that
        // never said (`Tail::Unknown`, the default) is spent at what the
        // others reported, not at the cap — which would append silence.
        export_config.render.tail = graph.reported_tail().resolve(cap);
    }

    let task = AsyncComputeTaskPool::get().spawn(async move {
        let rendered = match target {
            ExportTarget::File {
                path,
                normalize: None,
            } => {
                render_to_file(graph, &export_config, clock.as_ref(), &path).map(ExportOutput::File)
            }
            ExportTarget::File {
                path,
                normalize: Some(normalize),
            } => render_normalized_to_file(graph, &export_config, clock.as_ref(), normalize, &path)
                .map(ExportOutput::File),
            ExportTarget::Buffers => {
                render_to_buffers(graph, &export_config, clock.as_ref()).map(ExportOutput::Buffers)
            }
        };
        rendered.map_err(|e| nodes.name(e))
    });

    world.entity_mut(entity).insert(ExportInFlight::new(task));
}

/// Build the graph this request renders, plus the offline context its nodes
/// were rebound onto.
///
/// Refuses, with the reason, when the requested node has no outputs — there is
/// nothing to render from it — and when a node the copy needs cannot be
/// forked (named by the caller, [`NodeNames::name`]).
fn prepare_graph(
    graph: &AudioGraphRes,
    nodes: &NodeNames,
    request: &ExportRequest,
) -> Result<(RenderGraph, OfflineTransport), tutti_export::Error> {
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
    match graph.export(node, &ctx, request.config.render.sample_rate, &nodes.midi) {
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
    /// Every node with a captured MIDI port (its entity's current
    /// `MidiTarget`): a fork must carry the clip on it, or refuse.
    midi: std::collections::BTreeSet<NodeKey>,
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
            by_key.insert(
                crate::graph::native::key(*node),
                (entity, name.map(|n| n.as_str().to_owned())),
            );
        }
        #[allow(unused_mut)]
        let mut midi = std::collections::BTreeSet::new();
        #[cfg(feature = "midi")]
        for (node, target) in world
            .query::<(&tutti_core::AudioNode, &crate::midi::MidiTarget)>()
            .iter(world)
        {
            // A target left over from a node replaced by hand is inert, as
            // it is to every other reader (`MidiTargetResolver::port`).
            if target.node() == node.0 {
                midi.insert(crate::graph::native::key(*node));
            }
        }
        Self {
            nodes,
            by_key,
            midi,
        }
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

/// Drive in-flight renders; trigger [`ExportDone`] on the ones that finished.
pub fn poll_exports(mut commands: Commands, mut in_flight: Query<(Entity, &mut ExportInFlight)>) {
    for (entity, mut export) in in_flight.iter_mut() {
        let Some(result) = export.poll() else {
            continue; // still running
        };
        commands
            .entity(entity)
            .remove::<ExportInFlight>()
            .trigger(move |entity: Entity| ExportDone { entity, result });
    }
}

/// Exports from an engine as a host builds one (`build_on`, the device-free
/// `build_into`), over a manual stream: the beat clock and the metronome in
/// the graph, the click wired to the clock and to the master. Every other
/// export test builds a bare `headless` graph, which has neither — and on
/// the native graph the clock was once inserted with no fork source, so every
/// engine-built graph refused a master export as not forkable.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::build::build_on;
    use crate::export::{ExportClock, ExportPlugin};
    use crate::graph::{
        EngineNodes, GraphReconcilePlugin, MasterSources, MetronomeRes, TransportRes,
    };
    use crate::{AudioEngineState, TuttiPlugin};
    use bevy_app::App;
    use std::sync::{Arc, Mutex};
    use tutti_core::transport::{MetronomeMode, OfflineTimeline, OfflineTimelineConfig};
    use tutti_core::{Beat, Bpm, ChannelLayout, MotionEvent, SampleRate};
    use tutti_cpal::{AudioEngine, ManualStreamDriver, OutputSpec};
    use tutti_export::{ExportConfig, RenderConfig};

    const RATE: f64 = 48_000.0;

    /// An engine at 48 kHz, the metronome always on and wired to the master,
    /// the live transport rolling. Returns the app and the click's entity.
    fn engine_app() -> (App, Entity, tutti_cpal::ManualStream) {
        let mut app = App::new();
        let plugin = TuttiPlugin::default();
        let (driver, stream) = ManualStreamDriver::new();
        build_on(
            &plugin,
            &mut app,
            AudioEngine::from_spec(OutputSpec::new(
                SampleRate(RATE),
                ChannelLayout::STEREO,
                tutti_cpal::cpal::SampleFormat::F32,
            )),
            |engine, state| engine.start_with(state, driver),
        )
        .expect("builds with no device");
        app.insert_resource(AudioEngineState::Running);
        app.add_plugins((
            bevy_app::TaskPoolPlugin::default(),
            GraphReconcilePlugin,
            ExportPlugin,
        ));
        let click = app.world().resource::<EngineNodes>().click;
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
        (app, click, stream)
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

    /// **An engine-built graph exports — the master, the click, and the beat
    /// clock — and the clock's beat is the render's.**
    /// The render's timeline is 120 BPM from beat 0.25 at 48 kHz, so the
    /// clock's whole-beat port (the export's left channel; a node export
    /// clamps the right channel onto its last port, the fraction) reads 0
    /// until frame 18 000, 1 until 42 000, then 2 — to a frame: the clock
    /// accumulates `beats_per_sample` (1/24 000 of a beat, which binary does
    /// not hold) frame by frame, and lands on beat 2 one frame late (the
    /// offline-timeline rounding doc 013 records as a follow-up). The live
    /// transport, rolling since the build, is at another beat: a forked
    /// clock that read it would not step there: the forked `EnvClock` reads
    /// the render's `Env`. (Until PR 13 this ran on `Net` too, where only the
    /// lengths were asserted: a `Net` node export of the `TransportClock`
    /// started from beat 0, not from the timeline's 0.25, and a master export
    /// was a plain clone clicking on the live transport's beats.)
    ///
    /// What the click itself renders is not asserted. The fork is cloned from
    /// the click's shadow, taken at insert, whose metronome mode is the one it
    /// had then (`Off` here):
    /// `MetronomeRes` writes the live node's settings cell, which the shadow
    /// detached from (doc 013, "Metronome volume and mode ... live-only; a
    /// click is not part of an export"). Its session flags are frozen at the
    /// fork from the live transport, and tutti-core's
    /// `isolate_snapshots_settings_and_session_flags` row pins that.
    ///
    /// Mutation (run): `insert_env_clock` inserting `EnvClock` plainly (no
    /// fork source, as before) → fails on its first export, refused as not
    /// forkable.
    #[test]
    fn an_engine_built_graph_exports_and_its_clock_is_the_renders() {
        let (mut app, click, _stream) = engine_app();
        let clock = app.world().resource::<EngineNodes>().clock;
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
                ExportSource::Node(clock),
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
            "the clock's whole beats on the render's timeline: {steps:?}, want {want:?}"
        );
    }
}
