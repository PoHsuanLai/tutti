//! Same-kind siblings run as one call: what batching `N` sibling nodes of
//! one kind into one kernel call would save, at the graph level. The
//! figures and the decision they led to are in doc 013, "Same-kind sibling
//! batching".
//!
//! Each case is one block of `frames` through an `Executor` (serial, no
//! pool), `members` siblings of one kind, every member reading the global
//! input and writing graph outputs of its own (no sum, so nothing but the
//! members and the copies differs between rows). Rows, per kind:
//!
//! - `none` — the same inputs wired straight to the outputs, no members:
//!   the copies alone. Subtract it to get the members' portion.
//! - `nodes` — `members` separate nodes, the graph today (light members are
//!   packed into a task, but each is still its own op and its own call).
//! - `batched` — one node holding every member and running them one after
//!   another, each with the per-member kernel `nodes` runs: the smallest
//!   batching design, which saves only the per-op cost.
//! - `soa` — one node holding every member, running eight members at once
//!   in `f32x8` lanes (gathered from and scattered to their planar
//!   buffers): the ceiling of a cross-member SIMD kernel.
//!
//! Kinds:
//!
//! - `strip` — a stereo gain ramped across the block (the bus strip's
//!   arithmetic, channel-major so it vectorises along time).
//! - `bus-strip` — the production `BusStripNode` itself (`nodes` only):
//!   what N strips cost today.
//! - `biquad` — a mono transposed-direct-form-II low-pass, the recursive
//!   kernel time cannot vectorise and members can.
//!
//! ```text
//! cargo bench -p tutti-nodes --bench sibling_batch
//! ```
//!
//! Each `batched` and `soa` graph is checked against `nodes` once, bit for
//! bit, before it is timed: the lanes run each member's own arithmetic in
//! its own order (no fused multiply-add), so the three render the same.

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use tutti_core::graph::{Edge, InPort, OutPort, Source};
use tutti_core::{ChannelLayout, NodeKey, SampleRate, Samples};
use tutti_graph::{Cost, Cx, Editor, Executor, Io, Node, Prepare, Shape, Status, Transport};
use tutti_graph::{ForkByClone, Unforkable};
use tutti_nodes::BusStripNode;
use wide::f32x8;

const SR: f64 = 48_000.0;
/// A node has at most 64 audio ports a side (`MAX_PORTS`), so one batched
/// node holds at most 32 stereo strips or 64 mono biquads.
const MEMBERS_STEREO: [usize; 2] = [16, 32];
const MEMBERS_MONO: [usize; 2] = [16, 64];
const FRAMES: [usize; 3] = [64, 128, 512];

// ---- strip: a stereo gain ramp ------------------------------------------

#[derive(Clone, Copy)]
struct Ramp {
    from: f32,
    to: f32,
}

impl Ramp {
    fn member(m: usize) -> Self {
        Self {
            from: 0.5 + m as f32 * 1e-3,
            to: 0.75 - m as f32 * 1e-3,
        }
    }

    /// The gain at frame `i` of `n`: the strip's `from·(1−t) + to·t`.
    #[inline(always)]
    fn at(self, i: usize, n: usize) -> f32 {
        let t = (i + 1) as f32 / n as f32;
        self.from * (1.0 - t) + self.to * t
    }

    #[inline(always)]
    fn run(self, x: &[f32], y: &mut [f32]) {
        let n = y.len();
        for (i, (y, x)) in y.iter_mut().zip(x).enumerate() {
            *y = x * self.at(i, n);
        }
    }
}

/// One strip member as its own node: stereo in, stereo out.
#[derive(Clone)]
struct StripNode(Ramp);

