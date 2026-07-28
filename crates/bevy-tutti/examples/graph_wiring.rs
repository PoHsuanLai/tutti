//! Building an audio graph from the ECS, headless.
//!
//! The whole core surface in one pass: spawn nodes, declare what feeds what,
//! declare what reaches the speakers, and read the result back. No audio device
//! — the graph is rendered by hand at the end so the numbers are checkable.
//!
//! ```sh
//! cargo run -p bevy-tutti --example graph_wiring
//! ```
//!
//! The shape worth noticing: **nothing here calls `connect`**. A node arrives
//! unwired, and what feeds it is a component. That is what makes the graph
//! inspectable — and what makes "two nodes both claim the output" impossible to
//! write rather than a race.

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;

use bevy_tutti::prelude::*;
// `AudioUnit` is not imported here: the prelude carries it, because
// `spawn_audio_node` is generic over it and a host needs to name it.
use tutti_core::dsp::{lowpass_hz, pass, saw_hz, Net, Source};
use tutti_core::transport::Transport;

const SAMPLE_RATE: f64 = 48_000.0;

/// A marker so the report can name what it is looking at.
#[derive(Component)]
struct Label(&'static str);

fn main() {
    let mut app = App::new();

    // ── The engine, minus the device ──────────────────────────────────────
    //
    // `TuttiPlugin` would open CPAL and insert all of this. Doing it by hand
    // keeps the example headless and shows exactly what the core needs:
    // a graph, a claim that the engine is up, and the reconcile pipeline.
    app.insert_resource(AudioGraphRes(Net::with_backend(2)));
    app.insert_resource(AudioEngineState::Running);
    app.insert_resource(TransportRes(Transport::new(SAMPLE_RATE)));
    app.add_plugins(GraphReconcilePlugin);

    app.add_systems(Startup, (build_chain, set_up_transport));
    app.add_systems(Update, report.run_if(run_once_after_startup));

    app.update();
}

/// Everything transport-shaped goes through `TransportRes`'s `Deref`.
///
/// There is no wrapper for any of this — `TransportRes` is a newtype over
/// tutti-core's `Transport`, so a host calls the engine's own API. Note every
/// setter takes `&self` (the state is atomics), which is why `Res` suffices and
/// why `Res<TransportRes>` never triggers Bevy change detection: a host that
/// wants "did the tempo change this frame" diffs it itself.
fn set_up_transport(transport: Res<TransportRes>) {
    transport.settings.set_tempo(128.0);

    // Loop bars 2–4, in beats.
    transport.settings.loop_span.set_range(4.0, 8.0);
    transport.settings.loop_span.set_enabled(true);

    // Transitions are *queued*, not applied: `motion` is a lock-free ring the
    // audio thread drains at the top of each block. `is_playing()` still reads
    // false here, and that is correct — nothing has rendered yet.
    let _ = transport.motion.try_send(MotionEvent::Play);
}

/// Spawn three nodes and declare the signal path between them.
fn build_chain(mut commands: Commands) {
    // `spawn_audio_node` adds the unit and binds an entity to it. The node is
    // unwired: it renders nothing until something declares it as a source.
    let osc = commands
        .spawn_audio_node(saw_hz(110.0))
        .insert(Label("osc"))
        .id();

    // `AudioSources` on the *sink* says what feeds each of its input ports.
    // Index 0 is input port 0. The filter takes the oscillator.
    let filter = commands
        .spawn_audio_node(lowpass_hz(800.0, 1.0))
        .insert((Label("filter"), AudioSources::from(osc)))
        .id();

    // A stereo pair fed from the same mono filter — `with` sets one port at a
    // time, so an asymmetric chain is just two different declarations.
    let out = commands
        .spawn_audio_node(pass() | pass())
        .insert((
            Label("out"),
            AudioSources::silent()
                .with(0, AudioSource::Node { entity: filter, port: 0 })
                .with(1, AudioSource::Node { entity: filter, port: 0 }),
        ))
        .id();

    // And what reaches the speakers. One resource, one value per channel —
    // which is why two nodes cannot both claim the master.
    commands.insert_resource(MasterSources::from(out));
}

/// Read the graph back and render a few samples through it.
fn report(
    graph: Res<AudioGraphRes>,
    transport: Res<TransportRes>,
    nodes: Query<(&AudioNode, &Label)>,
) {
    println!("transport:");
    println!("  tempo    {:?}", transport.settings.tempo());
    println!(
        "  loop     {:?} enabled={}",
        transport.settings.loop_span.bounds(),
        transport.settings.loop_span.is_enabled()
    );
    println!(
        "  playing  {} (Play is queued; the audio thread drains it)",
        transport.motion.is_playing()
    );

    println!("\nnodes in the graph:");
    for (node, label) in &nodes {
        println!("  {:<7} inputs={}", label.0, graph.0.inputs_in(node.0));
    }

    println!("\nedges, read back from the engine:");
    for (node, label) in &nodes {
        for port in 0..graph.0.inputs_in(node.0) {
            // `Net::source` is why this layer keeps no shadow state: the engine
            // can always be asked what a port currently holds.
            let src = match graph.0.source(node.0, port) {
                Source::Local(id, p) => format!("node {id:?} port {p}"),
                Source::Global(p) => format!("graph input {p}"),
                Source::Zero => "silence".to_string(),
            };
            println!("  {:<7} port {port} <- {src}", label.0);
        }
    }

    println!("\nmaster bus:");
    for channel in 0..2 {
        println!("  channel {channel} <- {:?}", graph.0.output_source(channel));
    }

    // Render a handful of frames. A committed graph is a real signal path, so
    // this is the same arithmetic the audio thread would do.
    let mut net = graph.0.clone();
    net.set_sample_rate(tutti_core::SampleRate(SAMPLE_RATE));
    let mut frame = [0.0f32; 2];
    println!("\nfirst 4 output frames:");
    for i in 0..4 {
        net.tick(&[], &mut frame);
        println!("  {i}: [{:+.4}, {:+.4}]", frame[0], frame[1]);
    }
}

/// Run the report exactly once, on the frame after `Startup`'s commands flushed.
fn run_once_after_startup(mut done: Local<bool>) -> bool {
    if *done {
        return false;
    }
    *done = true;
    true
}
