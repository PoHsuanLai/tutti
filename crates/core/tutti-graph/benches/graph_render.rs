//! The serial plan executor against fundsp's `Net`, on the same work.
//!
//! Three runtimes per shape:
//!
//! - **`net`** — fundsp's `Net`, running `AudioUnit`s: a `sine_hz` source, a
//!   chain or fan of `lowpass_hz` filters, and (for the fan) one summing unit.
//! - **`graph`** — the plan executor running **the same `AudioUnit`s** through
//!   the `Legacy` adapter, so it also pays the adapter's copy into and out of
//!   fundsp's 64-frame buffers. That is the executor's *worst* case against
//!   `Net`: a cost native nodes do not pay.
//! - **`native`** — the plan executor running nodes written directly against
//!   `Io`, with the same arithmetic as the fundsp units (the SVF's `tick`, the
//!   sum's loop). The one difference is the source: fundsp evaluates the sine
//!   with `wide`'s SIMD `sin`, the native node with `f32::sin`. A shape has one
//!   source, so that shifts every row of a group by about the same constant.
//!
//! `overhead/<n>` isolates the runtime's **fixed per-node cost**: a chain of
//! `n` nodes that do no work (a `Net` unit whose `process` is empty; a native
//! node that returns `Status::Modified` on its in-place channel; the same
//! empty unit behind `Legacy`, which still copies). The per-node figure is the
//! slope between `n = 1` and `n = 128`.
//!
//! The shapes mirror `tutti-nodes/benches/engine_render.rs`: `nodes/<depth>`
//! (a chain of filters off one source) and `block_size/<frames>` (a fixed
//! 8-filter chain), plus `fan/<width>` — one source into `width` parallel
//! filters summed back to one — which `engine_render` cannot express without
//! its transport and so does not have.
//!
//! `Net` is driven directly (`AudioUnit::process` in 64-frame chunks), not
//! through `Engine`, so neither side pays the transport or the device fold.
//!
//! ```text
//! cargo bench -p tutti-graph --bench graph_render
//! cargo bench -p tutti-graph --bench graph_render -- overhead
//! ```

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use fundsp::net::{Net, NodeId};
use fundsp::prelude32::{lowpass_hz, sine_hz};
use tutti_graph::{Cx, Editor, Executor, Io, Legacy, Node, Prepare, Shape, Status, Transport};
use tutti_node::buffer::{BufferMut, BufferRef, BufferVec};
use tutti_node::signal::{Signal, SignalFrame};
use tutti_node::{AudioUnit, MAX_BUFFER_SIZE};
use tutti_types::graph::{Edge, InPort, OutPort, Source};
use tutti_types::{ChannelLayout, NodeKey, SampleRate, Samples, Tail};

const SR: f64 = 48_000.0;
const MAX_BLOCK: usize = 1024;

// ---- legacy units (both `net` and `graph` run these) ------------------------

/// Sums every input onto one output — the fan's merge, as a plain unit so
/// both runtimes run the same code.
#[derive(Clone)]
struct SumUnit(usize);

impl AudioUnit for SumUnit {
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        output[0] = input.iter().sum();
    }
    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        let out = output.channel_f32_mut(0);
        out[..size].fill(0.0);
        for c in 0..self.0 {
            for (o, &i) in out[..size].iter_mut().zip(&input.channel_f32(c)[..size]) {
                *o += i;
            }
        }
    }
    fn inputs(&self) -> usize {
        self.0
    }
    fn outputs(&self) -> usize {
        1
    }
    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let mut out = SignalFrame::new(1);
        out.set(0, Signal::Latency(0.0));
        out
    }
    fn tail(&mut self) -> Tail {
        Tail::None
    }
    fn get_id(&self) -> u64 {
        0x5355_4d00
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>()
    }
}

