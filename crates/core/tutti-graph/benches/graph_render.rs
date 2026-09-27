//! The serial plan executor, on a filter chain, a fan and a do-nothing chain.
//!
//! The nodes are written directly against `Io`: a sine source, a chain or
//! fan of two-pole lowpass filters (the SVF's `tick`), and (for the fan) one
//! summing node. Until doc 013 Phase 5 each shape also had a `net` row,
//! fundsp's `Net` running the same arithmetic as `AudioUnit`s; `Net` is
//! deleted, so the `graph` row is what is left, and the history of the
//! comparison (the figures it recorded) is in doc 013.
//!
//! `overhead/<n>` isolates the executor's **fixed per-node cost**: a chain of
//! `n` nodes that do no work (a node that returns `Status::Modified` on its
//! in-place channel). The per-node figure is the slope between `n = 1` and
//! `n = 128`. `overhead_form/<form>` does the same for each of the
//! executor's borrow forms: the one-channel and stereo direct forms, the
//! audio walk and the general walk.
//!
//! The shapes mirror `tutti-nodes/benches/engine_render.rs`: `nodes/<depth>`
//! (a chain of filters off one source) and `block_size/<frames>` (a fixed
//! 8-filter chain), plus `fan/<width>` — one source into `width` parallel
//! filters summed back to one — which `engine_render` cannot express without
//! its transport and so does not have.
//!
//! The executor is driven directly, not through `Engine`, so it pays neither
//! the transport nor the device fold.
//!
//! ```text
//! cargo bench -p tutti-graph --bench graph_render
//! cargo bench -p tutti-graph --bench graph_render -- overhead
//! ```

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use tutti_graph::{Cx, Editor, Executor, Io, Node, Prepare, Shape, Status, Transport, Unforkable};
use tutti_types::graph::{Edge, InPort, OutPort, Source};
use tutti_types::{ChannelLayout, NodeKey, SampleRate, Samples, Tail};

const SR: f64 = 48_000.0;
const MAX_BLOCK: usize = 1024;

fn cutoff(i: usize) -> f32 {
    // Vary the cutoff so nothing can be folded away as identical work.
    500.0 + (i as f32) * 7.0
}

// ---- graph nodes -------------------------------------------------------------

/// A sine: phase accumulator, `sin(phase · τ)` (fundsp's `sine_hz`, which
/// the `net` row ran).
struct Sine {
    hz: f32,
    phase: f32,
    dt: f32,
}

impl Node for Sine {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO).with_tail(Tail::Unbounded)
    }
    fn prepare(&mut self, p: &Prepare) {
        self.dt = (1.0 / p.sample_rate().get()) as f32;
    }
    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let mut phase = self.phase;
        for o in io.output(0) {
            *o = (phase * std::f32::consts::TAU).sin();
            phase += self.hz * self.dt;
            phase -= phase.floor();
        }
        self.phase = phase;
        Status::Modified
    }
    fn reset(&mut self) {
        self.phase = 0.0;
    }
}

/// A two-pole lowpass: the coefficients and `tick` of fundsp's
/// `lowpass_hz(cutoff, q)` (`FixedSvf<f32, LowpassMode>`), one sample at a
/// time.
struct Lowpass {
    cutoff: f32,
    q: f32,
    a1: f32,
    a2: f32,
    a3: f32,
    m0: f32,
    m1: f32,
    m2: f32,
    ic1eq: f32,
    ic2eq: f32,
}

impl Lowpass {
    fn new(cutoff: f32, q: f32) -> Self {
        let mut f = Self {
            cutoff,
            q,
            a1: 0.0,
            a2: 0.0,
            a3: 0.0,
            m0: 0.0,
            m1: 0.0,
            m2: 1.0,
            ic1eq: 0.0,
            ic2eq: 0.0,
        };
        f.coefs(SR as f32);
        f
    }

