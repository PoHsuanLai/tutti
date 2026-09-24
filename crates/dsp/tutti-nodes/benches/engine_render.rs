//! The graph render, per block — the engine's hot path.
//!
//! `Engine::process` is `tutti-core`'s, but the graph it renders here is built
//! from this crate's nodes (`Osc`, `EqBandNode`, `BusStripNode`,
//! `SvfFilterNode`), so the numbers are for the filters the engine ships rather
//! than fundsp's. That is why the bench lives here: `tutti-core` cannot depend
//! on this crate without a cycle.
//!
//! # Reading these numbers
//!
//! Nanoseconds per block are not actionable. Every group here sets
//! `Throughput::Elements(frames)`, so criterion prints **elements per
//! second**, and the number that answers a question is:
//!
//! ```text
//! realtime multiple = elem/s ÷ 48_000
//! budget at 64 frames = measured time ÷ 1.333 ms
//! ```
//!
//! A 64-frame block at 48 kHz has 1.333 ms to be produced in. "How many
//! tracks can this engine carry" is *that* fraction, not a nanosecond count —
//! so `nodes/…` is the group to read, and the node count at which one block
//! costs a meaningful slice of 1.333 ms is the answer.
//!
//! # Where criterion is the wrong instrument
//!
//! Criterion suits steady-state, allocation-free, fixed-working-set code, and
//! this path is exactly that — `tests/rt_no_alloc_engine.rs` (beside this
//! bench, in `tutti-nodes`) *proves* the
//! precondition. It is the wrong tool where cost is dominated by allocation
//! churn or by the scheduler: `tutti-sampler`'s
//! `examples/profile_stretch_clone.rs` measured an **81× wall-clock spread**
//! on identical work and deliberately reports a median with a sampling
//! profiler instead, because criterion's outlier *rejection* would discard
//! precisely the samples that decide whether audio drops out. Anything with
//! that shape belongs in a harness of that kind, not here.
//!
//! # Running
//!
//! ```text
//! just bench                       # everything
//! cargo bench -p tutti-nodes --bench engine_render
//! just bench-save main             # a baseline on THIS machine
//! just bench-cmp main              # compare against it
//! ```
//!
//! Baselines are per-machine and do not travel; see `just bench-save`.

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use tutti_core::dsp::Net;
use tutti_core::{AudioUnit, Db, Hz, Q};
use tutti_core::{ChannelLayout, Engine, InterleavedMut, MotionEvent, SampleRate};
use tutti_core::{MotionFsm, Transport, TransportClock, TransportSettings};
use tutti_nodes::testing::Osc;
use tutti_nodes::{BusStripNode, EqBandNode, SvfFilterNode, SvfType};

const SR: f64 = 48_000.0;

/// Leak the net so its backend stays valid.
///
/// The backend borrows through the net, so the net must outlive it. Built once
/// per case in setup and never inside `b.iter`, so the leak is bounded by the
/// number of cases rather than by the iteration count — the trap this idiom
/// invites.
fn keep(net: Net) {
    let _: &'static Net = Box::leak(Box::new(net));
}

/// A rolling transport driving a four-node chain, the shape
/// `tests/rt_no_alloc_engine.rs` already pins as allocation-free.
fn chain_engine(outputs: usize) -> Engine {
    let transport = Transport::new(SR);
    let mut net = Net::new(0, outputs);
    net.push(Box::new(TransportClock::new(transport.clock_links(), SR)));
    {
        let inner = &mut net;
        inner.chain(Box::new(Osc::sine(Hz(440.0))));
        inner.chain(Box::new(EqBandNode::<f64>::new(
            SvfType::Bell,
            Hz(1_000.0),
            Q(1.0),
            Db(6.0),
        )));
        inner.chain(Box::new(BusStripNode::with_channels(ChannelLayout::STEREO)));
    }
    net.set_sample_rate(SampleRate(SR));
    let backend = net.backend();
    keep(net);

    transport.settings.set_tempo(120.0);
    let _ = transport.motion.try_send(MotionEvent::Play);
    transport.motion.drain();
    Engine::new(transport.motion.clone(), backend)
}

