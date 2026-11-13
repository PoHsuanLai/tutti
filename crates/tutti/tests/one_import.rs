//! The headline claim, compiled and run: **one dependency, one `use` line,
//! a graph that renders.**
//!
//! Device-free, so it runs on every platform in CI. The smallest harness in
//! the repo (`tutti-engine/tests/render/root_channel_layouts.rs`) is the shape this
//! borrows — a graph, its editor and executor, an engine, one block, and an
//! assertion that the block is not silence.
//!
//! What this cannot prove is that a *live* engine assembles in one import;
//! nothing in the engine offers that (see `examples/headless_engine.rs` and
//! the note in `src/lib.rs`). It proves the vocabulary arrives.

use tutti::types::MaxBlock;
use tutti::{
    At, BarNumber, Beat, Bpm, Cents, ChannelLayout, Db, Engine, Frame, Hz, InterleavedMut, Meter,
    MeterMap, MotionEvent, SampleRate, Samples, Seconds, TempoMap, Transport, TransportCommand,
};

/// Everything below is reached through `tutti::` and nothing else. If the
/// façade stopped re-exporting something, this stops compiling.
#[test]
fn one_import_renders_a_block() {
    let mut g = tutti::graph::GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let tone = g.add(tutti::nodes::testing::Osc::sine(Hz(440.0)));
    g.pipe_output(tone);
    let (mut editor, executor) = g
        .build(tutti::graph::Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::of(Samples(256)),
        ))
        .expect("builds");

    let transport = tutti::transport::Transport::new(SampleRate::SR_48K);
    let engine = Engine::new(&transport, &mut editor, executor).expect("within the limits");

    let mut out = vec![0.0f32; 256 * 2];
    engine.process(&mut InterleavedMut::new(&mut out, ChannelLayout::STEREO));

    assert!(
        out.iter().any(|&s| s != 0.0),
        "the graph must actually render — an assertion over silence would \
         pass against a façade that re-exported nothing useful"
    );
}

/// The measurement vocabulary is at the crate's root, converting as
/// documented. If the root ever narrowed, this names which type went missing
/// rather than failing somewhere downstream.
#[test]
fn the_measurement_vocabulary_arrives_and_converts() {
    assert!((Db(-6.0).to_amplitude().get() - 0.501).abs() < 0.01);
    assert_eq!(Seconds(1.0).to_samples(SampleRate::SR_48K).get(), 48_000);
    assert!((Cents(1200.0).to_semitones().get() - 12.0).abs() < 1e-6);
    assert_eq!(ChannelLayout::STEREO.count(), 2);
}

/// The nodes and the node contract are reachable without naming their crates.
#[test]
fn the_dsp_library_and_the_node_contract_are_reachable() {
    // A `tutti-nodes` unit, built and driven through `tutti::` alone.
    let _lfo = tutti::nodes::LfoNode::new(tutti::nodes::LfoShape::Sine);
    // The node contract, `tutti_graph::Node`, on a node reached through
    // `tutti::` too.
    let node = tutti::nodes::testing::Const::frame(&[0.5, 0.5]);
    let shape = tutti::graph::Node::shape(&node);
    assert_eq!(shape.audio_out(), ChannelLayout::STEREO);
    assert_eq!(shape.audio_in(), ChannelLayout::EMPTY);
}

/// Offline export, end to end, through the façade only.
#[cfg(feature = "export")]
#[test]
fn a_graph_bounces_to_buffers_through_the_facade() {
    let rate = SampleRate::SR_48K;
    let mut g = tutti::graph::GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let node = g.add(tutti::nodes::testing::Const::frame(&[0.5, 0.5]));
    g.pipe_output(node);
    let (editor, executor) = g
        .build(tutti::export::RenderGraph::prepare(rate))
        .expect("builds");
    let mut graph =
        tutti::export::RenderGraph::assemble(editor, executor, tutti::export::Setup::new())
            .expect("built together");

    let job = tutti::export::Job::new(rate, Seconds(0.01).to_samples(rate), Samples(0), Samples(0))
        .expect("a render");
    let rendered = tutti::export::render_one(
        &mut graph,
        &job,
        &tutti::transport::FrozenClock,
        tutti::export::Output::master(tutti::export::BufferTarget::new(ChannelLayout::STEREO)),
        &mut tutti::export::Control::none(),
    )
    .expect("a DC graph renders")
    .into_finished()
    .and_then(tutti::export::Produced::into_buffers)
    .expect("nothing cancels a control with no token");
    assert!(!rendered.planes().is_empty());
    assert!(rendered.planes()[0].iter().all(|&s| (s - 0.5).abs() < 1e-6));
}

/// A timed transport program is spelled from the root alone: `At`,
/// `TransportCommand`, and the time-signature trait with its `BarNumber`.
#[test]
fn a_timed_transport_program_needs_only_the_root() {
    let transport = Transport::new(SampleRate::SR_48K);
    transport
        .motion()
        .schedule(At::Frame(Frame(128)), MotionEvent::Play)
        .expect("room");
    transport
        .motion()
        .schedule(
            At::Beat(Beat(16.0)),
            TransportCommand::Motion(MotionEvent::stop()),
        )
        .expect("room");
    assert_eq!(transport.motion().scheduled_outstanding(), 2);

    let map = MeterMap::default();
    let bar: BarNumber = map.bar_numbers()[0];
    assert_eq!(Meter::bar_start(&map, bar), Beat(0.0));
}

/// A tempo map is built from `tutti::tempo` alone and published to the
/// transport's settings (what `Session::set_tempo_map` forwards to), with
/// `TempoMap` itself at the root: 120 BPM, then a time ramp to 180 over four
/// beats, lasts 1.6 s, so beat 4 is frame 76 800 at 48 kHz.
#[test]
fn a_tempo_map_is_built_and_published_through_the_facade() {
    let map: TempoMap = tutti::tempo::TempoMap::builder(Bpm::of(120.0))
        .ramp_time_to(Beat(4.0), Bpm::of(180.0))
        .build()
        .expect("a map");
    let rate = SampleRate::SR_48K;
    assert_eq!(map.view(rate).frame_at_or_after(Beat(4.0)), Frame(76_800));
    let transport = tutti::transport::Transport::new(rate);
    transport.settings().set_tempo_map(map);
}
