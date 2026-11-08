//! The **whole** output callback, not just the graph render.
//!
//! `tutti-nodes`' `engine_render` bench measures `Engine::process`. That is
//! not what a sound card calls. The real callback also clamps to
//! `MAX_FRAMES`, zero-fills the mix and converts to the device's sample
//! format. [`OutputBlock`] runs all of that without a live stream, so it can be measured here.
//!
//! **The case that justifies this file is `callback/vs_render`.** Its delta
//! against `engine_render` is the conversion tax the engine
//! bench cannot see. If that tax is a large fraction of the block budget,
//! optimising DSP is the wrong place to look.
//!
//! Read the numbers the way `engine_render`'s header describes: elem/s ÷
//! 48 000 is the realtime multiple, and at 64 frames the budget is 1.333 ms.

use std::hint::black_box;
use std::sync::Arc;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use tutti_cpal::{AudioCallbackState, OutputBlock, process_audio};
use tutti_engine::Engine;
use tutti_graph::{Editor, Prepare, TapReader};
use tutti_nodes::testing::Osc;
use tutti_transport::Transport;
use tutti_types::SampleRate;
use tutti_types::graph::{OutPort, Source};
use tutti_types::{ChannelLayout, Hz, InterleavedMut, NodeKey, Samples, TapAlign, TapId};

const SR: f64 = 48_000.0;

fn state(outputs: usize, tap_open: bool) -> (Arc<AudioCallbackState>, Option<TapReader>) {
    let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(SR), Samples(512)));
    ed.insert(NodeKey(1), "sine", Osc::sine(Hz(440.0)));
    ed.spec_mut().topology.outputs = vec![
        Source::Node(OutPort {
            node: NodeKey(1),
            port: 0,
        });
        outputs
    ];
    let tap = tap_open.then(|| {
        let sources = ed.spec().topology.outputs.clone();
        ed.open_tap(TapId(0), sources, TapAlign::ToOutput, Samples(8192))
            .expect("the sources exist")
    });
    ed.commit().expect("commits");
    let engine = Engine::new(&Transport::new(SR), &mut ed, exec).expect("within the limits");
    // Built once per case in setup, so the leak is bounded by case count
    // rather than iteration count.
    Box::leak(Box::new(ed));

    (Arc::new(AudioCallbackState::new(engine)), tap)
}

/// The full callback across block sizes and device widths.
fn bench_callback(c: &mut Criterion) {
    let mut group = c.benchmark_group("callback");
    for outputs in [2usize, 6] {
        let (st, _cons) = state(outputs, false);
        let mut block = OutputBlock::new(st, ChannelLayout::from(outputs));
        for frames in [64usize, 256, 512] {
            let mut data = vec![0.0f32; frames * outputs];
            group.throughput(Throughput::Elements(frames as u64));
            group.bench_with_input(
                BenchmarkId::new(format!("{outputs}ch-f32"), frames),
                &frames,
                |b, _| b.iter(|| block.render(black_box(&mut data))),
            );
        }
    }
    group.finish();
}

/// **The payoff case.** `process_audio` alone against the full `render`, at
/// the same width and block size. The difference is the fold, the metering
/// and the format conversion — everything a graph-only benchmark omits.
fn bench_vs_render(c: &mut Criterion) {
    let mut group = c.benchmark_group("callback/vs_render");
    const FRAMES: usize = 256;

    let (st, _c1) = state(2, false);
    let mut mix = vec![0.0f32; FRAMES * 2];
    group.throughput(Throughput::Elements(FRAMES as u64));
    group.bench_function("process_audio_only", |b| {
        b.iter(|| {
            process_audio(
                &st,
                &mut InterleavedMut::new(black_box(&mut mix), ChannelLayout::STEREO),
            )
        });
    });

    let (st2, _c2) = state(2, false);
    let mut block = OutputBlock::new(st2, ChannelLayout::STEREO);
    let mut data = vec![0.0f32; FRAMES * 2];
    group.bench_function("full_callback_f32", |b| {
        b.iter(|| block.render(black_box(&mut data)));
    });

    // The conversion half, isolated: same work, an integer destination.
    let (st3, _c3) = state(2, false);
    let mut block16 = OutputBlock::new(st3, ChannelLayout::STEREO);
    let mut data16 = vec![0i16; FRAMES * 2];
    group.bench_function("full_callback_i16", |b| {
        b.iter(|| block16.render(black_box(&mut data16)));
    });

    group.finish();
}

/// What an open tap costs the audio thread.
///
/// A closed tap is not in the plan, so it costs nothing. This prices the
/// graph's tap op when it is open on both outputs, aligned and never drained
/// (the ring fills and the op counts what it drops): the figure a host
/// recording the master bus pays.
fn bench_tap(c: &mut Criterion) {
    let mut group = c.benchmark_group("callback/tap");
    const FRAMES: usize = 256;
    group.throughput(Throughput::Elements(FRAMES as u64));

    for (label, open) in [("closed", false), ("open", true)] {
        let (st, cons) = state(2, open);
        let mut block = OutputBlock::new(st, ChannelLayout::STEREO);
        let mut data = vec![0.0f32; FRAMES * 2];
        group.bench_function(label, |b| {
            b.iter(|| block.render(black_box(&mut data)));
        });
        // Held so an open tap keeps its reader for the whole measurement; a
        // dropped reader would make the op write nothing and quietly measure
        // the `closed` case twice.
        drop(cons);
    }
    group.finish();
}

criterion_group!(benches, bench_callback, bench_vs_render, bench_tap);
criterion_main!(benches);
