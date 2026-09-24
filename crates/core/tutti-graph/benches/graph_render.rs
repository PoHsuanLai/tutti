//! The serial plan executor against fundsp's `Net`, on the same nodes.
//!
//! Both sides render **identical `AudioUnit`s** — a `sine_hz` source, a chain
//! or fan of `lowpass_hz` filters, and (for the fan) one summing unit — so the
//! difference is the graph runtime alone: `Net`'s vertex walk against the
//! plan's op walk plus the `Legacy` adapter's copy into and out of fundsp's
//! 64-frame buffers. That copy is a cost the native nodes of doc 013 Phase 4
//! do not pay, so these numbers are the executor's *worst* case against
//! `Net`, not its steady state.
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
//! ```

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use fundsp::net::{Net, NodeId};
use fundsp::prelude32::{lowpass_hz, sine_hz};
use tutti_graph::{Editor, Executor, Legacy, Prepare, Transport};
use tutti_node::buffer::{BufferMut, BufferRef, BufferVec};
use tutti_node::signal::{Signal, SignalFrame};
use tutti_node::{AudioUnit, MAX_BUFFER_SIZE};
use tutti_types::graph::{Edge, InPort, OutPort, Source};
use tutti_types::{NodeKey, SampleRate, Samples, Tail};

const SR: f64 = 48_000.0;
const MAX_BLOCK: usize = 1024;

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

fn filter(i: usize) -> impl AudioUnit + 'static {
    // Vary the cutoff so nothing can be folded away as identical work.
    lowpass_hz(500.0 + (i as f32) * 7.0, 0.7)
}

/// A shape, built on both sides.
enum Shape {
    Chain(usize),
    Fan(usize),
}

fn net_for(shape: &Shape) -> Net {
    let mut net = Net::new(0, 1);
    let src = net.push(Box::new(sine_hz(440.0)));
    let last: NodeId = match *shape {
        Shape::Chain(depth) => {
            let mut last = src;
            for i in 0..depth {
                let f = net.push(Box::new(filter(i)));
                net.connect(last, 0, f, 0);
                last = f;
            }
            last
        }
        Shape::Fan(width) => {
            let sum = net.push(Box::new(SumUnit(width)));
            for i in 0..width {
                let f = net.push(Box::new(filter(i)));
                net.connect(src, 0, f, 0);
                net.connect(f, 0, sum, i);
            }
            sum
        }
    };
    net.pipe_output(last);
    net.set_sample_rate(SampleRate(SR));
    net.allocate();
    net
}

fn executor_for(shape: &Shape) -> Executor {
    let prepare = Prepare::new(SampleRate(SR), Samples(MAX_BLOCK));
    let mut ed = Editor::new(prepare);
    let src = NodeKey(0);
    ed.insert(src, "sine", Legacy::new(sine_hz(440.0)));
    let wire = |ed: &mut Editor, sink: NodeKey, port: u16, from: NodeKey| {
        ed.spec_mut().topology.edges.insert(
            InPort { node: sink, port },
            Edge::Direct(Source::Node(OutPort {
                node: from,
                port: 0,
            })),
        );
    };
    let last = match *shape {
        Shape::Chain(depth) => {
            let mut last = src;
            for i in 0..depth {
                let k = NodeKey(1 + i as u64);
                ed.insert(k, "lowpass", Legacy::new(filter(i)));
                wire(&mut ed, k, 0, last);
                last = k;
            }
            last
        }
        Shape::Fan(width) => {
            let sum = NodeKey(u64::MAX);
            ed.insert(sum, "sum", Legacy::new(SumUnit(width)));
            for i in 0..width {
                let k = NodeKey(1 + i as u64);
                ed.insert(k, "lowpass", Legacy::new(filter(i)));
                wire(&mut ed, k, 0, src);
                wire(&mut ed, sum, i as u16, k);
            }
            sum
        }
    };
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort {
        node: last,
        port: 0,
    })];
    let mut exec = Executor::new(prepare);
    let done = exec.apply(ed.commit().expect("the bench graph compiles"));
    ed.reclaim(done);
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

fn render_graph(exec: &mut Executor, frames: usize, out: &mut [f32]) {
    exec.process(
        frames,
        &Transport::default(),
        &[],
        &mut [&mut out[..frames]],
    );
}

fn compare(c: &mut Criterion, group: &str, cases: &[(String, Shape, usize)]) {
    let mut g = c.benchmark_group(group);
    for (label, shape, frames) in cases {
        let frames = *frames;
        g.throughput(Throughput::Elements(frames as u64));
        let mut net = net_for(shape);
        let input = BufferVec::new(0);
        let mut output = BufferVec::new(1);
        g.bench_with_input(BenchmarkId::new("net", label), &frames, |b, _| {
            b.iter(|| render_net(&mut net, frames, &input, black_box(&mut output)));
        });
        let mut exec = executor_for(shape);
        let mut out = vec![0.0f32; MAX_BLOCK];
        g.bench_with_input(BenchmarkId::new("graph", label), &frames, |b, _| {
            b.iter(|| render_graph(&mut exec, frames, black_box(&mut out)));
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
                Shape::Chain(depth),
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
        .map(|f| (f.to_string(), Shape::Chain(8), f))
        .collect();
    compare(c, "block_size", &cases);
}

/// One source into `width` parallel filters, summed.
fn bench_fan(c: &mut Criterion) {
    let cases: Vec<_> = [4usize, 16, 64]
        .into_iter()
        .map(|w| (format!("{w}-wide/256"), Shape::Fan(w), 256))
        .collect();
    compare(c, "fan", &cases);
}

criterion_group!(benches, bench_depth, bench_block_size, bench_fan);
criterion_main!(benches);