    fn coefs(&mut self, sr: f32) {
        let g = (std::f32::consts::PI * self.cutoff / sr).tan();
        let k = 1.0 / self.q;
        self.a1 = 1.0 / (1.0 + g * (g + k));
        self.a2 = g * self.a1;
        self.a3 = g * self.a2;
    }
}

impl Node for Lowpass {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::MONO, ChannelLayout::MONO)
            .with_tail(Tail::Unknown)
            .with_in_place()
    }
    fn prepare(&mut self, p: &Prepare) {
        self.coefs(p.sample_rate().get() as f32);
    }
    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let (mut ic1eq, mut ic2eq) = (self.ic1eq, self.ic2eq);
        let (a1, a2, a3, m0, m1, m2) = (self.a1, self.a2, self.a3, self.m0, self.m1, self.m2);
        io.channel(0).map(|v0| {
            let v3 = v0 - ic2eq;
            let v1 = a1 * ic1eq + a2 * v3;
            let v2 = ic2eq + a2 * ic1eq + a3 * v3;
            ic1eq = 2.0 * v1 - ic1eq;
            ic2eq = 2.0 * v2 - ic2eq;
            m0 * v0 + m1 * v1 + m2 * v2
        });
        self.ic1eq = ic1eq;
        self.ic2eq = ic2eq;
        Status::Modified
    }
    fn reset(&mut self) {
        self.ic1eq = 0.0;
        self.ic2eq = 0.0;
    }
}

/// `SumUnit`'s loop, against `Io`.
struct Sum(usize);

impl Node for Sum {
    fn shape(&self) -> Shape {
        Shape::audio(
            ChannelLayout::from_count(self.0 as u16),
            ChannelLayout::MONO,
        )
    }
    fn prepare(&mut self, _p: &Prepare) {}
    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let (ins, mut outs) = io.split();
        let out = outs.get(0);
        out.fill(0.0);
        for c in 0..self.0 {
            for (o, &i) in out.iter_mut().zip(ins.get(c)) {
                *o += i;
            }
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// No work: its one channel is in place, so its output already holds its
/// input.
struct Nop;

impl Node for Nop {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::MONO, ChannelLayout::MONO).with_in_place()
    }
    fn prepare(&mut self, _p: &Prepare) {}
    fn process(&mut self, _cx: &Cx<'_>, io: Io<'_>) -> Status {
        debug_assert!(io.in_place().get(0), "the chain aliases every link");
        Status::Modified
    }
    fn reset(&mut self) {}
}

// ---- shapes -----------------------------------------------------------------

/// A shape.
enum Topo {
    /// A source into `depth` filters in series.
    Chain(usize),
    /// A source into `width` parallel filters, summed.
    Fan(usize),
    /// A global input through `n` no-op nodes.
    Nops(usize),
}

fn executor_for(shape: &Topo) -> Executor {
    let prepare = Prepare::new(SampleRate(SR), Samples(MAX_BLOCK));
    let (mut ed, mut exec) = Editor::new(prepare);
    let sine = || -> Box<dyn Node> {
        Box::new(Sine {
            hz: 440.0,
            phase: 0.0,
            dt: 0.0,
        })
    };
    let lowpass = |i: usize| -> Box<dyn Node> { Box::new(Lowpass::new(cutoff(i), 0.7)) };
    let wire = |ed: &mut Editor, sink: NodeKey, port: u16, from: Source| {
        ed.spec_mut()
            .topology
            .edges
            .insert(InPort { node: sink, port }, Edge::Direct(from));
    };
    let node = |k: NodeKey| Source::Node(OutPort { node: k, port: 0 });
    let last = match *shape {
        Topo::Chain(depth) => {
            let src = NodeKey(0);
            ed.insert(src, "sine", Unforkable(sine()));
            let mut last = src;
            for i in 0..depth {
                let k = NodeKey(1 + i as u64);
                ed.insert(k, "lowpass", Unforkable(lowpass(i)));
                wire(&mut ed, k, 0, node(last));
                last = k;
            }
            last
        }
        Topo::Fan(width) => {
            let src = NodeKey(0);
            ed.insert(src, "sine", Unforkable(sine()));
            let sum = NodeKey(u64::MAX);
            let s: Box<dyn Node> = Box::new(Sum(width));
            ed.insert(sum, "sum", Unforkable(s));
            for i in 0..width {
                let k = NodeKey(1 + i as u64);
                ed.insert(k, "lowpass", Unforkable(lowpass(i)));
                wire(&mut ed, k, 0, node(src));
                wire(&mut ed, sum, i as u16, node(k));
            }
            sum
        }
        Topo::Nops(n) => {
            ed.spec_mut().topology.inputs = ChannelLayout::MONO;
            let mut from = Source::Global(0);
            let mut last = NodeKey(0);
            for i in 0..n {
                let k = NodeKey(i as u64);
                let nop: Box<dyn Node> = Box::new(Nop);
                ed.insert(k, "nop", Unforkable(nop));
                wire(&mut ed, k, 0, from);
                from = node(k);
                last = k;
            }
            last
        }
    };
    ed.spec_mut().topology.outputs = vec![node(last)];
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    exec
}

