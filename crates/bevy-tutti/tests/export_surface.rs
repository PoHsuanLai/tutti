//! The export surface is components plus one event.
//!
//! These tests pin the *shape* rather than the audio: that a spawned request
//! starts and reports, that the result reaches an observer attached at the
//! spawn site, that a batch spawned in one frame starts one at a time, and that
//! a request's `prepare` hook reaches the net that is actually rendered.

#![cfg(all(feature = "export", feature = "wav"))]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;
use bevy_tutti::export::{
    ExportDone, ExportInFlight, ExportOutput, ExportPlugin, ExportRequest, ExportSource,
    ExportTarget,
};
use bevy_tutti::graph::{AudioConfig, AudioGraphRes};
use tutti_export::{
    AudioFormat, BitDepth, ChannelLayout, EncodeConfig, ExportConfig, FrozenClock, RenderConfig,
};
use tutti_nodes::testing::{Const, Sink};

/// A tiny CPAL-free graph with one node piped to the output bus, so
/// `clone_isolated` succeeds.
fn graph_with_one_node() -> (AudioGraphRes, tutti_core::NodeId) {
    let mut net = tutti_core::dsp::Net::with_backend(2);
    let id = net.master(Const::mono(0.5));
    (AudioGraphRes(net), id)
}

fn stereo_config() -> ExportConfig {
    ExportConfig {
        render: RenderConfig {
            sample_rate: tutti_core::SampleRate(44_100.0),
            duration_seconds: 0.05,
            ..Default::default()
        },
        encode: EncodeConfig {
            format: AudioFormat::Wav,
            bit_depth: BitDepth::Float32,
            channels: ChannelLayout::STEREO,
        },
        ..Default::default()
    }
}

/// Build an app with the export plugin and a ready engine.
fn app_with_engine() -> (App, Entity) {
    let (graph, node) = graph_with_one_node();
    let mut app = App::new();
    app.add_plugins(bevy_app::TaskPoolPlugin::default());
    app.add_plugins(ExportPlugin);
    app.insert_resource(graph);
    app.insert_resource(AudioConfig {
        sample_rate: tutti_core::SampleRate(44_100.0),
        channels: ChannelLayout::STEREO,
    });
    // `engine_ready` gates `start_exports` on this state, not on the graph
    // resource, so a test graph alone is not enough.
    app.insert_resource(bevy_tutti::AudioEngineState::Running);
    // `ExportSource::Node` names an entity, so bind one to the node.
    let entity = app.world_mut().spawn(tutti_core::AudioNode(node)).id();
    (app, entity)
}

