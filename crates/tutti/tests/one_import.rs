//! The headline claim, compiled and run: **one dependency, one `use` line,
//! a graph that renders.**
//!
//! Device-free, so it runs on every platform in CI. The smallest harness in
//! the repo (`tutti-core/tests/root_channel_layouts.rs`) is the shape this
//! borrows — a net, a backend, an engine, one block, and an assertion that
//! the block is not silence.
//!
//! What this cannot prove is that a *live* engine assembles in one import;
//! nothing in the engine offers that (see `examples/headless_engine.rs` and
//! the note in `src/lib.rs`). It proves the vocabulary arrives.

use tutti::prelude::*;

/// Everything below is reached through `tutti::` and nothing else. If the
/// façade stopped re-exporting something, this stops compiling.
#[test]
fn one_import_renders_a_block() {
    let mut net = tutti::dsp::Net::new(0, 2);
    let tone = net.push(Box::new(tutti::nodes::testing::Osc::sine(Hz(440.0))));
    net.pipe_output(tone);

    let engine = tutti::core::Engine::new(
        tutti::core::MotionFsm::new(tutti::core::TransportSettings::new()),
        net.backend(),
    );

    let mut out = vec![0.0f32; 256 * 2];
    engine.process(&mut InterleavedMut::new(&mut out, ChannelLayout::STEREO));

    assert!(
        out.iter().any(|&s| s != 0.0),
        "the graph must actually render — an assertion over silence would \
         pass against a façade that re-exported nothing useful"
    );
    // The backend borrows through the net, so the net outlives this scope.
    std::mem::forget(net);
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
    // The planar block buffers, from `tutti-node` via `tutti::dsp`.
    let buf = tutti::dsp::BufferArray::<tutti::dsp::U2>::new();
    assert_eq!(buf.buffer_ref().channels(), 2);
}

/// Offline export, end to end, through the façade only.
#[cfg(feature = "export")]
#[test]
fn a_graph_bounces_to_buffers_through_the_facade() {
    let mut net = tutti::dsp::Net::new(0, 2);
    let node = net.push(Box::new(tutti::nodes::testing::Const::frame(&[0.5, 0.5])));
    net.pipe_output(node);

    let cfg = tutti::export::ExportConfig {
        render: tutti::export::RenderConfig {
            sample_rate: SampleRate(48_000.0),
            duration_seconds: 0.01,
            ..Default::default()
        },
        ..Default::default()
    };
    let rendered = tutti::export::render_to_buffers(net, &cfg, &tutti::core::FrozenClock)
        .expect("a DC graph renders");
    assert!(!rendered.planes.is_empty());
    assert!(rendered.planes[0].iter().all(|&s| (s - 0.5).abs() < 1e-6));
}