fn render_graph(exec: &mut Executor, frames: usize, input: &[f32], out: &mut [f32]) {
    let inputs: &[&[f32]] = if exec.plan().is_some_and(|p| p.global_inputs() > 0) {
        &[input]
    } else {
        &[]
    };
    exec.process(
        frames,
        &Transport::default(),
        inputs,
        &mut [&mut out[..frames]],
    );
}

fn shapes(c: &mut Criterion, group: &str, cases: &[(String, Topo, usize)]) {
    let mut g = c.benchmark_group(group);
    for (label, shape, frames) in cases {
        let frames = *frames;
        g.throughput(Throughput::Elements(frames as u64));
        let graph_in = vec![0.25f32; MAX_BLOCK];
        let mut out = vec![0.0f32; MAX_BLOCK];
        let mut exec = executor_for(shape);
        g.bench_with_input(BenchmarkId::new("graph", label), &frames, |b, _| {
            b.iter(|| render_graph(&mut exec, frames, &graph_in, black_box(&mut out)));
        });
    }
    g.finish();
}

/// The "how many nodes" axis: a chain of `depth` filters.
fn bench_depth(c: &mut Criterion) {
    let mut cases = Vec::new();
    for depth in [1usize, 8, 32, 128, 512] {
        for frames in [64usize, 512] {
            cases.push((
                format!("{depth}-nodes/{frames}"),
                Topo::Chain(depth),
                frames,
            ));
        }
    }
    shapes(c, "nodes", &cases);
}

/// Block size at a fixed 8-filter chain: the per-callback overhead.
fn bench_block_size(c: &mut Criterion) {
    let cases: Vec<_> = [64usize, 128, 256, 512, 1024]
        .into_iter()
        .map(|f| (f.to_string(), Topo::Chain(8), f))
        .collect();
    shapes(c, "block_size", &cases);
}

/// One source into `width` parallel filters, summed.
fn bench_fan(c: &mut Criterion) {
    let mut cases = Vec::new();
    for w in [4usize, 16, 64] {
        for frames in [64usize, 512] {
            cases.push((format!("{w}-wide/{frames}"), Topo::Fan(w), frames));
        }
    }
    shapes(c, "fan", &cases);
}

/// The fixed per-node cost: chains of nodes that do nothing.
fn bench_overhead(c: &mut Criterion) {
    let mut cases = Vec::new();
    for n in [1usize, 128] {
        for frames in [64usize, 512] {
            cases.push((format!("{n}-nodes/{frames}"), Topo::Nops(n), frames));
        }
    }
    shapes(c, "overhead", &cases);
}

// ---- per-form overhead (graph only) -----------------------------------------