/// Run frames until `predicate` holds or the budget runs out. The render is on
/// a real task pool, so completion takes an unknown number of frames.
fn run_until(app: &mut App, mut predicate: impl FnMut(&mut World) -> bool) -> bool {
    for _ in 0..600 {
        app.update();
        if predicate(app.world_mut()) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    false
}

/// A spawned request starts (gaining `ExportInFlight`) and finishes (triggering
/// `ExportDone` with the variant its target asked for).
#[test]
fn a_buffers_request_runs_and_reports_planes() {
    let (mut app, _node) = app_with_engine();

    static CHANNELS: AtomicUsize = AtomicUsize::new(usize::MAX);
    CHANNELS.store(usize::MAX, Ordering::SeqCst);

    app.world_mut()
        .spawn(ExportRequest::new(
            ExportSource::Master,
            ExportTarget::Buffers,
            stereo_config(),
            Arc::new(FrozenClock),
        ))
        .observe(|done: On<ExportDone>| {
            match &done.result {
                Ok(ExportOutput::Buffers(rendered)) => {
                    CHANNELS.store(rendered.channels(), Ordering::SeqCst);
                }
                other => panic!("expected rendered buffers, got {other:?}"),
            };
        });

    let finished = run_until(&mut app, |_| CHANNELS.load(Ordering::SeqCst) != usize::MAX);
    assert!(finished, "the render never reported");
    assert_eq!(
        CHANNELS.load(Ordering::SeqCst),
        2,
        "a stereo request must come back as two planes"
    );
}

/// The file target writes a real file and reports its path.
#[test]
fn a_file_request_writes_and_reports_the_path() {
    let (mut app, _node) = app_with_engine();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.wav");

    static DONE: AtomicUsize = AtomicUsize::new(0);
    DONE.store(0, Ordering::SeqCst);

    app.world_mut()
        .spawn(ExportRequest::new(
            ExportSource::Master,
            ExportTarget::File {
                path: path.clone(),
                normalize: None,
            },
            stereo_config(),
            Arc::new(FrozenClock),
        ))
        .observe(|done: On<ExportDone>| {
            match &done.result {
                Ok(ExportOutput::File(written)) => {
                    assert!(written.bytes > 0, "an empty file was reported as written");
                    DONE.store(1, Ordering::SeqCst);
                }
                other => panic!("expected a written file, got {other:?}"),
            };
        });

    assert!(
        run_until(&mut app, |_| DONE.load(Ordering::SeqCst) == 1),
        "the export never reported"
    );
    assert!(path.exists(), "the file was reported but does not exist");
}

/// `ExportInFlight` marks the whole render: present the frame it starts, gone
/// once it reports.
///
/// `start_exports` checks this component to enforce one-at-a-time, so a gap at
/// either end would let a second render start on top of a live one. It is also
/// what a caller watches to know an export is running.
#[test]
fn export_in_flight_marks_the_whole_render_so_callers_can_gate_on_it() {
    let (mut app, _node) = app_with_engine();

    let entity = app
        .world_mut()
        .spawn(ExportRequest::new(
            ExportSource::Master,
            ExportTarget::Buffers,
            stereo_config(),
            Arc::new(FrozenClock),
        ))
        .id();

    // One frame: the request is consumed and the marker is on.
    app.update();
    assert!(
        !app.world().entity(entity).contains::<ExportRequest>(),
        "the request must be consumed when the render starts"
    );
    assert!(
        app.world().entity(entity).contains::<ExportInFlight>(),
        "a running render must be visible to a caller's run condition"
    );

    // And it clears, or the caller would gate itself off forever.
    assert!(
        run_until(&mut app, |world| {
            !world.entity(entity).contains::<ExportInFlight>()
        }),
        "ExportInFlight never cleared — a caller gating on it would deadlock"
    );
}

/// A node with no outputs cannot be rendered from; that must be a reported
/// failure, not a silent drop or a panic on the pool.
///
/// "No outputs" means the *unit* produces none (`clone_isolated` asks
/// `outputs_in`, the node's own arity — not whether it happens to be wired), so
/// this needs a genuine sink. A `dc` pushed but left unwired still has one
/// output and renders fine.
#[test]
fn an_unrenderable_node_reports_a_failure() {
    let (mut app, _node) = app_with_engine();

    // `Sink::mono()` consumes one channel and produces nothing.
    let orphan = {
        let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
        let id = graph.0.push(Box::new(Sink::mono()));
        app.world_mut().spawn(tutti_core::AudioNode(id)).id()
    };

    static FAILED: AtomicUsize = AtomicUsize::new(0);
    FAILED.store(0, Ordering::SeqCst);

    app.world_mut()
        .spawn(ExportRequest::new(
            ExportSource::Node(orphan),
            ExportTarget::Buffers,
            stereo_config(),
            Arc::new(FrozenClock),
        ))
        .observe(|done: On<ExportDone>| {
            assert!(
                done.result.is_err(),
                "a node with no outputs must not report success"
            );
            FAILED.store(1, Ordering::SeqCst);
        });

    assert!(
        run_until(&mut app, |_| FAILED.load(Ordering::SeqCst) == 1),
        "an unrenderable request must still report"
    );
}

/// Several requests spawned in the SAME frame must not all start at once.
///
/// This is the property a caller-side `run_if(not(any_with_component::<
/// ExportInFlight>))` cannot provide and this crate previously advertised: a run
/// condition is evaluated against the previous frame's state, so every request
/// spawned in one frame passes it and they all start together — each doing a
/// main-thread deep clone of the live net, back to back, which is what stalls
/// the audio callback.
#[test]
fn a_batch_spawned_in_one_frame_starts_one_at_a_time() {
    let (mut app, _node) = app_with_engine();

    for _ in 0..5 {
        app.world_mut().spawn(ExportRequest::new(
            ExportSource::Master,
            ExportTarget::Buffers,
            stereo_config(),
            Arc::new(FrozenClock),
        ));
    }

    // Watch every frame until the batch drains: at no point may two renders be
    // in flight together. Checking only the first frame would pass even if the
    // limit were "start two per frame".
    let mut max_seen = 0usize;
    for _ in 0..64 {
        app.update();
        let in_flight = app
            .world_mut()
            .query_filtered::<Entity, With<ExportInFlight>>()
            .iter(app.world())
            .count();
        max_seen = max_seen.max(in_flight);
    }

    assert_eq!(
        max_seen, 1,
        "at most one render may be in flight; saw {max_seen} at once"
    );

    let unstarted = app
        .world_mut()
        .query_filtered::<Entity, With<ExportRequest>>()
        .iter(app.world())
        .count();
    assert_eq!(
        unstarted, 0,
        "every request must eventually start — the limit delays, it does not drop"
    );
}

/// The `prepare` hook runs on the net that is actually rendered, with the world
/// readable, before the render leaves the main thread.
///
/// Pinned by *audio*, not by a call counter: a hook that runs against some other
/// net, or after the task was spawned, would leave the rendered signal at the
/// graph's own 0.5 rather than the 0.25 this one writes.
#[test]
fn a_prepare_hook_reaches_the_net_that_gets_rendered() {
    let (mut app, _node) = app_with_engine();

    // The hook needs something in the world to read, or it could be pinned by a
    // closure capture alone — which would not show that `&World` arrives.
    #[derive(Resource)]
    struct Level(f32);
    app.insert_resource(Level(0.25));

    static PEAK_MILLI: AtomicUsize = AtomicUsize::new(usize::MAX);
    PEAK_MILLI.store(usize::MAX, Ordering::SeqCst);

    let request = ExportRequest::new(
        ExportSource::Master,
        ExportTarget::Buffers,
        stereo_config(),
        Arc::new(FrozenClock),
    )
    .with_prepare(|prepared, world| {
        // Replace the whole graph's output with a constant read from the world.
        let level = world.resource::<Level>().0;
        prepared.net.master(Const::mono(level));
    });

    app.world_mut()
        .spawn(request)
        .observe(|done: On<ExportDone>| {
            if let Ok(ExportOutput::Buffers(rendered)) = &done.result {
                let peak = rendered.planes[0]
                    .iter()
                    .fold(0.0f32, |acc, s| acc.max(s.abs()));
                PEAK_MILLI.store((peak * 1000.0).round() as usize, Ordering::SeqCst);
            }
        });

    assert!(
        run_until(&mut app, |_| PEAK_MILLI.load(Ordering::SeqCst)
            != usize::MAX),
        "the export never reported"
    );

    assert_eq!(
        PEAK_MILLI.load(Ordering::SeqCst),
        250,
        "prepare must run on the net that is rendered — an unmodified graph \
         would come back at the 0.5 it was built with"
    );
}

/// **The caller's timeline is what the nodes get rebound onto.**
///
/// `RenderClock` is advance-only, so this crate cannot read a timeline back out
/// of the `clock` a caller supplies. It used to manufacture its own — hardcoded
/// to 120 BPM at beat 0 — and rebind every transport-aware node in the clone
/// onto *that*, while the renderer advanced the caller's. The two ends
/// disagreed: a tap on a 90 BPM project bound its voices to a 120 BPM playhead
/// that nothing then advanced, which is the silent-playhead failure the
/// per-node rebind exists to prevent.
#[test]
fn the_callers_timeline_is_the_one_nodes_are_rebound_onto() {
    use tutti_core::transport::{OfflineTimeline, OfflineTimelineConfig};

    let (mut app, node) = app_with_engine();

    // A deliberately un-default transport: neither 120 BPM nor beat 0.
    let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        start_beat: tutti_core::Beat(16.0),
        tempo: tutti_core::Bpm(90.0),
        sample_rate: tutti_core::SampleRate(44_100.0),
        loop_range: None,
    }));
    static SEEN_TEMPO: AtomicUsize = AtomicUsize::new(usize::MAX);
    static SEEN_BEAT: AtomicUsize = AtomicUsize::new(usize::MAX);
    SEEN_TEMPO.store(usize::MAX, Ordering::SeqCst);
    SEEN_BEAT.store(usize::MAX, Ordering::SeqCst);

    let request = ExportRequest::new(
        ExportSource::Node(node),
        ExportTarget::Buffers,
        stereo_config(),
        timeline.clone(),
    )
    .on_timeline(timeline.clone())
    // The hook sees the very context the nodes were rebound with.
    .with_prepare(|prepared, _world| {
        if let Some(transport) = prepared.ctx {
            // Required for `tempo()`/`beat()` in a default build. Some
            // feature-gated import already brings the trait into scope under
            // `--all-features`, where it then reads as redundant — so it is
            // allowed rather than removed; deleting it breaks the default build.
            #[allow(unused_imports)]
            use tutti_core::Timeline;
            SEEN_TEMPO.store(transport.tempo().get().round() as usize, Ordering::SeqCst);
            SEEN_BEAT.store(transport.beat().get().round() as usize, Ordering::SeqCst);
        }
    });

    app.world_mut().spawn(request);

    assert!(
        run_until(&mut app, |_| SEEN_TEMPO.load(Ordering::SeqCst)
            != usize::MAX),
        "the render never started"
    );

    assert_eq!(
        SEEN_TEMPO.load(Ordering::SeqCst),
        90,
        "nodes must be rebound onto the caller's timeline, not a manufactured 120 BPM one"
    );
    assert_eq!(
        SEEN_BEAT.load(Ordering::SeqCst),
        16,
        "and at the caller's start beat, not beat 0"
    );
}
