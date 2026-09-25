//! The headline claim, compiled and run: **one dependency, one `use` line,
//! a graph that renders.**
//!
//! Device-free, so it runs on every platform in CI. The smallest harness in
//! the repo (`tutti-core/tests/root_channel_layouts.rs`) is the shape this
//! borrows — a graph, its editor and executor, an engine, one block, and an
//! assertion that the block is not silence.
//!
//! What this cannot prove is that a *live* engine assembles in one import;
//! nothing in the engine offers that (see `examples/headless_engine.rs` and
//! the note in `src/lib.rs`). It proves the vocabulary arrives.

use tutti::prelude::*;

/// Everything below is reached through `tutti::` and nothing else. If the
/// façade stopped re-exporting something, this stops compiling.
///
/// (Until doc 013 Phase 3 PR 15 this built a `tutti::dsp::Net` and handed
/// its backend to `Engine::new`; the engine renders only the native graph
/// now, built here with `tutti::graph::GraphBuilder`.)
#[test]
fn one_import_renders_a_block() {
    let mut g = tutti::graph::GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let tone = g.add_unit(Box::new(tutti::nodes::testing::Osc::sine(Hz(440.0))));
    g.pipe_output(tone);
    let (mut editor, executor) = g
        .build(tutti::graph::Prepare::new(
            SampleRate(48_000.0),
            Samples(256),
        ))
        .expect("builds");

    let transport = tutti::core::Transport::new(48_000.0);
    let engine =
        tutti::core::Engine::new(&transport, &mut editor, executor).expect("within the limits");

    let mut out = vec![0.0f32; 256 * 2];
    engine.process(&mut InterleavedMut::new(&mut out, ChannelLayout::STEREO));

    assert!(
        out.iter().any(|&s| s != 0.0),
        "the graph must actually render — an assertion over silence would \
         pass against a façade that re-exported nothing useful"
    );
}

/// The measurement vocabulary arrives through the prelude, converting as
/// documented.
///
/// Mirrors `bevy-tutti`'s `prelude_surface.rs`, which makes the same claim
/// for the Bevy side. If `tutti-core`'s prelude ever narrowed, this names
/// which type went missing rather than failing somewhere downstream.
#[test]
fn the_measurement_vocabulary_arrives_and_converts() {
    assert!((Db(-6.0).to_amplitude().get() - 0.501).abs() < 0.01);
    assert_eq!(Seconds(1.0).to_samples(SampleRate(48_000.0)).get(), 48_000);
    assert!((Cents(1200.0).to_semitones().get() - 12.0).abs() < 1e-6);
    assert_eq!(ChannelLayout::STEREO.count(), 2);
}

/// The nodes and the node contract are reachable without naming their crates.
#[test]
fn the_dsp_library_and_the_node_contract_are_reachable() {
    // A `tutti-nodes` unit, built and driven through `tutti::` alone.
    let _lfo = tutti::nodes::Lfo::default();
    // The planar block buffers: `tutti-node`'s, at the engine root.
    let buf: tutti::core::BufferVec = tutti::core::BufferVec::new(2);
    assert_eq!(buf.buffer_ref().channels(), 2);
}

/// Offline export, end to end, through the façade only.
#[cfg(feature = "export")]
#[test]
fn a_graph_bounces_to_buffers_through_the_facade() {
    let rate = SampleRate(48_000.0);
    let mut g = tutti::graph::GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let node = g.add_unit(Box::new(tutti::nodes::testing::Const::frame(&[0.5, 0.5])));
    g.pipe_output(node);
    let (editor, executor) = g
        .build(tutti::export::RenderGraph::prepare(rate))
        .expect("builds");
    let graph = tutti::export::RenderGraph::new(editor, executor).expect("built together");

    let cfg = tutti::export::ExportConfig {
        render: tutti::export::RenderConfig {
            sample_rate: rate,
            duration_seconds: 0.01,
            ..Default::default()
        },
        ..Default::default()
    };
    let rendered = tutti::export::render_to_buffers(graph, &cfg, &tutti::core::FrozenClock)
        .expect("a DC graph renders");
    assert!(!rendered.planes.is_empty());
    assert!(rendered.planes[0].iter().all(|&s| (s - 0.5).abs() < 1e-6));
}
