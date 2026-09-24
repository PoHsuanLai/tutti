//! `Legacy`: an unmodified `AudioUnit` runs through the new graph and renders
//! exactly what fundsp's `Net` renders from it.

mod common;

use common::prepare;
use fundsp::net::Net;
use fundsp::prelude32::{limiter, lowpass_hz};
use tutti_graph::{Editor, Legacy, Node, Transport};
use tutti_node::buffer::BufferVec;
use tutti_node::{AudioUnit, MAX_BUFFER_SIZE};
use tutti_types::graph::{Edge, InPort, OutPort, Source};
use tutti_types::{ChannelLayout, Latency, NodeKey, SampleRate, Samples, Tail};

fn signal(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| ((i * 7919 % 97) as f32 - 48.0) / 48.0)
        .collect()
}

/// Global input → two Legacy filters in series → output, rendered in
/// 200-frame blocks (so the adapter sub-chunks 64/64/64/8 and the chunk
/// boundaries drift against `Net`'s), against the same two filters in a `Net`
/// rendered in 64-frame chunks. Bit-identical.
///
/// Mutation: in `Legacy::process`, copy the input with `start` fixed at 0
/// (every chunk re-reads the first 64 frames) → diverges from frame 64 →
/// fails. Mutation: skip `set_sample_rate` in `Legacy::prepare` → the filters
/// run at fundsp's default rate → diverges → fails.
#[test]
fn legacy_nodes_render_what_net_renders() {
    const RATE: f64 = 48_000.0;
    let total = 2_000;
    let input = signal(total);

    // The Net side.
    let mut net = Net::new(1, 1);
    let a = net.push(Box::new(lowpass_hz(700.0, 0.8)));
    let b = net.push(Box::new(lowpass_hz(2_300.0, 1.1)));
    net.pipe_input(a);
    net.connect(a, 0, b, 0);
    net.pipe_output(b);
    net.set_sample_rate(SampleRate(RATE));
    net.allocate();
    let mut want = Vec::with_capacity(total);
    let mut ibuf = BufferVec::new(1);
    let mut obuf = BufferVec::new(1);
    for chunk in input.chunks(MAX_BUFFER_SIZE) {
        ibuf.channel_f32_mut(0)[..chunk.len()].copy_from_slice(chunk);
        net.process(chunk.len(), &ibuf.buffer_ref(), &mut obuf.buffer_mut());
        want.extend_from_slice(&obuf.channel_f32_mut(0)[..chunk.len()]);
    }

    // The graph side.
    let (fa, fb) = (NodeKey(1), NodeKey(2));
    let (mut ed, mut exec) = Editor::new(prepare(256));
    ed.spec_mut().topology.inputs = ChannelLayout::MONO;
    ed.insert(fa, "lowpass", Legacy::new(lowpass_hz(700.0, 0.8)));
    ed.insert(fb, "lowpass", Legacy::new(lowpass_hz(2_300.0, 1.1)));
    let t = &mut ed.spec_mut().topology;
    t.edges.insert(
        InPort { node: fa, port: 0 },
        Edge::Direct(Source::Global(0)),
    );
    t.edges.insert(
        InPort { node: fb, port: 0 },
        Edge::Direct(Source::Node(OutPort { node: fa, port: 0 })),
    );
    t.outputs = vec![Source::Node(OutPort { node: fb, port: 0 })];
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    // Aliased in place: the adapter opts in, and this exercises its path.
    assert!(exec.plan().unwrap().in_place(fb).get(0));

    let mut got = Vec::with_capacity(total);
    let mut out = vec![0.0f32; 200];
    for chunk in input.chunks(200) {
        let n = chunk.len();
        exec.process(n, &Transport::default(), &[chunk], &mut [&mut out[..n]]);
        got.extend_from_slice(&out[..n]);
    }

    let bits = |v: &[f32]| v.iter().map(|x| x.to_bits()).collect::<Vec<_>>();
    assert_eq!(bits(&got), bits(&want));
    assert!(got.iter().any(|&x| x != 0.0));
}

/// A unit reporting a fractional latency through `route`, as fundsp derives
/// it — the only way to pin the rounding, since the library's own latent
/// nodes round internally and report whole frames.
#[derive(Clone)]
struct FractionalLatency;

impl AudioUnit for FractionalLatency {
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        output[0] = input[0];
    }
    fn process(
        &mut self,
        size: usize,
        input: &tutti_node::buffer::BufferRef,
        output: &mut tutti_node::buffer::BufferMut,
    ) {
        let src = input.channel_f32(0);
        output.channel_f32_mut(0)[..size].copy_from_slice(&src[..size]);
    }
    fn inputs(&self) -> usize {
        1
    }
    fn outputs(&self) -> usize {
        1
    }
    fn route(
        &mut self,
        _input: &tutti_node::signal::SignalFrame,
        _frequency: f64,
    ) -> tutti_node::signal::SignalFrame {
        let mut out = tutti_node::signal::SignalFrame::new(1);
        out.set(0, tutti_node::signal::Signal::Latency(2.5));
        out
    }
    fn get_id(&self) -> u64 {
        0x4652_4143
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn footprint(&self) -> usize {
        0
    }
}

