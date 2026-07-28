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
use tutti_core::dsp::{lowpass_hz, pass, saw_hz, AudioUnit as _, Net, Source};

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
    app.add_plugins(GraphReconcilePlugin);

    app.add_systems(Startup, build_chain);
    app.add_systems(Update, report.run_if(run_once_after_startup));

    app.update();
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
fn report(graph: Res<AudioGraphRes>, nodes: Query<(&AudioNode, &Label)>) {
    println!("nodes in the graph:");
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
