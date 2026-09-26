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
use tutti_core::graph::{Edge, InPort, OutPort, Source};
use tutti_core::{ChannelLayout, Engine, InterleavedMut, MotionEvent, NodeKey, SampleRate};
use tutti_core::{Db, Hz, Q};
use tutti_core::{Samples, Transport};
use tutti_graph::{Editor, Legacy, Prepare};
use tutti_nodes::testing::Osc;
use tutti_nodes::{BusStripNode, EqBandNode, SvfFilterNode, SvfType};

const SR: f64 = 48_000.0;

/// Wire `from`'s output `out` into `node`'s input `port`.
fn wire(ed: &mut Editor, node: NodeKey, port: u16, from: NodeKey, out: u16) {
    ed.spec_mut().topology.edges.insert(
        InPort { node, port },
        Edge::Direct(Source::Node(OutPort {
            node: from,
            port: out,
        })),
    );
}

/// A rolling transport driving a three-node chain (this crate's units,
/// through `Legacy`), the shape `tests/rt_no_alloc_engine.rs` already pins
/// as allocation-free, on `outputs` global outputs (the strip's pair,
/// repeated).
fn chain_engine(outputs: usize) -> Engine {
    let transport = Transport::new(SR);
    let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(SR), Samples(512)));
    let [osc, eq, strip] = [1, 2, 3].map(NodeKey);
    ed.insert(osc, "osc", Legacy::new(Osc::sine(Hz(440.0))));
    ed.insert(
        eq,
        "eq",
        Legacy::new(EqBandNode::<f64>::new(
            SvfType::Bell,
            Hz(1_000.0),
            Q(1.0),
            Db(6.0),
        )),
    );
    ed.insert(
        strip,
        "strip",
        Legacy::new(BusStripNode::with_channels(ChannelLayout::STEREO)),
    );
    wire(&mut ed, eq, 0, osc, 0);
    wire(&mut ed, strip, 0, eq, 0);
    wire(&mut ed, strip, 1, eq, 0);
    ed.spec_mut().topology.outputs = (0..outputs)
        .map(|c| {
            Source::Node(OutPort {
                node: strip,
                port: (c % 2) as u16,
            })
        })
        .collect();
    ed.commit().expect("commits");
    let engine = Engine::new(&transport, &mut ed, exec).expect("within the limits");

    transport.settings.set_tempo(120.0);
    let _ = transport.motion.try_send(MotionEvent::Play);
    transport.motion.drain();
    engine
}

/// `depth` filters in series off one source — the "how many nodes" axis:
/// this crate's `Osc` and `SvfFilterNode`, through `Legacy`.
fn depth_engine(depth: usize) -> Engine {
    depth_graph_engine(depth, true)
}

fn render(engine: &Engine, buf: &mut [f32], layout: ChannelLayout) {
    engine.process(&mut InterleavedMut::new(buf, layout));
}

/// Block size, at a fixed graph. Isolates the fixed per-callback cost: if
/// elem/s at 64 frames is materially below elem/s at 1024, the difference is
/// what the engine pays *per callback* rather than per sample — the prologue
/// (installing commits, the transport walk, the fold) rather than the DSP.
/// The chain's units are `Legacy`, so every block renders chunk-major, in
/// 64-frame graph blocks.
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

// ---- node contract: `Legacy` against native (doc 013) -----------------------
//
// `backend/<runtime>/<depth>/<frames>`: the `nodes` shape — a source into
// `depth` filters in series, stereo device — through the whole `Engine`
// (motion drain, transport walk, render, fold, declick) on each node kind.
// (A `net` row ran fundsp's `Net` through the engine until doc 013 Phase 3
// PR 15 removed that backend.)
//
// - `graph-legacy`: this crate's `Osc` and `SvfFilterNode` through
//   `tutti_graph::Legacy`, which copies in and out of fundsp buffers (the
//   `nodes` group's engine);
// - `graph-native`: the native executor running nodes written against `Io`
//   (a phase-accumulator sine and an SVF lowpass with fundsp's `FixedSvf`
//   arithmetic, the `graph_render` bench's native pair). Not
//   `SvfFilterNode`'s code — no native port of it exists yet — so this row
//   prices the runtime with a filter of the same order of work.
//
// The graph engines are prepared for 512-frame blocks, so every row here is
// one executor block per device block.

/// `sine_hz`, against `Io`.
struct NativeSine {
    hz: f32,
    phase: f32,
    dt: f32,
}

impl tutti_graph::Node for NativeSine {
    fn shape(&self) -> tutti_graph::Shape {
        tutti_graph::Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO)
            .with_tail(tutti_core::Tail::Unbounded)
    }
    fn prepare(&mut self, p: &tutti_graph::Prepare) {
        self.dt = (1.0 / p.sample_rate().get()) as f32;
    }
    fn process(
        &mut self,
        _: &tutti_graph::Cx<'_>,
        mut io: tutti_graph::Io<'_>,
    ) -> tutti_graph::Status {
        let mut phase = self.phase;
        for o in io.output(0) {
            *o = (phase * std::f32::consts::TAU).sin();
            phase += self.hz * self.dt;
            phase -= phase.floor();
        }
        self.phase = phase;
        tutti_graph::Status::Modified
    }
    fn reset(&mut self) {
        self.phase = 0.0;
    }
}

