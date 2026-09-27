//! The parallel executor renders **bit for bit** what the serial one does
//! (doc 013 Phase 6): on a graph with fan-out, fan-in, PDC rings, event
//! fan-in, compiler-owned param modulation (an audio source behind a latent
//! sibling, and ramp events), a feedback loop and scheduled commands, for
//! every worker count in `common::WORKERS` and block schedules from one frame
//! to the prepared maximum, ragged ones included — and the same on an
//! offline fork of it (the graph export renders).
//!
//! The differential suite (`tests/differential.rs`) holds the parallel
//! executor to the reference interpreter on random graphs as well; this file
//! is the direct comparison, on one graph that has everything at once.

mod common;

use common::{bits, test_pool, Kind, TestNode, WORKERS};
use tutti_graph::{
    Cx, Editor, EventEdge, EventIn, EventKind, EventOut, Executor, ForkCause, ForkMode, ForkSource,
    ForkTarget, Forked, IntoNode, Io, Node, NodeParts, ParamFrom, ParamIn, ParamRamp, ParamShaping,
    Prepare, Shape, Status, Transport, Ump,
};
use tutti_types::graph::{Edge, FeedbackFrom, InPort, OutPort, Source};
use tutti_types::{At, Beat, Bpm, ChannelLayout, Frame, NodeKey, SampleRate, Samples};

const MAX_BLOCK: usize = 128;

/// A `Kind` whose fork is a fresh node of the same kind.
struct Forkable(Kind);

struct KindFork(Kind);

impl ForkSource for KindFork {
    fn fork(&self, _mode: ForkMode<'_>) -> Result<Forked, ForkCause> {
        Ok(Forked::new(Box::new(TestNode::new(self.0.clone()))))
    }
}

impl IntoNode for Forkable {
    type Controls = ();
    fn into_node(self) -> (Box<dyn Node>, ()) {
        (Box::new(TestNode::new(self.0)), ())
    }
    fn into_parts(self) -> NodeParts<()> {
        NodeParts {
            node: Box::new(TestNode::new(self.0.clone())),
            controls: (),
            fork: Some(Box::new(KindFork(self.0))),
        }
    }
}

/// `Cutoff + Q` per frame from its two declared params, times its input.
#[derive(Clone)]
struct ParamSink;

impl Node for ParamSink {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::MONO, ChannelLayout::MONO)
            .with_params(&[tutti_types::UnitParam::Cutoff, tutti_types::UnitParam::Q])
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        for i in 0..io.frames() {
            let a = io.param(0).frames().map_or(1.0, |v| v[i]);
            let b = io.param(1).frames().map_or(2.0, |v| v[i]);
            let x = io.input(0)[i];
            io.output(0)[i] = (a + b) * x;
        }
        Status::Modified
    }
    fn reset(&mut self) {}
    fn param_base(&self, port: usize) -> Option<f32> {
        [1.0, 2.0].get(port).copied()
    }
}

/// A ramp on `Cutoff` every 37 frames of absolute time.
#[derive(Clone)]
struct RampEvery;

