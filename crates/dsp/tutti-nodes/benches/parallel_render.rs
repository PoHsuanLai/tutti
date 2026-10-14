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
//! - `tiny/<lanes>` — `lanes` single cheap gains off one input, summed:
//!   tasks too small to be worth dispatching. The gains declare
//!   `Cost::Light`, so the cost model (doc 013 step 6) packs them and keeps
//!   the block serial. `tiny/<lanes>-undeclared` is the same graph with
//!   gains that declare nothing: every gain its own task, spread over the
//!   pool, as part (a) ran every graph — where a pool loses.
//! - `mixed/<lanes>x<depth>x<fan>` — `lanes` chains of `depth` filters,
//!   each fanned out to `fan` light gains and summed back: heavy work with
//!   cheap siblings, where packing the siblings (and not dispatching each)
//!   matters. `-undeclared` again for the gains declaring nothing.
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

/// A gain: one multiply per sample, the cheapest node there is. It
/// declares `Cost::Light` when `light`, and nothing (`Cost::Heavy`, the
/// default) otherwise: the second is the part (a) behaviour, every gain a
/// task of its own and any two independent ones spread.
struct Gain {
    gain: f32,
    light: bool,
}

impl tutti_graph::Node for Gain {
    fn shape(&self) -> tutti_graph::Shape {
        let s = tutti_graph::Shape::audio(ChannelLayout::MONO, ChannelLayout::MONO);
        if self.light {
            s.with_cost(tutti_graph::Cost::Light)
        } else {
            s
        }
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(
        &mut self,
        _: &tutti_graph::Cx<'_>,
        mut io: tutti_graph::Io<'_>,
    ) -> tutti_graph::Status {
        let (ins, mut outs) = io.split();
        for (y, x) in outs.get(0).iter_mut().zip(ins.get(0)) {
            *y = x * self.gain;
        }
        tutti_graph::Status::Modified
    }
    fn reset(&mut self) {}
}

fn gain(ed: &mut Editor, k: NodeKey, i: usize, light: bool) {
    ed.insert(
        k,
        "gain",
        Unforkable(Gain {
            gain: 0.5 + i as f32 * 1e-3,
            light,
        }),
    );
}

/// `lanes` single gains off one input, summed.
fn tiny(lanes: usize, light: bool, workers: usize) -> Executor {
    let (mut ed, mut exec) = Editor::new(Prepare::new(SampleRate(SR), Samples(FRAMES)));
    if workers > 1 {
        exec.set_pool(Some(Arc::new(WorkerPool::new(workers))));
    }
    ed.spec_mut().topology.inputs = ChannelLayout::MONO;
    let sum = NodeKey(1_000_000);
    ed.insert(sum, "sum", ChannelSumNode::new(lanes, ChannelLayout::MONO));
    for lane in 0..lanes {
        let k = NodeKey(lane as u64 + 1);
        gain(&mut ed, k, lane, light);
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
    // The cost model keeps the declared-light graph on one thread (its
    // gains pack into two tasks of eight, four tasks in all, too cheap to
    // spread); the undeclared one spreads a task per gain, as part (a)
    // did.
    assert_eq!(exec.is_parallel(), workers > 1 && !light);
    exec
}

/// `lanes` chains of `depth` filters, each fanned out to `fan` gains that
/// a per-lane sum gathers, the lane sums summed: heavy chains with cheap
/// siblings hanging off them. Declared light, the cost model packs each
/// lane's gains; undeclared, each gain is a task.
fn mixed(lanes: usize, depth: usize, fan: usize, light: bool, workers: usize) -> Executor {
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
        let lane_sum = NodeKey(base + 999);
        ed.insert(
            lane_sum,
            "lane sum",
            ChannelSumNode::new(fan, ChannelLayout::MONO),
        );
        for j in 0..fan {
            let k = NodeKey(base + 500 + j as u64);
            gain(&mut ed, k, j, light);
            wire(&mut ed, k, 0, last);
            wire(&mut ed, lane_sum, j as u16, k);
        }
        wire(&mut ed, sum, lane as u16, lane_sum);
    }
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort { node: sum, port: 0 })];
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    assert_eq!(exec.is_parallel(), workers > 1);
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
    let light: &dyn Fn(usize) -> Executor = &|w| tiny(16, true, w);
    let undeclared: &dyn Fn(usize) -> Executor = &|w| tiny(16, false, w);
    group(
        c,
        "tiny",
        &[("16".into(), light), ("16-undeclared".into(), undeclared)],
    );
}

fn bench_mixed(c: &mut Criterion) {
    let light: &dyn Fn(usize) -> Executor = &|w| mixed(8, 4, 8, true, w);
    let undeclared: &dyn Fn(usize) -> Executor = &|w| mixed(8, 4, 8, false, w);
    group(
        c,
        "mixed",
        &[
            ("8x4x8".into(), light),
            ("8x4x8-undeclared".into(), undeclared),
        ],
    );
}

criterion_group!(benches, bench_wide, bench_deep, bench_tiny, bench_mixed);
criterion_main!(benches);