/// One in, one out, and no work: the runtime's own cost, nothing else.
///
/// `copy` makes it a pass-through instead. `Net` runs it with `copy` off —
/// its floor. `Legacy` needs it on: an empty unit leaves the adapter's
/// output buffer at zero, the adapter reports that silence, and the
/// executor then skips the rest of the chain, which would time the silence
/// skip rather than the node call.
#[derive(Clone)]
struct NopUnit {
    copy: bool,
}

impl AudioUnit for NopUnit {
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        if self.copy {
            output[0] = input[0];
        }
    }
    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        if self.copy {
            output.channel_f32_mut(0)[..size].copy_from_slice(&input.channel_f32(0)[..size]);
        }
    }
    fn inputs(&self) -> usize {
        1
    }
    fn outputs(&self) -> usize {
        1
    }
    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let mut out = SignalFrame::new(1);
        out.set(0, Signal::Latency(0.0));
        out
    }
    fn tail(&mut self) -> Tail {
        Tail::None
    }
    fn get_id(&self) -> u64 {
        0x4e4f_5000
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>()
    }
}

fn cutoff(i: usize) -> f32 {
    // Vary the cutoff so nothing can be folded away as identical work.
    500.0 + (i as f32) * 7.0
}

// ---- native nodes (`native` runs these) -------------------------------------

/// `sine_hz(hz)`: phase accumulator, `sin(phase · τ)`.
struct NativeSine {
    hz: f32,
    phase: f32,
    dt: f32,
}