/// A do-nothing node of a chosen shape, to time each of the executor's
/// borrow forms: `in_place` aliases every channel (otherwise the node writes
/// nothing, so its output holds stale samples the bench never reads), and
/// `events` adds unconnected event inputs, which moves it to the general
/// path.
struct FormNop {
    width: u16,
    in_place: bool,
    events: u16,
}

impl Node for FormNop {
    fn shape(&self) -> Shape {
        let ch = ChannelLayout::from_count(self.width);
        let s = Shape::audio(ch, ch).with_events(self.events, 0);
        if self.in_place {
            s.with_in_place()
        } else {
            s
        }
    }
    fn prepare(&mut self, _p: &Prepare) {}
    fn process(&mut self, _cx: &Cx<'_>, _io: Io<'_>) -> Status {
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// A chain of `n` [`FormNop`]s like `nop`, fed by as many global inputs.
fn form_executor(n: usize, nop: impl Fn() -> FormNop) -> Executor {
    let prepare = Prepare::new(SampleRate(SR), Samples(MAX_BLOCK));
    let (mut ed, mut exec) = Editor::new(prepare);
    let width = nop().width;
    ed.spec_mut().topology.inputs = ChannelLayout::from_count(width);
    for i in 0..n {
        let k = NodeKey(i as u64);
        ed.insert(k, "nop", Unforkable(Box::new(nop()) as Box<dyn Node>));
        for c in 0..width {
            let from = if i == 0 {
                Source::Global(c)
            } else {
                Source::Node(OutPort {
                    node: NodeKey(i as u64 - 1),
                    port: c,
                })
            };
            ed.spec_mut()
                .topology
                .edges
                .insert(InPort { node: k, port: c }, Edge::Direct(from));
        }
    }
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort {
        node: NodeKey(n as u64 - 1),
        port: 0,
    })];
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    exec
}

/// The fixed per-node cost of each borrow form, at 64 frames: the slope
/// between 1 and 128 nodes, as in `overhead`. `in-place` is the `overhead`
/// group's graph row; `split` is one channel in two slots; `stereo` and
/// `stereo-split` are two channels in place and in two slots each (the
/// stereo direct form); `audio` is three in-place channels (the presorted
/// borrow walk); `general` is one in-place channel plus an unconnected event
/// input (the walk with event tables).
fn bench_overhead_forms(c: &mut Criterion) {
    let mut g = c.benchmark_group("overhead_form");
    let frames = 64;
    let forms: [(&str, fn() -> FormNop); 6] = [
        ("in-place", || FormNop {
            width: 1,
            in_place: true,
            events: 0,
        }),
        ("split", || FormNop {
            width: 1,
            in_place: false,
            events: 0,
        }),
        ("stereo", || FormNop {
            width: 2,
            in_place: true,
            events: 0,
        }),
        ("stereo-split", || FormNop {
            width: 2,
            in_place: false,
            events: 0,
        }),
        ("audio", || FormNop {
            width: 3,
            in_place: true,
            events: 0,
        }),
        ("general", || FormNop {
            width: 1,
            in_place: true,
            events: 1,
        }),
    ];
    let input = vec![0.25f32; MAX_BLOCK];
    let mut out = vec![0.0f32; MAX_BLOCK];
    for (name, nop) in forms {
        for n in [1usize, 128] {
            let mut exec = form_executor(n, nop);
            let width = nop().width as usize;
            g.bench_function(BenchmarkId::new(name, format!("{n}-nodes/{frames}")), |b| {
                b.iter(|| {
                    let ins: [&[f32]; 3] = [&input[..frames], &input[..frames], &input[..frames]];
                    exec.process(
                        frames,
                        &Transport::default(),
                        &ins[..width],
                        &mut [black_box(&mut out[..frames])],
                    );
                });
            });
        }
    }
    g.finish();
}

criterion_group!(
    benches,
    bench_depth,
    bench_block_size,
    bench_fan,
    bench_overhead,
    bench_overhead_forms
);
criterion_main!(benches);