impl Node for StripNode {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::STEREO, ChannelLayout::STEREO).with_cost(Cost::Light)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let (ins, mut outs) = io.split();
        for c in 0..2 {
            self.0.run(ins.get(c), outs.get(c));
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// Every strip member in one node, one after another.
#[derive(Clone)]
struct StripBatch(Vec<Ramp>);

impl Node for StripBatch {
    fn shape(&self) -> Shape {
        let w = ChannelLayout::from_count(2 * self.0.len() as u16);
        Shape::audio(w, w).with_cost(Cost::Light)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let (ins, mut outs) = io.split();
        for (m, r) in self.0.iter().enumerate() {
            for c in 0..2 {
                r.run(ins.get(2 * m + c), outs.get(2 * m + c));
            }
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// Every strip member in one node, eight channels' gains in one `f32x8`.
#[derive(Clone)]
struct StripSoa(Vec<Ramp>);

impl Node for StripSoa {
    fn shape(&self) -> Shape {
        let w = ChannelLayout::from_count(2 * self.0.len() as u16);
        Shape::audio(w, w).with_cost(Cost::Light)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let n = io.frames();
        let (ins, mut outs) = io.split();
        let mut outs = outs.iter_mut();
        // Eight channels a group: four members, two channels each.
        for (g, members) in self.0.as_chunks::<4>().0.iter().enumerate() {
            let from = f32x8::new(std::array::from_fn(|l| members[l / 2].from));
            let to = f32x8::new(std::array::from_fn(|l| members[l / 2].to));
            let x: [&[f32]; 8] = std::array::from_fn(|l| ins.get(8 * g + l));
            let y: [&mut [f32]; 8] = std::array::from_fn(|_| outs.next().unwrap());
            for i in 0..n {
                let t = f32x8::splat((i + 1) as f32 / n as f32);
                let gain = from * (f32x8::ONE - t) + to * t;
                let v = f32x8::new(std::array::from_fn(|l| x[l][i])) * gain;
                for (l, v) in v.to_array().into_iter().enumerate() {
                    y[l][i] = v;
                }
            }
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

// ---- biquad: a recursive low-pass ----------------------------------------

#[derive(Clone, Copy)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    s1: f32,
    s2: f32,
}

impl Biquad {
    /// An RBJ low-pass at a cutoff of its own, Q 0.707.
    fn member(m: usize) -> Self {
        let w = std::f64::consts::TAU * (300.0 + 37.0 * m as f64) / SR;
        let alpha = w.sin() / (2.0 * std::f64::consts::FRAC_1_SQRT_2);
        let a0 = 1.0 + alpha;
        let b1 = (1.0 - w.cos()) / a0;
        Self {
            b0: (b1 / 2.0) as f32,
            b1: b1 as f32,
            b2: (b1 / 2.0) as f32,
            a1: (-2.0 * w.cos() / a0) as f32,
            a2: ((1.0 - alpha) / a0) as f32,
            s1: 0.0,
            s2: 0.0,
        }
    }

    #[inline(always)]
    fn run(&mut self, x: &[f32], y: &mut [f32]) {
        let (mut s1, mut s2) = (self.s1, self.s2);
        for (y, &x) in y.iter_mut().zip(x) {
            let out = self.b0 * x + s1;
            s1 = self.b1 * x - self.a1 * out + s2;
            s2 = self.b2 * x - self.a2 * out;
            *y = out;
        }
        (self.s1, self.s2) = (s1, s2);
    }
}

#[derive(Clone)]
struct BiquadNode(Biquad);

impl Node for BiquadNode {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::MONO, ChannelLayout::MONO).with_cost(Cost::Moderate)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let (ins, mut outs) = io.split();
        self.0.run(ins.get(0), outs.get(0));
        Status::Modified
    }
    fn reset(&mut self) {}
}

#[derive(Clone)]
struct BiquadBatch(Vec<Biquad>);

impl Node for BiquadBatch {
    fn shape(&self) -> Shape {
        let w = ChannelLayout::from_count(self.0.len() as u16);
        Shape::audio(w, w).with_cost(Cost::Moderate)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let (ins, mut outs) = io.split();
        for (m, f) in self.0.iter_mut().enumerate() {
            f.run(ins.get(m), outs.get(m));
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// Eight biquads a group, structure of arrays.
#[derive(Clone)]
struct BiquadGroup {
    b0: f32x8,
    b1: f32x8,
    b2: f32x8,
    a1: f32x8,
    a2: f32x8,
    s1: f32x8,
    s2: f32x8,
}

#[derive(Clone)]
struct BiquadSoa(Vec<BiquadGroup>);

impl BiquadSoa {
    fn new(members: &[Biquad]) -> Self {
        Self(
            members
                .as_chunks::<8>()
                .0
                .iter()
                .map(|m| {
                    let f = |g: fn(&Biquad) -> f32| f32x8::new(std::array::from_fn(|l| g(&m[l])));
                    BiquadGroup {
                        b0: f(|b| b.b0),
                        b1: f(|b| b.b1),
                        b2: f(|b| b.b2),
                        a1: f(|b| b.a1),
                        a2: f(|b| b.a2),
                        s1: f(|b| b.s1),
                        s2: f(|b| b.s2),
                    }
                })
                .collect(),
        )
    }
}

impl Node for BiquadSoa {
    fn shape(&self) -> Shape {
        let w = ChannelLayout::from_count(8 * self.0.len() as u16);
        Shape::audio(w, w).with_cost(Cost::Moderate)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let n = io.frames();
        let (ins, mut outs) = io.split();
        let mut outs = outs.iter_mut();
        for (g, q) in self.0.iter_mut().enumerate() {
            let x: [&[f32]; 8] = std::array::from_fn(|l| ins.get(8 * g + l));
            let y: [&mut [f32]; 8] = std::array::from_fn(|_| outs.next().unwrap());
            let (mut s1, mut s2) = (q.s1, q.s2);
            for i in 0..n {
                let x = f32x8::new(std::array::from_fn(|l| x[l][i]));
                let out = q.b0 * x + s1;
                s1 = q.b1 * x - q.a1 * out + s2;
                s2 = q.b2 * x - q.a2 * out;
                for (l, v) in out.to_array().into_iter().enumerate() {
                    y[l][i] = v;
                }
            }
            (q.s1, q.s2) = (s1, s2);
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

// ---- graphs ----------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum Kind {
    Strip,
    BusStrip,
    Biquad,
}

impl Kind {
    fn width(self) -> usize {
        match self {
            Kind::Strip | Kind::BusStrip => 2,
            Kind::Biquad => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Variant {
    None,
    Nodes,
    Batched,
    Soa,
}

/// Member `m`'s channel `c` reads global input `c`, and output
/// `m * width + c` is its own.
fn graph(kind: Kind, variant: Variant, members: usize, frames: usize) -> Executor {
    let (mut ed, mut exec) = Editor::new(Prepare::new(SampleRate(SR), Samples(frames)));
    let w = kind.width();
    ed.spec_mut().topology.inputs = ChannelLayout::from_count(w as u16);
    let mut outputs = Vec::with_capacity(members * w);
    let feed = |ed: &mut Editor, node: NodeKey, port: u16, c: usize| {
        ed.spec_mut().topology.edges.insert(
            InPort { node, port },
            Edge::Direct(Source::Global(c as u16)),
        );
    };
    match variant {
        Variant::None => {
            for _ in 0..members {
                for c in 0..w {
                    outputs.push(Source::Global(c as u16));
                }
            }
        }
        Variant::Nodes => {
            for m in 0..members {
                let k = NodeKey(m as u64 + 1);
                match kind {
                    Kind::Strip => {
                        ed.insert(k, "strip", ForkByClone(StripNode(Ramp::member(m))));
                    }
                    Kind::BusStrip => {
                        let s = BusStripNode::new();
                        s.set_volume(0.5 + m as f32 * 1e-3);
                        ed.insert(k, "bus strip", s);
                    }
                    Kind::Biquad => {
                        ed.insert(k, "biquad", ForkByClone(BiquadNode(Biquad::member(m))));
                    }
                }
                for c in 0..w {
                    feed(&mut ed, k, c as u16, c);
                    outputs.push(Source::Node(OutPort {
                        node: k,
                        port: c as u16,
                    }));
                }
            }
        }
        Variant::Batched | Variant::Soa => {
            let k = NodeKey(1);
            let soa = variant == Variant::Soa;
            match kind {
                Kind::Strip => {
                    let r: Vec<_> = (0..members).map(Ramp::member).collect();
                    match soa {
                        true => ed.insert(k, "strips", Unforkable(StripSoa(r))),
                        false => ed.insert(k, "strips", Unforkable(StripBatch(r))),
                    }
                }
                Kind::Biquad => {
                    let b: Vec<_> = (0..members).map(Biquad::member).collect();
                    match soa {
                        true => ed.insert(k, "biquads", Unforkable(BiquadSoa::new(&b))),
                        false => ed.insert(k, "biquads", Unforkable(BiquadBatch(b))),
                    }
                }
                Kind::BusStrip => unreachable!("the production strip is measured as nodes only"),
            };
            for m in 0..members {
                for c in 0..w {
                    let port = (m * w + c) as u16;
                    feed(&mut ed, k, port, c);
                    outputs.push(Source::Node(OutPort { node: k, port }));
                }
            }
        }
    }
    ed.spec_mut().topology.outputs = outputs;
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    exec
}

struct Bufs {
    input: Vec<Vec<f32>>,
    out: Vec<Vec<f32>>,
}

impl Bufs {
    fn new(kind: Kind, members: usize, frames: usize) -> Self {
        let input = (0..kind.width())
            .map(|c| {
                (0..frames)
                    .map(|i| ((i * 7 + c * 3) % 23) as f32 / 23.0 - 0.5)
                    .collect()
            })
            .collect();
        Self {
            input,
            out: vec![vec![0.0; frames]; members * kind.width()],
        }
    }

    fn block(&mut self, exec: &mut Executor, frames: usize) {
        let ins: Vec<&[f32]> = self.input.iter().map(|v| &v[..]).collect();
        let mut outs: Vec<&mut [f32]> = self.out.iter_mut().map(|v| &mut v[..]).collect();
        exec.process(frames, &Transport::default(), &ins, &mut outs);
    }
}

/// `batched` and `soa` render `nodes` bit for bit over a few blocks (the
/// recursive state included).
///
/// Mutations (run, `-- --test`): the SoA biquad's state update reassociated
/// (`b1·x + s2 − a1·y`) → "Biquad/Soa differs"; the SoA strip's ramp as
/// `from + (to − from)·t` → "Strip/Soa differs".
fn check(kind: Kind, variant: Variant, members: usize, frames: usize) {
    let mut want = graph(kind, Variant::Nodes, members, frames);
    let mut got = graph(kind, variant, members, frames);
    let (mut a, mut b) = (
        Bufs::new(kind, members, frames),
        Bufs::new(kind, members, frames),
    );
    for _ in 0..4 {
        a.block(&mut want, frames);
        b.block(&mut got, frames);
        for (x, y) in a.out.iter().zip(&b.out) {
            assert!(
                x.iter().zip(y).all(|(x, y)| x.to_bits() == y.to_bits()),
                "{kind:?}/{variant:?} differs from separate nodes"
            );
        }
    }
}

fn bench_kind(c: &mut Criterion, kind: Kind, variants: &[Variant]) {
    let mut g = c.benchmark_group(format!("{kind:?}").to_lowercase());
    let members_for = match kind.width() {
        1 => MEMBERS_MONO,
        _ => MEMBERS_STEREO,
    };
    for members in members_for {
        for frames in FRAMES {
            g.throughput(Throughput::Elements((members * frames) as u64));
            for &v in variants {
                if matches!(v, Variant::Batched | Variant::Soa) {
                    check(kind, v, members, frames);
                }
                let mut exec = graph(kind, v, members, frames);
                let mut bufs = Bufs::new(kind, members, frames);
                let id = BenchmarkId::new(
                    format!("{v:?}").to_lowercase(),
                    format!("{members}x{frames}"),
                );
                g.bench_function(id, |b| {
                    b.iter(|| {
                        bufs.block(&mut exec, frames);
                        black_box(&bufs.out);
                    })
                });
            }
        }
    }
    g.finish();
}

fn benches(c: &mut Criterion) {
    use Variant::*;
    bench_kind(c, Kind::Strip, &[None, Nodes, Batched, Soa]);
    bench_kind(c, Kind::BusStrip, &[None, Nodes]);
    bench_kind(c, Kind::Biquad, &[None, Nodes, Batched, Soa]);
}

criterion_group!(group, benches);
criterion_main!(group);
