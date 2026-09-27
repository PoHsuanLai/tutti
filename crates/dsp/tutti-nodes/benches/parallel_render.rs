//! The parallel executor against the serial one (doc 013 Phase 6), on this
//! crate's filters, at 1, 2, 4 and 8 participants.
//!
//! Shapes, each one block of `FRAMES` at 48 kHz, driven through the
//! `Executor` directly (no engine fold, no transport):
//!
//! - `wide/<lanes>x<depth>` — `lanes` independent chains of `depth`
//!   `SvfFilterNode`s, each off its own oscillator, summed by one
//!   `ChannelSumNode`: the shape that parallelises (every chain is one
//!   fused task, and `lanes` of them are ready at once).
//! - `deep/<depth>` — one chain of `depth` filters: nothing to spread (one
//!   task), so a pool must cost nothing here. The executor falls back to the
//!   serial walk for a one-task plan.
//! - `tiny/<lanes>` — `lanes` single cheap gains: tasks too small to be worth
//!   dispatching, where a pool can lose (what the cost model, doc 013 step 6,
//!   is for).
//!
//! `workers` is the participant count, the calling thread included; `1` is
//! the serial executor with no pool. The pool is `tutti_core::WorkerPool`
//! with its default waits, so a block's helpers are usually still spinning
//! from the last block (criterion runs blocks back to back); the wake cost
//! of a parked helper is not in these numbers.
//!
//! ```text
//! cargo bench -p tutti-nodes --bench parallel_render
//! ```
//!
//! The figures in doc 013 ("Phase 6") were taken on the machine named
//! there; they do not travel. Eight participants on a four-core machine
//! oversubscribe it and are reported as measured, not dropped.

use std::hint::black_box;
use std::sync::Arc;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use tutti_core::graph::{Edge, InPort, OutPort, Source};
use tutti_core::{ChannelLayout, Hz, NodeKey, SampleRate, Samples, WorkerPool, Q};
use tutti_graph::{Editor, Executor, Prepare, Transport, Unforkable};
use tutti_nodes::testing::Osc;
use tutti_nodes::{ChannelSumNode, SvfFilterNode, SvfType};

const SR: f64 = 48_000.0;
const FRAMES: usize = 256;
const WORKERS: [usize; 4] = [1, 2, 4, 8];

fn wire(ed: &mut Editor, node: NodeKey, port: u16, from: NodeKey) {
    ed.spec_mut().topology.edges.insert(
        InPort { node, port },
        Edge::Direct(Source::Node(OutPort {
            node: from,
            port: 0,
        })),
    );
}

/// `lanes` chains of `depth` filters (depth 0: a lone oscillator per lane),
/// summed; on a pool of `workers` participants when more than one.
fn wide(lanes: usize, depth: usize, workers: usize) -> Executor {
    let (mut ed, mut exec) = Editor::new(Prepare::new(SampleRate(SR), Samples(FRAMES)));
    if workers > 1 {
        exec.set_pool(Some(Arc::new(WorkerPool::new(workers))));
    }
    let sum = NodeKey(1_000_000);
    ed.insert(sum, "sum", ChannelSumNode::new(lanes, ChannelLayout::MONO));
    for lane in 0..lanes {
        let base = 1_000 * (lane as u64 + 1);
        let osc = NodeKey(base);
        ed.insert(osc, "osc", Osc::sine(Hz(110.0 + lane as f32)));
        let mut last = osc;
        for i in 0..depth {
            let k = NodeKey(base + 1 + i as u64);
            ed.insert(
                k,
                "svf",
                SvfFilterNode::<f64>::new(
                    SvfType::LowPass,
                    Hz(500.0 + (lane * depth + i) as f32 * 3.0),
                    Q(0.7),
                ),
            );
            wire(&mut ed, k, 0, last);
            last = k;
        }
        wire(&mut ed, sum, lane as u16, last);
    }
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort { node: sum, port: 0 })];
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    assert_eq!(exec.is_parallel(), workers > 1 && lanes > 1);
    // The editor is dropped here; the executor keeps its plan.
    exec
}

/// A gain: one multiply per sample, the cheapest node there is.
struct Gain(f32);

impl tutti_graph::Node for Gain {
    fn shape(&self) -> tutti_graph::Shape {
        tutti_graph::Shape::audio(ChannelLayout::MONO, ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(
        &mut self,
        _: &tutti_graph::Cx<'_>,
        mut io: tutti_graph::Io<'_>,
    ) -> tutti_graph::Status {
        let (ins, mut outs) = io.split();
        for (y, x) in outs.get(0).iter_mut().zip(ins.get(0)) {
            *y = x * self.0;
        }
        tutti_graph::Status::Modified
    }
    fn reset(&mut self) {}
}

/// `lanes` single gains off one input, summed.
fn tiny(lanes: usize, workers: usize) -> Executor {
    let (mut ed, mut exec) = Editor::new(Prepare::new(SampleRate(SR), Samples(FRAMES)));
    if workers > 1 {
        exec.set_pool(Some(Arc::new(WorkerPool::new(workers))));
    }
    ed.spec_mut().topology.inputs = ChannelLayout::MONO;
    let sum = NodeKey(1_000_000);
    ed.insert(sum, "sum", ChannelSumNode::new(lanes, ChannelLayout::MONO));
    for lane in 0..lanes {
        let k = NodeKey(lane as u64 + 1);
        ed.insert(k, "gain", Unforkable(Gain(0.5 + lane as f32 * 1e-3)));
        ed.spec_mut()
            .topology
            .edges
            .insert(InPort { node: k, port: 0 }, Edge::Direct(Source::Global(0)));
        wire(&mut ed, sum, lane as u16, k);
    }
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort { node: sum, port: 0 })];
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    exec
}

fn block(exec: &mut Executor, input: &[f32], out: &mut [f32]) {
    let t = Transport::default();
    exec.process(FRAMES, &t, &[input], &mut [out]);
}

fn group(c: &mut Criterion, name: &str, cases: &[(String, &dyn Fn(usize) -> Executor)]) {
    let mut g = c.benchmark_group(name);
    g.throughput(Throughput::Elements(FRAMES as u64));
    let input = vec![0.25f32; FRAMES];
    let mut out = vec![0.0f32; FRAMES];
    for (shape, build) in cases {
        for workers in WORKERS {
            let mut exec = build(workers);
            g.bench_with_input(BenchmarkId::new(shape, workers), &workers, |b, _| {
                b.iter(|| block(&mut exec, black_box(&input), black_box(&mut out)));
            });
        }
    }
    g.finish();
}

fn bench_wide(c: &mut Criterion) {
    let w8: &dyn Fn(usize) -> Executor = &|w| wide(8, 8, w);
    let w32: &dyn Fn(usize) -> Executor = &|w| wide(32, 8, w);
    let w64: &dyn Fn(usize) -> Executor = &|w| wide(64, 16, w);
    group(
        c,
        "wide",
        &[
            ("8x8".into(), w8),
            ("32x8".into(), w32),
            ("64x16".into(), w64),
        ],
    );
}

fn bench_deep(c: &mut Criterion) {
    let d: &dyn Fn(usize) -> Executor = &|w| wide(1, 128, w);
    group(c, "deep", &[("128".into(), d)]);
}

fn bench_tiny(c: &mut Criterion) {
    let t: &dyn Fn(usize) -> Executor = &|w| tiny(16, w);
    group(c, "tiny", &[("16".into(), t)]);
}

criterion_group!(benches, bench_wide, bench_deep, bench_tiny);
criterion_main!(benches);