/// An SVF lowpass with fundsp's `FixedSvf<f32, LowpassMode>` arithmetic,
/// against `Io`, in place.
struct NativeLowpass {
    cutoff: f32,
    q: f32,
    a: [f32; 3],
    ic: [f32; 2],
}

impl NativeLowpass {
    fn new(cutoff: f32, q: f32) -> Self {
        Self {
            cutoff,
            q,
            a: [0.0; 3],
            ic: [0.0; 2],
        }
    }
}

impl tutti_graph::Node for NativeLowpass {
    fn shape(&self) -> tutti_graph::Shape {
        tutti_graph::Shape::audio(ChannelLayout::MONO, ChannelLayout::MONO)
            .with_tail(tutti_core::Tail::Unknown)
            .with_in_place()
    }
    fn prepare(&mut self, p: &tutti_graph::Prepare) {
        let g = (std::f32::consts::PI * self.cutoff / p.sample_rate().get() as f32).tan();
        let k = 1.0 / self.q;
        let a1 = 1.0 / (1.0 + g * (g + k));
        let a2 = g * a1;
        self.a = [a1, a2, g * a2];
    }
    fn process(
        &mut self,
        _: &tutti_graph::Cx<'_>,
        mut io: tutti_graph::Io<'_>,
    ) -> tutti_graph::Status {
        let [a1, a2, a3] = self.a;
        let [mut ic1eq, mut ic2eq] = self.ic;
        io.channel(0).map(|v0| {
            let v3 = v0 - ic2eq;
            let v1 = a1 * ic1eq + a2 * v3;
            let v2 = ic2eq + a2 * ic1eq + a3 * v3;
            ic1eq = 2.0 * v1 - ic1eq;
            ic2eq = 2.0 * v2 - ic2eq;
            v2
        });
        self.ic = [ic1eq, ic2eq];
        tutti_graph::Status::Modified
    }
    fn reset(&mut self) {
        self.ic = [0.0; 2];
    }
}

/// The `nodes` shape on the native graph: `legacy` runs this crate's own
/// units through `Legacy`, otherwise the native pair.
fn depth_graph_engine(depth: usize, legacy: bool) -> Engine {
    use tutti_core::graph::{Edge, InPort, OutPort, Source};
    use tutti_core::NodeKey;
    let (mut ed, exec) = tutti_graph::Editor::new(tutti_graph::Prepare::new(
        SampleRate(SR),
        tutti_core::Samples(512),
    ));
    let src: Box<dyn tutti_graph::Node> = if legacy {
        tutti_graph::IntoNode::into_node(tutti_graph::Legacy::new(Osc::sine(Hz(440.0)))).0
    } else {
        Box::new(NativeSine {
            hz: 440.0,
            phase: 0.0,
            dt: 0.0,
        })
    };
    ed.insert(NodeKey(0), "sine", src);
    let mut last = NodeKey(0);
    for i in 0..depth {
        let cutoff = 500.0 + (i as f32) * 7.0;
        let f: Box<dyn tutti_graph::Node> =
            if legacy {
                tutti_graph::IntoNode::into_node(tutti_graph::Legacy::pure(
                    SvfFilterNode::<f64>::new(SvfType::LowPass, Hz(cutoff), Q(0.7)),
                ))
                .0
            } else {
                Box::new(NativeLowpass::new(cutoff, 0.7))
            };
        let k = NodeKey(1 + i as u64);
        ed.insert(k, "lowpass", f);
        ed.spec_mut().topology.edges.insert(
            InPort { node: k, port: 0 },
            Edge::Direct(Source::Node(OutPort {
                node: last,
                port: 0,
            })),
        );
        last = k;
    }
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort {
        node: last,
        port: 0,
    })];
    ed.commit().expect("commits");
    let engine = Engine::new(&Transport::new(SR), &mut ed, exec).expect("within the limits");
    // Install the plan outside the timed loop; the editor may go.
    let mut warm = vec![0.0f32; 512 * 2];
    render(&engine, &mut warm, ChannelLayout::STEREO);
    engine
}

/// `Legacy` units against native nodes, through the whole engine, on the
/// `nodes` shape. See the section comment above for what each row runs.
fn bench_backend(c: &mut Criterion) {
    let mut group = c.benchmark_group("backend");
    for depth in [1usize, 8, 128] {
        let engines = [
            ("graph-legacy", depth_graph_engine(depth, true)),
            ("graph-native", depth_graph_engine(depth, false)),
        ];
        for (name, engine) in &engines {
            for frames in [64usize, 512] {
                let mut buf = vec![0.0f32; frames * 2];
                group.throughput(Throughput::Elements(frames as u64));
                group.bench_with_input(
                    BenchmarkId::new(format!("{name}/{depth}"), frames),
                    &frames,
                    |b, _| b.iter(|| render(engine, black_box(&mut buf), ChannelLayout::STEREO)),
                );
            }
        }
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_block_size,
    bench_depth,
    bench_width,
    bench_transport_overhead,
    bench_backend
);
criterion_main!(benches);