impl Node for RampEvery {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY)
            .with_events(0, 1)
            .with_event_capacity(16)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let start = cx.env.frame.get();
        for i in 0..io.frames() {
            if (start + i as u64).is_multiple_of(37) {
                let r = ParamRamp::new(
                    tutti_types::ParamKey::<tutti_types::Hz>::CUTOFF,
                    tutti_types::Hz(((start + i as u64) % 5) as f32),
                    Samples(20),
                );
                let o = io.offset(i).expect("inside");
                let _ = io.event_out(0).push(tutti_graph::Event::ramp(o, r));
            }
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

fn at(node: u64, port: u16) -> InPort {
    InPort {
        node: NodeKey(node),
        port,
    }
}

fn out(node: u64, port: u16) -> OutPort {
    OutPort {
        node: NodeKey(node),
        port,
    }
}

fn from(node: u64, port: u16) -> Edge {
    Edge::Direct(Source::Node(out(node, port)))
}

fn ev_out(node: u64) -> EventOut {
    EventOut {
        node: NodeKey(node),
        port: 0,
    }
}

fn consumer_in(port: u16) -> EventIn {
    EventIn {
        node: NodeKey(40),
        port,
    }
}

/// The graph (keys in brackets):
///
/// ```text
/// in ─┬─ Lag 5 [2] ──┐
///     ├─ Lag 17 [3] ─┼─ Sum [8] ─┐
///     ├─ Gain [4] ───┘            ├─ Sum [9] ─── out 0
///     ├─ Smooth [5] ──────────────┤
///     └─ Lag 3 [6] ─ Gain [7] ────┘
/// Emitter [20] ─┬─ EventLag 9 [22] ─┐
/// Emitter [21] ─┴───────────────────┴─ Consumer [40] (fan-in on port 0) ─ out 1
///                                    Mixed [41] ◀── events [21] ─ out 2
/// in ─ ParamSink [30] (Cutoff ← Gain [4] audio, ← RampEvery [31]; Q ← Lag 17) ─ out 3
/// Sum [50] ← in, ← feedback of Smooth [51] (256 frames) ; Smooth [51] ← [50] ─ out 4
/// ```
fn build(ed: &mut Editor) {
    ed.spec_mut().topology.inputs = ChannelLayout::MONO;
    let gain = |g| Forkable(Kind::Gain { gain: g, width: 1 });
    ed.insert(NodeKey(2), "lag5", Forkable(Kind::Lag { latency: 5 }));
    ed.insert(NodeKey(3), "lag17", Forkable(Kind::Lag { latency: 17 }));
    ed.insert(NodeKey(4), "gain", gain(0.5));
    ed.insert(NodeKey(5), "smooth", Forkable(Kind::Smooth));
    ed.insert(NodeKey(6), "lag3", Forkable(Kind::Lag { latency: 3 }));
    ed.insert(NodeKey(7), "gain", gain(0.75));
    ed.insert(NodeKey(8), "sum3", Forkable(Kind::Sum { inputs: 3 }));
    ed.insert(NodeKey(9), "sum3", Forkable(Kind::Sum { inputs: 3 }));
    ed.insert(
        NodeKey(20),
        "emit",
        Forkable(Kind::Emitter {
            period: 5,
            phase: 0,
        }),
    );
    ed.insert(
        NodeKey(21),
        "emit",
        Forkable(Kind::Emitter {
            period: 7,
            phase: 3,
        }),
    );
    ed.insert(NodeKey(22), "elag", Forkable(Kind::EventLag { latency: 9 }));
    ed.insert(
        NodeKey(40),
        "consumer",
        Forkable(Kind::Consumer { inputs: 2 }),
    );
    ed.insert(
        NodeKey(41),
        "mixed",
        Forkable(Kind::Mixed {
            width: 2,
            events_in: 1,
            events_out: 1,
        }),
    );
    ed.insert(NodeKey(30), "psink", tutti_graph::ForkByClone(ParamSink));
    ed.insert(NodeKey(31), "ramps", tutti_graph::ForkByClone(RampEvery));
    ed.insert(NodeKey(50), "fbsum", Forkable(Kind::Sum { inputs: 2 }));
    ed.insert(NodeKey(51), "fbsmooth", Forkable(Kind::Smooth));
    let spec = ed.spec_mut();
    let t = &mut spec.topology;
    let g = Edge::Direct(Source::Global(0));
    for k in [2, 3, 4, 5, 6, 30, 50] {
        t.edges.insert(at(k, 0), g);
    }
    t.edges.insert(at(7, 0), from(6, 0));
    t.edges.insert(at(8, 0), from(2, 0));
    t.edges.insert(at(8, 1), from(3, 0));
    t.edges.insert(at(8, 2), from(4, 0));
    t.edges.insert(at(9, 0), from(8, 0));
    t.edges.insert(at(9, 1), from(5, 0));
    t.edges.insert(at(9, 2), from(7, 0));
    t.edges.insert(at(41, 0), from(9, 0));
    t.edges.insert(at(41, 1), from(5, 0));
    t.edges.insert(
        at(50, 1),
        Edge::Feedback(FeedbackFrom::new(out(51, 0), Samples(256))),
    );
    t.edges.insert(at(51, 0), from(50, 0));
    t.outputs = vec![
        Source::Node(out(9, 0)),
        Source::Node(out(40, 0)),
        Source::Node(out(41, 1)),
        Source::Node(out(30, 0)),
        Source::Node(out(51, 0)),
    ];
    for (to, src) in [
        (consumer_in(0), 20),
        (consumer_in(0), 22),
        (consumer_in(1), 21),
    ] {
        spec.connect_events(to, EventEdge::Direct(ev_out(src)));
    }
    spec.connect_events(
        EventIn {
            node: NodeKey(22),
            port: 0,
        },
        EventEdge::Direct(ev_out(20)),
    );
    spec.connect_events(
        EventIn {
            node: NodeKey(41),
            port: 0,
        },
        EventEdge::Direct(ev_out(21)),
    );
    let cut = ParamIn {
        node: NodeKey(30),
        param: tutti_types::UnitParam::Cutoff,
    };
    let q = ParamIn {
        node: NodeKey(30),
        param: tutti_types::UnitParam::Q,
    };
    spec.connect_param(cut, ParamFrom::Audio(out(4, 0)), ParamShaping::Identity);
    spec.connect_param(cut, ParamFrom::Events(ev_out(31)), ParamShaping::Identity);
    spec.connect_param(q, ParamFrom::Audio(out(3, 0)), ParamShaping::Identity);
    ed.commit().expect("the graph commits");
}

fn prepare(max: usize) -> Prepare {
    Prepare::new(SampleRate(48_000.0), Samples(max))
}

/// The block schedules: one frame, odd lengths, the maximum, and a ragged
/// one that changes every block.
fn schedules(max: usize) -> Vec<Vec<usize>> {
    let total = 3_000;
    let fixed = |n: usize| {
        let mut v = vec![n; total / n];
        v.push(total % n.max(1));
        v.retain(|&n| n > 0);
        v
    };
    let mut ragged = Vec::new();
    let (mut sum, mut i) = (0, 0usize);
    while sum < total {
        let n = [1, max, 7, 64, max - 1, 33, 2][i % 7].min(total - sum);
        ragged.push(n);
        sum += n;
        i += 1;
    }
    vec![fixed(1), fixed(7), fixed(64), fixed(max), ragged]
}

/// Render `blocks` through `exec` (drained back into `ed`), with commands
/// scheduled into the consumer along the way and the transport rolling;
/// every output channel, concatenated.
fn render(ed: &mut Editor, exec: &mut Executor, blocks: &[usize]) -> Vec<Vec<f32>> {
    let outputs = 5;
    let mut all = vec![Vec::new(); outputs];
    let mut frame = 0u64;
    for (i, &n) in blocks.iter().enumerate() {
        if i % 11 == 3 {
            let kind = EventKind::Midi(Ump([i as u32, 0, 0, 0]));
            ed.schedule(At::Frame(Frame(frame + 5)), consumer_in(1), kind)
                .expect("room");
            ed.schedule(
                At::Beat(Beat(frame as f64 / 24_000.0 + 0.01)),
                consumer_in(0),
                kind,
            )
            .expect("room");
        }
        let input: Vec<f32> = (0..n)
            .map(|k| (((frame + k as u64) * 7919) % 211) as f32 / 211.0 - 0.5)
            .collect();
        let mut chans = vec![vec![0.0f32; n]; outputs];
        {
            let mut outs: Vec<&mut [f32]> = chans.iter_mut().map(Vec::as_mut_slice).collect();
            let t = Transport::new(true, Bpm(120.0), Beat(frame as f64 / 24_000.0), None);
            exec.process(n, &t, &[&input], &mut outs);
        }
        for (a, c) in all.iter_mut().zip(chans) {
            a.extend(c);
        }
        ed.collect();
        frame += n as u64;
    }
    assert_eq!(exec.dropped_events(), 0);
    all
}

fn pair(workers: usize) -> (Editor, Executor) {
    let (mut ed, mut exec) = Editor::new(prepare(MAX_BLOCK));
    if workers > 1 {
        exec.set_pool(Some(test_pool(workers)));
    }
    build(&mut ed);
    exec.apply_pending();
    ed.collect();
    assert_eq!(exec.is_parallel(), workers > 1, "{workers} workers");
    (ed, exec)
}

/// Every worker count and block schedule renders what the serial executor
/// renders, sample for sample, on every output.
///
/// Mutation (each applied to `src/par.rs`, seen to fail, reverted): skip
/// `Op::Capture` in `Shared::run_op` (the feedback loop's ring never fills)
/// → diverges; run a task's ops in reverse → diverges.
#[test]
fn the_parallel_render_is_bit_identical_to_the_serial_one() {
    for blocks in schedules(MAX_BLOCK) {
        let (mut ed, mut exec) = pair(1);
        let serial = render(&mut ed, &mut exec, &blocks);
        assert!(
            serial.iter().all(|c| c.iter().any(|&x| x != 0.0)),
            "every output carries signal"
        );
        for workers in WORKERS {
            let (mut ed, mut exec) = pair(workers);
            let parallel = render(&mut ed, &mut exec, &blocks);
            for (c, (s, p)) in serial.iter().zip(&parallel).enumerate() {
                assert_eq!(
                    bits(std::slice::from_ref(s)),
                    bits(std::slice::from_ref(p)),
                    "output {c} diverges with {workers} workers, blocks {:?}…",
                    &blocks[..blocks.len().min(4)]
                );
            }
        }
    }
}

/// The plan really is parallel: several tasks, a level wide enough to
/// spread.
///
/// Mutation: make `compile`'s `level_width` return 1 → fails.
#[test]
fn the_graph_spreads() {
    let (_ed, exec) = pair(4);
    let plan = exec.plan().expect("a plan");
    assert!(plan.tasks().len() > 8, "{} tasks", plan.tasks().len());
    assert!(plan.task_width() >= 5, "width {}", plan.task_width());
}

/// An offline fork (what export renders) renders the same on a pool as
/// alone, at the export's block size and at ragged ones — and the same as
/// the live graph's own fresh render.
///
/// Mutation: in `Executor::set_pool`, build no parallel state (`par =
/// None`) → the fork is not parallel → the `is_parallel` assert fails.
#[test]
fn an_offline_fork_renders_the_same_on_a_pool() {
    let (live_ed, _live) = pair(1);
    let pre = prepare(256);
    let fork = |workers: usize| {
        let ctx = tutti_types::OfflineTransport::new(std::sync::Arc::new(Frozen));
        let (ed, mut exec) = live_ed
            .fork(ForkTarget::Master, ForkMode::Offline(&ctx), pre)
            .expect("every node forks");
        if workers > 1 {
            exec.set_pool(Some(test_pool(workers)));
        }
        (ed, exec)
    };
    for blocks in [vec![256; 12], schedules(256).pop().expect("ragged")] {
        let (mut ed, mut exec) = fork(1);
        exec.apply_pending();
        let serial = render(&mut ed, &mut exec, &blocks);
        for workers in WORKERS {
            let (mut ed, mut exec) = fork(workers);
            exec.apply_pending();
            assert!(exec.is_parallel());
            let parallel = render(&mut ed, &mut exec, &blocks);
            assert_eq!(bits(&serial), bits(&parallel), "{workers} workers");
        }
    }
}

/// The offline fork's timeline: frozen at beat 0 (nothing here reads it).
struct Frozen;

impl tutti_types::Timeline for Frozen {
    fn beat(&self) -> Beat {
        Beat(0.0)
    }
    fn tempo(&self) -> Bpm {
        Bpm(120.0)
    }
    fn is_rolling(&self) -> bool {
        true
    }
    fn segment_generation(&self) -> u64 {
        0
    }
}

impl tutti_types::OfflineClock for Frozen {}

/// A node that panics on a worker aborts the block, and the caller panics
/// with a message naming the cause once every participant has left.
///
/// Mutation: in `Shared::participate`, do not abort the task graph when a
/// task panics → the other participants wait forever for the dead task's
/// successors → the test hangs.
#[test]
#[should_panic(expected = "a node panicked on a parallel worker")]
fn a_panic_on_a_worker_reaches_the_caller() {
    struct Boom;
    impl Node for Boom {
        fn shape(&self) -> Shape {
            Shape::audio(ChannelLayout::MONO, ChannelLayout::MONO)
        }
        fn prepare(&mut self, _: &Prepare) {}
        fn process(&mut self, cx: &Cx<'_>, _: Io<'_>) -> Status {
            if cx.env.frame.get() >= 256 {
                panic!("boom");
            }
            Status::Silent
        }
        fn reset(&mut self) {}
    }
    let (mut ed, mut exec) = Editor::new(prepare(MAX_BLOCK));
    exec.set_pool(Some(test_pool(4)));
    ed.spec_mut().topology.inputs = ChannelLayout::MONO;
    // One node panics; the other five are fine, so every participant but
    // the one that caught the panic would keep waiting for its task.
    ed.insert(NodeKey(1), "boom", tutti_graph::Unforkable(Boom));
    for k in 2..=6 {
        ed.insert(
            NodeKey(k),
            "gain",
            tutti_graph::Unforkable(TestNode::new(Kind::Gain {
                gain: 0.5,
                width: 1,
            })),
        );
    }
    for k in 1..=6 {
        ed.spec_mut()
            .topology
            .edges
            .insert(at(k, 0), Edge::Direct(Source::Global(0)));
    }
    ed.spec_mut().topology.outputs = (1..=6).map(|k| Source::Node(out(k, 0))).collect();
    ed.commit().expect("commits");
    let input = vec![0.5f32; MAX_BLOCK];
    let mut chans = vec![vec![0.0f32; MAX_BLOCK]; 6];
    for _ in 0..4 {
        let mut outs: Vec<&mut [f32]> = chans.iter_mut().map(Vec::as_mut_slice).collect();
        exec.process(MAX_BLOCK, &Transport::default(), &[&input], &mut outs);
    }
}