/// A fractional latency rounds exactly as `Net`'s `LatencyGraph` impl rounds
/// it (`fundsp-tutti/src/latency/mod.rs:51`): 2.5 frames is 3.
///
/// Mutation: floor instead of round in `Legacy::probe` → 2 → fails.
#[test]
fn legacy_rounds_latency_as_net_does() {
    let mut node = Legacy::new(FractionalLatency);
    node.prepare(&prepare(64));
    assert_eq!(node.shape().latency, Latency::new(Samples(3)));
    let mut net = Net::new(1, 1);
    let id = net.push(Box::new(FractionalLatency));
    net.pipe_input(id);
    net.pipe_output(id);
    assert_eq!(
        tutti_types::latency::plan(&net).total(),
        node.shape().latency.samples()
    );
}

/// The adapter declares the unit's latency and its own tail, **at the
/// prepared rate**: a lookahead is a time, so the same limiter is 445 frames
/// late at 44.1 kHz and 485 at 48 kHz.
///
/// Mutation: drop the re-probe from `Legacy::prepare` → the shape keeps the
/// construction-time (44.1 kHz) figure → fails. Mutation: report
/// `Tail::Unknown` instead of `unit.tail()` → fails.
#[test]
fn legacy_declares_the_units_latency_and_tail() {
    let mut probe = limiter(0.0101, 0.01);
    probe.set_sample_rate(SampleRate(48_000.0));
    let reported = probe.latency().expect("a limiter reports latency");
    let mut node = Legacy::new(limiter(0.0101, 0.01));
    let before = node.shape().latency;
    node.prepare(&prepare(64));
    let shape = node.shape();
    assert_ne!(before, shape.latency, "the rate moved the latency");
    assert_eq!(
        shape.latency,
        Latency::new(Samples(reported.round() as usize))
    );
    assert!(!shape.latency.is_zero());
    assert_eq!(shape.tail, probe.tail(), "the unit's own tail, unchanged");
    assert_ne!(shape.tail, Tail::Unknown, "and this one does report one");
    assert!(shape.in_place);
}

/// Halves its input, reports `Tail::None`, and counts `process` calls.
#[derive(Clone)]
struct Half(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl AudioUnit for Half {
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        output[0] = input[0] * 0.5;
    }
    fn process(
        &mut self,
        size: usize,
        input: &tutti_node::buffer::BufferRef,
        output: &mut tutti_node::buffer::BufferMut,
    ) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let src = input.channel_f32(0);
        for (o, &i) in output.channel_f32_mut(0)[..size]
            .iter_mut()
            .zip(&src[..size])
        {
            *o = i * 0.5;
        }
    }
    fn inputs(&self) -> usize {
        1
    }
    fn outputs(&self) -> usize {
        1
    }
    fn route(
        &mut self,
        _input: &tutti_node::signal::SignalFrame,
        _frequency: f64,
    ) -> tutti_node::signal::SignalFrame {
        let mut out = tutti_node::signal::SignalFrame::new(1);
        out.set(0, tutti_node::signal::Signal::Latency(0.0));
        out
    }
    fn tail(&mut self) -> Tail {
        Tail::None
    }
    fn get_id(&self) -> u64 {
        0x4841_4c46
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn footprint(&self) -> usize {
        0
    }
}

/// `Legacy` reports the silence its unit produced, so an `AudioUnit` that
/// declares a tail is skipped on silent input again — the skip needs the last
/// output flagged silent, and a `Legacy` that always said `Modified` was
/// never skipped.
///
/// Mutation: return `Status::Modified` from `Legacy::process` instead of the
/// scanned mask → the unit runs every block → fails.
#[test]
fn legacy_reports_silence_so_a_silent_unit_is_skipped() {
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (mut ed, mut exec) = Editor::new(prepare(64));
    ed.spec_mut().topology.inputs = ChannelLayout::MONO;
    let key = NodeKey(1);
    ed.insert(
        key,
        "half",
        Legacy::new(Half(std::sync::Arc::clone(&calls))),
    );
    ed.spec_mut()
        .topology
        .edges
        .insert(InPort { node: key, port: 0 }, Edge::Direct(Source::Zero));
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort { node: key, port: 0 })];
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    let mut out = vec![0.0f32; 64];
    for _ in 0..10 {
        exec.process(
            64,
            &Transport::default(),
            &[&[0.0; 64]],
            &mut [&mut out[..]],
        );
    }
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "called once, found silent, then skipped"
    );
}