impl Node for NativeSine {
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

/// `lowpass_hz(cutoff, q)`: fundsp's `FixedSvf<f32, LowpassMode>` — the same
/// coefficients and the same `tick`, one sample at a time.
struct NativeLowpass {
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

impl NativeLowpass {
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

impl Node for NativeLowpass {
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
struct NativeSum(usize);

impl Node for NativeSum {
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
struct NativeNop;

impl Node for NativeNop {
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

/// A shape, built on every side.
enum Topo {
    /// A source into `depth` filters in series.
    Chain(usize),
    /// A source into `width` parallel filters, summed.
    Fan(usize),
    /// A global input through `n` no-op nodes.
    Nops(usize),
}

#[derive(Clone, Copy)]
enum Kind {
    Legacy,
    Native,
}

fn net_for(shape: &Topo) -> Net {
    let inputs = matches!(shape, Topo::Nops(_)) as usize;
    let mut net = Net::new(inputs, 1);
    let last: NodeId = match *shape {
        Topo::Chain(depth) => {
            let mut last = net.push(Box::new(sine_hz(440.0)));
            for i in 0..depth {
                let f = net.push(Box::new(lowpass_hz(cutoff(i), 0.7)));
                net.connect(last, 0, f, 0);
                last = f;
            }
            last
        }
        Topo::Fan(width) => {
            let src = net.push(Box::new(sine_hz(440.0)));
            let sum = net.push(Box::new(SumUnit(width)));
            for i in 0..width {
                let f = net.push(Box::new(lowpass_hz(cutoff(i), 0.7)));
                net.connect(src, 0, f, 0);
                net.connect(f, 0, sum, i);
            }
            sum
        }
        Topo::Nops(n) => {
            let first = net.push(Box::new(NopUnit { copy: false }));
            net.connect_input(0, first, 0);
            let mut last = first;
            for _ in 1..n {
                let f = net.push(Box::new(NopUnit { copy: false }));
                net.connect(last, 0, f, 0);
                last = f;
            }
            last
        }
    };
    net.pipe_output(last);
    net.set_sample_rate(SampleRate(SR));
    net.allocate();
    net
}

fn executor_for(shape: &Topo, kind: Kind) -> Executor {
    let prepare = Prepare::new(SampleRate(SR), Samples(MAX_BLOCK));
    let (mut ed, mut exec) = Editor::new(prepare);
    let native = matches!(kind, Kind::Native);
    let sine = || -> Box<dyn Node> {
        if native {
            Box::new(NativeSine {
                hz: 440.0,
                phase: 0.0,
                dt: 0.0,
            })
        } else {
            Box::new(Legacy::new(sine_hz(440.0)))
        }
    };
    let lowpass = |i: usize| -> Box<dyn Node> {
        if native {
            Box::new(NativeLowpass::new(cutoff(i), 0.7))
        } else {
            Box::new(Legacy::new(lowpass_hz(cutoff(i), 0.7)))
        }
    };
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
            ed.insert(src, "sine", sine());
            let mut last = src;
            for i in 0..depth {
                let k = NodeKey(1 + i as u64);
                ed.insert(k, "lowpass", lowpass(i));
                wire(&mut ed, k, 0, node(last));
                last = k;
            }
            last
        }
        Topo::Fan(width) => {
            let src = NodeKey(0);
            ed.insert(src, "sine", sine());
            let sum = NodeKey(u64::MAX);
            let s: Box<dyn Node> = if native {
                Box::new(NativeSum(width))
            } else {
                Box::new(Legacy::new(SumUnit(width)))
            };
            ed.insert(sum, "sum", s);
            for i in 0..width {
                let k = NodeKey(1 + i as u64);
                ed.insert(k, "lowpass", lowpass(i));
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
                let nop: Box<dyn Node> = if native {
                    Box::new(NativeNop)
                } else {
                    Box::new(Legacy::new(NopUnit { copy: true }))
                };
                ed.insert(k, "nop", nop);
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

fn render_net(net: &mut Net, frames: usize, input: &BufferVec, output: &mut BufferVec) {
    let mut done = 0;
    while done < frames {
        let n = (frames - done).min(MAX_BUFFER_SIZE);
        net.process(n, &input.buffer_ref(), &mut output.buffer_mut());
        done += n;
    }
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

fn compare(c: &mut Criterion, group: &str, cases: &[(String, Topo, usize)]) {
    let mut g = c.benchmark_group(group);
    for (label, shape, frames) in cases {
        let frames = *frames;
        g.throughput(Throughput::Elements(frames as u64));
        let mut net = net_for(shape);
        let mut input = BufferVec::new(1);
        input.channel_f32_mut(0).fill(0.25);
        let mut output = BufferVec::new(1);
        g.bench_with_input(BenchmarkId::new("net", label), &frames, |b, _| {
            b.iter(|| render_net(&mut net, frames, &input, black_box(&mut output)));
        });
        let graph_in = vec![0.25f32; MAX_BLOCK];
        let mut out = vec![0.0f32; MAX_BLOCK];
        for (name, kind) in [("graph", Kind::Legacy), ("native", Kind::Native)] {
            let mut exec = executor_for(shape, kind);
            g.bench_with_input(BenchmarkId::new(name, label), &frames, |b, _| {
                b.iter(|| render_graph(&mut exec, frames, &graph_in, black_box(&mut out)));
            });
        }
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
    compare(c, "nodes", &cases);
}

/// Block size at a fixed 8-filter chain: the per-callback overhead.
fn bench_block_size(c: &mut Criterion) {
    let cases: Vec<_> = [64usize, 128, 256, 512, 1024]
        .into_iter()
        .map(|f| (f.to_string(), Topo::Chain(8), f))
        .collect();
    compare(c, "block_size", &cases);
}

/// One source into `width` parallel filters, summed.
fn bench_fan(c: &mut Criterion) {
    let mut cases = Vec::new();
    for w in [4usize, 16, 64] {
        for frames in [64usize, 512] {
            cases.push((format!("{w}-wide/{frames}"), Topo::Fan(w), frames));
        }
    }
    compare(c, "fan", &cases);
}

/// The fixed per-node cost: chains of nodes that do nothing.
fn bench_overhead(c: &mut Criterion) {
    let mut cases = Vec::new();
    for n in [1usize, 128] {
        for frames in [64usize, 512] {
            cases.push((format!("{n}-nodes/{frames}"), Topo::Nops(n), frames));
        }
    }
    compare(c, "overhead", &cases);
}

criterion_group!(
    benches,
    bench_depth,
    bench_block_size,
    bench_fan,
    bench_overhead
);
criterion_main!(benches);
