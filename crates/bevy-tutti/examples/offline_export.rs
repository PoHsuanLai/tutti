//! Rendering a graph to disk, headless.
//!
//! Picks up where `graph_wiring` leaves off: the same spawn-and-declare graph,
//! then two exports of it — the whole mix, and one node on its own.
//!
//! ```sh
//! cargo run -p bevy-tutti --example offline_export --features export,wav
//! # the same on the native graph, where an export forks the live graph:
//! cargo run -p bevy-tutti --example offline_export --features export,wav -- --native
//! ```
//!
//! The shape worth noticing: an export is an **entity**. You spawn a request,
//! and the result arrives as an event on that same entity — so the handler sits
//! next to the code that knows what the render was for.

use std::sync::Arc;

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;

// Export lives in the prelude alongside the rest of the layer.
use bevy_tutti::prelude::*;
use tutti_core::transport::{OfflineTimeline, OfflineTimelineConfig, Transport};
use tutti_core::{Hz, Q};
use tutti_export::{
    AudioFormat, BitDepth, ChannelLayout, EncodeConfig, ExportConfig, Normalize, RenderConfig,
};
use tutti_nodes::testing::{Osc, Through};
use tutti_nodes::{SvfFilterNode, SvfType};
use tutti_types::Db;

const SAMPLE_RATE: f64 = 48_000.0;

/// The node we want to hear on its own, remembered from the build step.
#[derive(Resource)]
struct FilterNode(Entity);

fn main() {
    let mut app = App::new();

    // Same headless engine as `graph_wiring`, plus the export plugin. On
    // `--native` the graph is the native one, and each export renders a fork
    // of it (design doc 013, PR 12) instead of a clone of fundsp's `Net`.
    let backend = if std::env::args().any(|a| a == "--native") {
        bevy_tutti::graph::GraphBackend::Native
    } else {
        bevy_tutti::graph::GraphBackend::Net
    };
    println!("exporting from the {backend:?} graph");
    let mut graph = AudioGraphRes::headless_with(backend, 0, 2);
    graph.set_sample_rate(tutti_core::SampleRate(SAMPLE_RATE));
    app.insert_resource(graph);
    app.insert_resource(AudioEngineState::Running);
    app.insert_resource(TransportRes(Transport::new(SAMPLE_RATE)));
    app.insert_resource(AudioConfig {
        sample_rate: tutti_core::SampleRate(SAMPLE_RATE),
        channels: ChannelLayout::STEREO,
    });
    app.add_plugins((bevy_app::TaskPoolPlugin::default(), GraphReconcilePlugin));
    app.add_plugins(ExportPlugin);

    app.add_systems(Startup, build_chain);
    app.add_systems(Update, request_exports.run_if(run_once_after_startup));

    // The renders run on the task pool, so the app has to keep ticking. A real
    // host is already looping; here we spin until both have reported.
    for _ in 0..2000 {
        app.update();
        if app.world().resource::<Remaining>().0 == 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }

    println!("\ndone.");
}

/// Counts outstanding exports so `main` knows when to stop ticking.
#[derive(Resource)]
struct Remaining(usize);

fn build_chain(mut commands: Commands) {
    let osc = commands.spawn_audio_node(Osc::saw(Hz(110.0))).id();

    let filter = commands
        .spawn_audio_node(SvfFilterNode::<f64>::new(
            SvfType::LowPass,
            Hz(800.0),
            Q(1.0),
        ))
        .insert(PortSources::from(osc))
        .id();

    let out = commands
        .spawn_audio_node(Through::new(ChannelLayout::STEREO))
        .insert(
            PortSources::silent()
                .with(
                    0,
                    PortSource::Node {
                        entity: filter,
                        port: 0,
                    },
                )
                .with(
                    1,
                    PortSource::Node {
                        entity: filter,
                        port: 0,
                    },
                ),
        )
        .id();

    commands.insert_resource(MasterSources::from(out));
    commands.insert_resource(FilterNode(filter));
    commands.insert_resource(Remaining(2));
}

fn request_exports(mut commands: Commands, transport: Res<TransportRes>, filter: Res<FilterNode>) {
    let dir = std::env::temp_dir();

    // ── 1. The whole mix, normalized, to a file ───────────────────────────
    //
    // `Master` renders the graph as the speakers hear it. `FrozenClock` is the
    // honest clock for this graph — nothing in it reads musical time — and
    // saying so is a choice rather than an omission.
    let mix_path = dir.join("bevy-tutti-mix.wav");
    commands
        .spawn(ExportRequest::new(
            ExportSource::Master,
            ExportTarget::File {
                path: mix_path.clone(),
                // Two passes: the signal is measured, then a gain applied.
                normalize: Some(Normalize::lufs(Db(-14.0))),
            },
            config(),
            Arc::new(tutti_export::FrozenClock),
        ))
        .observe(report);

    // ── 2. One node's output, into memory ─────────────────────────────────
    //
    // `Node` isolates: everything downstream is severed, so this is "what does
    // the filter actually sound like" rather than its contribution to the mix.
    //
    // An isolated clone is rebound onto an offline timeline, seeded at the
    // session's real tempo. `on_timeline` sets the clock the renderer advances
    // AND the timeline the nodes read, from one argument — they are the same
    // object, and any other arrangement is a bug.
    let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        start_beat: tutti_core::Beat(0.0),
        tempo: transport.settings.tempo(),
        sample_rate: tutti_core::SampleRate(SAMPLE_RATE),
        loop_range: None,
    }));

    commands
        .spawn(
            ExportRequest::new(
                // The entity, like every other edge in this crate — no
                // unwrapping an `AudioNode` to get at an id.
                ExportSource::Node(filter.0),
                ExportTarget::Buffers,
                config(),
                timeline.clone(),
            )
            .on_timeline(timeline),
        )
        .observe(report);
}

/// One handler for both, because `ExportDone` carries which kind it was.
fn report(done: On<ExportDone>, mut remaining: ResMut<Remaining>) {
    match &done.result {
        Ok(ExportOutput::File(written)) => {
            println!("wrote {} ({} bytes)", written.path.display(), written.bytes);
        }
        Ok(ExportOutput::Buffers(rendered)) => {
            // Raw node output, un-normalized — a saw through a lowpass can
            // sit above 1.0, which is exactly why the mix above asks for a gain.
            let peak = rendered.planes[0]
                .iter()
                .fold(0.0f32, |acc, s| acc.max(s.abs()));
            println!(
                "rendered {} frames x {} ch at {:?}, peak {peak:.4}",
                rendered.frames().get(),
                rendered.channels(),
                rendered.sample_rate,
            );
        }
        Err(e) => println!("export failed: {e}"),
    }
    remaining.0 -= 1;
}

/// True on the first `Update` only — the requests are spawned once.
fn run_once_after_startup(mut done: Local<bool>) -> bool {
    if *done {
        return false;
    }
    *done = true;
    true
}

fn config() -> ExportConfig {
    ExportConfig {
        render: RenderConfig {
            sample_rate: tutti_core::SampleRate(SAMPLE_RATE),
            duration_seconds: 0.5,
            ..Default::default()
        },
        encode: EncodeConfig {
            format: AudioFormat::Wav,
            bit_depth: BitDepth::Int24,
            channels: ChannelLayout::STEREO,
        },
        ..Default::default()
    }
}