/// `depth` filters in series off one source — the "how many nodes" axis.
fn depth_engine(depth: usize) -> Engine {
    let mut net = Net::new(0, 2);
    let mut last = net.push(Box::new(Osc::sine(Hz(440.0))));
    for i in 0..depth {
        // Vary the cutoff so nothing can be folded away as identical work.
        let f = net.push(Box::new(SvfFilterNode::<f64>::new(
            SvfType::LowPass,
            Hz(500.0 + (i as f32) * 7.0),
            Q(0.7),
        )));
        net.connect(last, 0, f, 0);
        last = f;
    }
    net.pipe_output(last);
    net.set_sample_rate(SampleRate(SR));
    let backend = net.backend();
    keep(net);
    Engine::new(MotionFsm::new(TransportSettings::new()), backend)
}

fn render(engine: &Engine, buf: &mut [f32], layout: ChannelLayout) {
    engine.process(&mut InterleavedMut::new(buf, layout));
}

/// Block size, at a fixed graph. Isolates the fixed per-callback cost: if
/// elem/s at 64 frames is materially below elem/s at 1024, the difference is
/// what the engine pays *per callback* rather than per sample — the prologue
/// (`backend.pump()`, the stack `BufferArray` zeroing) rather than the DSP.
fn bench_block_size(c: &mut Criterion) {
    let mut group = c.benchmark_group("block_size");
    let engine = chain_engine(2);
    for frames in [64usize, 128, 256, 512, 1024] {
        let mut buf = vec![0.0f32; frames * 2];
        group.throughput(Throughput::Elements(frames as u64));
        group.bench_with_input(BenchmarkId::from_parameter(frames), &frames, |b, _| {
            b.iter(|| render(&engine, black_box(&mut buf), ChannelLayout::STEREO));
        });
    }
    group.finish();
}

/// **The "how many nodes" answer.** Linear in `depth` by construction; a
/// superlinear fit is itself the finding — cache pressure, or an accidental
/// quadratic in the graph walk.
fn bench_depth(c: &mut Criterion) {
    let mut group = c.benchmark_group("nodes");
    for depth in [1usize, 8, 32, 128, 512] {
        let engine = depth_engine(depth);
        for frames in [64usize, 512] {
            let mut buf = vec![0.0f32; frames * 2];
            group.throughput(Throughput::Elements(frames as u64));
            group.bench_with_input(
                BenchmarkId::new(format!("{depth}-nodes"), frames),
                &frames,
                |b, _| b.iter(|| render(&engine, black_box(&mut buf), ChannelLayout::STEREO)),
            );
        }
    }
    group.finish();
}

/// Device width. Prices the per-frame gather and `fold_frame` at the root:
/// the graph work is identical, only the fold width changes.
fn bench_width(c: &mut Criterion) {
    let mut group = c.benchmark_group("width");
    const FRAMES: usize = 256;
    for outputs in [1usize, 2, 6, 8] {
        let engine = chain_engine(outputs);
        let mut buf = vec![0.0f32; FRAMES * outputs];
        let layout = ChannelLayout::from(outputs);
        group.throughput(Throughput::Elements(FRAMES as u64));
        group.bench_with_input(BenchmarkId::from_parameter(outputs), &outputs, |b, _| {
            b.iter(|| render(&engine, black_box(&mut buf), layout));
        });
    }
    group.finish();
}

/// `process` (motion drain + render + declick) against `process_segment`
/// (render only). The delta is what the transport costs per block.
fn bench_transport_overhead(c: &mut Criterion) {
    let mut group = c.benchmark_group("transport");
    const FRAMES: usize = 256;
    let engine = chain_engine(2);
    let mut buf = vec![0.0f32; FRAMES * 2];
    group.throughput(Throughput::Elements(FRAMES as u64));
    group.bench_function("process", |b| {
        b.iter(|| render(&engine, black_box(&mut buf), ChannelLayout::STEREO));
    });
    group.bench_function("process_segment", |b| {
        b.iter(|| {
            engine.process_segment(&mut InterleavedMut::new(
                black_box(&mut buf),
                ChannelLayout::STEREO,
            ))
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_block_size,
    bench_depth,
    bench_width,
    bench_transport_overhead
);
criterion_main!(benches);
