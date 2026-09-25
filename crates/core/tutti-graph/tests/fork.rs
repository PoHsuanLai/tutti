//! `Editor::fork`: a copy of the graph (or of what feeds one node) that
//! shares no state with the live one — the replacement for fundsp's
//! `clone_isolated` → `isolate_for_offline` → `reset` (doc 013 Phase 3 PR 2).

mod common;

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use common::{bits, prepare, Kind, TestNode};
use fundsp::net::Net;
use fundsp::prelude32::{lowpass_hz, mul, pass, sine_hz};
use tutti_graph::{
    Editor, EventEdge, EventIn, EventOut, ForkError, ForkMode, ForkSource, ForkTarget,
    GraphBuilder, IntoNode, Legacy, Node, NodeParts, Renderer,
};
use tutti_node::buffer::{BufferMut, BufferRef, BufferVec};
use tutti_node::signal::{Signal, SignalFrame};
use tutti_node::{Address, AudioUnit, Parameter, Setting, MAX_BUFFER_SIZE};
use tutti_types::graph::{Edge, FeedbackFrom, InPort, OutPort, Source};
use tutti_types::{ChannelLayout, NodeKey, SampleRate, Samples};

/// The `AudioUnit` methods every probe below has alike: `outs` outputs, no
/// inputs, no latency.
macro_rules! probe_boilerplate {
    () => {
        fn inputs(&self) -> usize {
            0
        }
        fn outputs(&self) -> usize {
            self.outs
        }
        fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
            let mut out = SignalFrame::new(self.outs);
            for c in 0..self.outs {
                out.set(c, Signal::Latency(0.0));
            }
            out
        }
        fn get_id(&self) -> u64 {
            0x464f_524b
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
    };
}

/// Output `c` is `base + c`. `Value(v)` at `Index(0)` sets `base` — a plain
/// field, reachable only through `AudioUnit::set`, as the sampler voice's
/// `play.gain` is.
#[derive(Clone)]
struct Consts {
    outs: usize,
    base: f32,
}

impl AudioUnit for Consts {
    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        for (c, o) in output.iter_mut().enumerate() {
            *o = self.base + c as f32;
        }
    }
    fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
        for c in 0..self.outs {
            output.channel_f32_mut(c)[..size].fill(self.base + c as f32);
        }
    }
    fn set(&mut self, setting: Setting) {
        if let (Parameter::Value(v), Address::Index(0)) = (setting.parameter(), setting.direction())
        {
            self.base = *v;
        }
    }
    probe_boilerplate!();
}

/// Makes the fork's three steps observable, and their order. Outputs
/// `level`. `isolate` drops the binding, `rebind_offline` binds to the
/// context (an `f32`), and `reset` re-reads `level` from the binding — so
/// only isolate → rebind → reset leaves `level` at the context's value.
#[derive(Clone)]
struct Bindable {
    outs: usize,
    level: f32,
    bound: Option<f32>,
}

impl AudioUnit for Bindable {
    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        output[0] = self.level;
    }
    fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
        output.channel_f32_mut(0)[..size].fill(self.level);
    }
    fn isolate(&mut self) {
        self.bound = None;
    }
    fn rebind_offline(&mut self, ctx: &dyn std::any::Any) {
        if let Some(v) = ctx.downcast_ref::<f32>() {
            self.bound = Some(*v);
        }
    }
    fn reset(&mut self) {
        self.level = self.bound.unwrap_or(0.0);
    }
    probe_boilerplate!();
}

/// A ramp whose position lives in an `Arc` cell a clone **shares** — the
/// shape of a voice pool's command channel or a stretcher's bank. `isolate`
/// gives the unit a cell of its own at the current position; `reset` rewinds
/// the cell. Without `isolate`, a clone's rendering and resetting move the
/// original.
#[derive(Clone)]
struct Shared {
    outs: usize,
    pos: Arc<AtomicU32>,
}

impl Shared {
    fn new() -> Self {
        Self {
            outs: 1,
            pos: Arc::new(AtomicU32::new(0)),
        }
    }
}

impl AudioUnit for Shared {
    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        output[0] = self.pos.fetch_add(1, Ordering::Relaxed) as f32;
    }
    fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
        for x in &mut output.channel_f32_mut(0)[..size] {
            *x = self.pos.fetch_add(1, Ordering::Relaxed) as f32;
        }
    }
    fn isolate(&mut self) {
        self.pos = Arc::new(AtomicU32::new(self.pos.load(Ordering::Relaxed)));
    }
    fn reset(&mut self) {
        self.pos.store(0, Ordering::Relaxed);
    }
    probe_boilerplate!();
}

/// A native node made forkable the way a Phase 4 node will be: an `IntoNode`
/// whose `into_parts` hands over a `ForkSource`. The fork is a fresh node of
/// the same kind.
struct Native(Kind);

struct KindFork(Kind);

impl ForkSource for KindFork {
    fn fork(&self, _mode: ForkMode<'_>) -> Box<dyn Node> {
        Box::new(TestNode::new(self.0.clone()))
    }
}

impl IntoNode for Native {
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

fn out(node: NodeKey, port: u16) -> Source {
    Source::Node(OutPort { node, port })
}

fn render(ed: Editor, exec: tutti_graph::Executor, frames: usize) -> Vec<Vec<f32>> {
    Renderer::new(ed, exec).render(frames)
}

/// sine → mix → lowpass, the lowpass fed back into the mix's second input
/// (256 frames, so the fork may run 256-frame blocks), and the lowpass fanned out to both outputs.
fn chain() -> GraphBuilder {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let osc = g.add_unit(Box::new(sine_hz(440.0)));
    let mix = g.add_unit(Box::new(pass() + mul(0.5)));
    let lp = g.add_unit(Box::new(lowpass_hz(1_200.0, 0.9)));
    g.connect(osc, 0, mix, 0)
        .feedback(lp, 0, mix, 1, Samples(256))
        .connect(mix, 0, lp, 0)
        .pipe_output(lp);
    g
}

/// **A forked `Legacy` chain renders exactly what a freshly built copy of
/// the same graph renders**, however long the live graph has run: nothing of
/// its running state — oscillator phase, filter memory, the feedback edge's
/// captured block — reaches the fork, and all of its wiring does. The fork is
/// prepared for its own `Prepare` (a larger block here), not the live one's.
///
/// Mutation: drop the `edges` copy in `Editor::fork` → the fork's filter
/// reads silence → fails. Mutation: drop `topology.outputs = outputs` →
/// every fork output is `Zero` → fails. Mutation: prepare the fork with the
/// live editor's `Prepare` → the `prepare()` assertion fails.
#[test]
fn a_forked_legacy_chain_renders_like_a_fresh_build() {
    let mut live = chain().renderer(prepare(64)).expect("builds");
    let before = live.render(1_000);
    assert!(before[0].iter().any(|&x| x != 0.0), "the live graph runs");

    let pre = prepare(256);
    let (fork_ed, fork_exec) = live
        .editor()
        .fork(ForkTarget::Master, ForkMode::Live, pre)
        .expect("every node is a Legacy");
    assert_eq!(fork_ed.prepare(), &pre);
    let forked = render(fork_ed, fork_exec, 3_000);

    let fresh = chain().renderer(pre).expect("builds").render(3_000);
    assert_eq!(bits(&forked), bits(&fresh));
    assert!(fresh[1].iter().any(|&x| x != 0.0), "not vacuous");
}

/// **`isolate`, then `rebind_offline`, then `reset`** — fundsp's order
/// (`PendingClone::isolate_for_offline`, then `Net::reset`). An offline fork
/// renders the context's value; a live fork is not rebound, so its unit is
/// cut loose and resets to 0; the live node keeps its own value throughout.
///
/// Mutation: rebind before isolating in `LegacyFork::fork` → the binding is
/// severed → the offline fork renders 0 → fails. Mutation: drop `reset` →
/// the fork keeps the live 0.25 → fails. Mutation: reset before rebinding →
/// `reset` reads no binding → 0 → fails. Mutation: rebind in `Live` too →
/// the live fork renders 0.75 → fails.
#[test]
fn a_fork_isolates_then_rebinds_then_resets() {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
    let key = g.add_unit(Box::new(Bindable {
        outs: 1,
        level: 0.25,
        bound: Some(0.25),
    }));
    g.pipe_output(key);
    let mut live = g.renderer(prepare(64)).expect("builds");
    assert!(live.render(64)[0].iter().all(|&x| x == 0.25));

    let ctx: f32 = 0.75;
    let (ed, exec) = live
        .editor()
        .fork(ForkTarget::Master, ForkMode::Offline(&ctx), prepare(64))
        .expect("forks");
    let offline = render(ed, exec, 64);
    assert!(
        offline[0].iter().all(|&x| x == 0.75),
        "{:?}",
        &offline[0][..4]
    );

    let (ed, exec) = live
        .editor()
        .fork(ForkTarget::Master, ForkMode::Live, prepare(64))
        .expect("forks");
    let dup = render(ed, exec, 64);
    assert!(dup[0].iter().all(|&x| x == 0.0), "{:?}", &dup[0][..4]);

    assert!(
        live.render(64)[0].iter().all(|&x| x == 0.25),
        "live untouched"
    );
}

/// **A setting sent after insert is in the fork**, through the shadow: the
/// fork of a `Legacy::controlled` node clones the shadow, which every
/// `LegacyControls::set` reaches at once — even one the live node has not
/// drained yet (no block has run since). A setting sent after the fork does
/// not reach it: the fork has no link back.
///
/// Mutation: fork a controlled node from a clone taken at insert
/// (`into_parts` ignoring `fork_from`) → the fork renders the constructed
/// 0.5 → fails.
#[test]
fn a_setting_sent_after_insert_reaches_the_fork() {
    let (mut ed, mut exec) = Editor::new(prepare(64));
    let (node, mut controls) = Legacy::controlled(&mut ed, Consts { outs: 1, base: 0.5 });
    let key = NodeKey(1);
    ed.insert(key, "consts", node);
    ed.spec_mut().topology.outputs = vec![out(key, 0)];
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();

    let _ = controls.set(Setting::value(0.75).index(0));
    let (fork_ed, fork_exec) = ed
        .fork(ForkTarget::Master, ForkMode::Live, prepare(64))
        .expect("forks");
    let _ = controls.set(Setting::value(0.9).index(0));
    let forked = render(fork_ed, fork_exec, 128);
    assert!(
        forked[0].iter().all(|&x| x == 0.75),
        "{:?}",
        &forked[0][..4]
    );
}

/// **A node without a fork source makes the fork refuse, naming it** —
/// before anything is forked. A native node inserted as itself, a `Legacy`
/// inserted as a bare `Box<dyn Node>`, and a forkable key replaced by an
/// unforkable unit are all unforkable; a target that does not exist or has
/// no outputs is refused too.
///
/// Mutation: skip keys without a source in `Editor::fork` → `Ok` → fails.
/// Mutation: keep the old fork source when `insert` replaces a unit with
/// one that has none → key 3 forks → fails.
#[test]
fn a_node_without_a_fork_source_is_not_forkable() {
    let (mut ed, _exec) = Editor::new(prepare(64));
    ed.insert(NodeKey(1), "legacy", Legacy::new(mul(2.0)));
    ed.insert(
        NodeKey(2),
        "native",
        TestNode::new(Kind::Gain {
            gain: 1.0,
            width: 1,
        }),
    );
    let t = &mut ed.spec_mut().topology;
    t.edges.insert(
        InPort {
            node: NodeKey(1),
            port: 0,
        },
        Edge::Direct(out(NodeKey(2), 0)),
    );
    t.outputs = vec![out(NodeKey(1), 0)];
    let pre = prepare(64);
    assert_eq!(
        ed.fork(ForkTarget::Master, ForkMode::Live, pre).err(),
        Some(ForkError::NotForkable { key: NodeKey(2) })
    );
    assert_eq!(
        ed.fork(ForkTarget::Node(NodeKey(1)), ForkMode::Live, pre)
            .err(),
        Some(ForkError::NotForkable { key: NodeKey(2) }),
        "upstream of the target"
    );

    let (mut ed, _exec) = Editor::new(pre);
    ed.insert(NodeKey(1), "boxed", Legacy::new(mul(2.0)).into_node().0);
    assert_eq!(
        ed.fork(ForkTarget::Master, ForkMode::Live, pre).err(),
        Some(ForkError::NotForkable { key: NodeKey(1) })
    );

    let (mut ed, _exec) = Editor::new(pre);
    ed.insert(NodeKey(3), "legacy", Legacy::new(mul(2.0)));
    assert!(ed.fork(ForkTarget::Master, ForkMode::Live, pre).is_ok());
    ed.insert(NodeKey(3), "boxed", Legacy::new(mul(2.0)).into_node().0);
    assert_eq!(
        ed.fork(ForkTarget::Master, ForkMode::Live, pre).err(),
        Some(ForkError::NotForkable { key: NodeKey(3) })
    );
    assert_eq!(
        ed.fork(ForkTarget::Node(NodeKey(9)), ForkMode::Live, pre)
            .err(),
        Some(ForkError::NoSuchNode { key: NodeKey(9) })
    );
    ed.insert(
        NodeKey(4),
        "sink",
        Legacy::new(Consts { outs: 0, base: 0.0 }),
    );
    assert_eq!(
        ed.fork(ForkTarget::Node(NodeKey(4)), ForkMode::Live, pre)
            .err(),
        Some(ForkError::NoOutputs { key: NodeKey(4) })
    );
}

/// Render `net` for `frames` frames of silence, planar.
fn render_net(net: &mut Net, frames: usize) -> Vec<Vec<f32>> {
    net.set_sample_rate(SampleRate(48_000.0));
    net.allocate();
    let ibuf = BufferVec::new(net.inputs());
    let mut obuf = BufferVec::new(net.outputs());
    let mut out = vec![Vec::new(); net.outputs()];
    let mut done = 0;
    while done < frames {
        let n = (frames - done).min(MAX_BUFFER_SIZE);
        net.process(n, &ibuf.buffer_ref(), &mut obuf.buffer_mut());
        for (c, o) in out.iter_mut().enumerate() {
            o.extend_from_slice(&obuf.channel_f32_mut(c)[..n]);
        }
        done += n;
    }
    out
}

/// **A node fork's outputs follow `Net::clone_isolated`, checked against
/// `Net` itself**: channel `c` reads the node's port `min(c, outs - 1)` — a
/// mono node on every channel, and a wider graph *clamped* to the node's
/// last port (stereo into six is L R R R R R), not wrapped as `pipe_output`
/// wraps. Walked over a grid of node and graph widths.
///
/// Mutation: `c % outs` instead of the clamp → 2-into-3 reads port 0 on
/// channel 2 → fails.
#[test]
fn a_node_fork_fans_out_as_clone_isolated_does() {
    for node_outs in 1..=3 {
        for graph_outs in [1usize, 2, 3, 6] {
            let unit = Consts {
                outs: node_outs,
                base: 10.0,
            };
            let mut net = Net::new(0, graph_outs);
            let id = net.push(Box::new(unit.clone()));
            let mut want = net.clone_isolated(id).expect("has outputs").isolate();
            let want = render_net(&mut want, 64);

            let (mut ed, _exec) = Editor::new(prepare(64));
            let key = NodeKey(7);
            ed.insert(key, "consts", Legacy::new(unit));
            ed.spec_mut().topology.outputs = vec![Source::Zero; graph_outs];
            let (fe, fx) = ed
                .fork(ForkTarget::Node(key), ForkMode::Live, prepare(64))
                .expect("forks");
            let got = render(fe, fx, 64);
            assert_eq!(bits(&got), bits(&want), "{node_outs} into {graph_outs}");
        }
    }
}

/// **A node fork holds exactly the sub-graph feeding the node**: what
/// reaches it back along an audio edge (`osc` → `a`), a feedback edge
/// (`fb` → `a`, delayed) and an event edge (`emit` → `target`) — and not the
/// node it feeds (`after`) or a sibling branch (`side`, which is not even
/// forkable, and so must not be asked). The wiring inside comes along.
///
/// Mutation: drop the feedback arm from `Editor::upstream` → `fb` is
/// missing and `a`'s edge dangles → the fork fails to commit → fails.
/// Mutation: drop the event walk → `emit` is missing → fails. Mutation:
/// fork every node for `Node` too → `side` is `NotForkable` → fails.
#[test]
fn a_node_fork_holds_exactly_what_feeds_the_node() {
    let (osc, a, fb, emit, target, after, side) = (
        NodeKey(1),
        NodeKey(2),
        NodeKey(3),
        NodeKey(4),
        NodeKey(5),
        NodeKey(6),
        NodeKey(7),
    );
    let (mut ed, _exec) = Editor::new(prepare(64));
    ed.insert(osc, "osc", Legacy::new(sine_hz(220.0)));
    ed.insert(a, "mix", Legacy::new(pass() + mul(0.5)));
    ed.insert(
        fb,
        "fb",
        Legacy::new(Consts {
            outs: 1,
            base: 0.25,
        }),
    );
    ed.insert(
        emit,
        "emit",
        Native(Kind::Emitter {
            period: 50,
            phase: 7,
        }),
    );
    ed.insert(
        target,
        "target",
        Native(Kind::Mixed {
            width: 1,
            events_in: 1,
            events_out: 0,
        }),
    );
    ed.insert(after, "after", Legacy::new(mul(2.0)));
    ed.insert(
        side,
        "side",
        TestNode::new(Kind::Const {
            value: 1.0,
            width: 1,
        }),
    );
    let spec = ed.spec_mut();
    let t = &mut spec.topology;
    t.edges
        .insert(InPort { node: a, port: 0 }, Edge::Direct(out(osc, 0)));
    t.edges.insert(
        InPort { node: a, port: 1 },
        Edge::Feedback(FeedbackFrom::new(
            OutPort { node: fb, port: 0 },
            Samples(64),
        )),
    );
    t.edges.insert(
        InPort {
            node: target,
            port: 0,
        },
        Edge::Direct(out(a, 0)),
    );
    t.edges.insert(
        InPort {
            node: after,
            port: 0,
        },
        Edge::Direct(out(target, 0)),
    );
    t.outputs = vec![out(after, 0), out(side, 0)];
    spec.connect_events(
        EventIn {
            node: target,
            port: 0,
        },
        EventEdge::Direct(EventOut {
            node: emit,
            port: 0,
        }),
    );
    ed.commit().expect("the live graph commits");

    let (fe, fx) = ed
        .fork(ForkTarget::Node(target), ForkMode::Live, prepare(64))
        .expect("forks");
    let keys: BTreeSet<NodeKey> = fe.spec().topology.nodes.keys().copied().collect();
    assert_eq!(keys, BTreeSet::from([osc, a, fb, emit, target]));
    assert_eq!(
        fe.spec().topology.edges.len(),
        3,
        "a's two inputs and the target's"
    );
    assert_eq!(fe.spec().events, ed.spec().events);
    assert_eq!(fe.spec().topology.outputs, vec![out(target, 0); 2]);
    let got = render(fe, fx, 256);
    assert!(got[0].iter().any(|&x| x != 0.0), "not vacuous");
}

/// **The live graph is unaffected while a fork renders on another thread**:
/// a live graph that was forked renders bit-identically to a twin that never
/// was, while the fork renders concurrently. The graph holds a unit whose
/// clone shares a cell with it (`Shared`), which is exactly what `isolate`
/// exists to sever, and a `controlled` node whose shadow the fork clones.
///
/// Mutation: drop `unit.isolate()` in `LegacyFork::fork` → the fork's
/// `reset` rewinds and its rendering advances the live ramp's cell → the
/// live output leaves the twin's → fails.
#[test]
fn the_live_graph_is_unaffected_while_a_fork_renders() {
    fn build() -> (
        Editor,
        tutti_graph::Executor,
        tutti_graph::LegacyControls<Consts>,
    ) {
        let (mut ed, mut exec) = Editor::new(prepare(64));
        let (node, controls) = Legacy::controlled(&mut ed, Consts { outs: 1, base: 0.5 });
        ed.insert(NodeKey(1), "ramp", Legacy::new(Shared::new()));
        ed.insert(NodeKey(2), "consts", node);
        ed.spec_mut().topology.outputs = vec![out(NodeKey(1), 0), out(NodeKey(2), 0)];
        ed.commit().expect("commits");
        exec.apply_pending();
        ed.collect();
        (ed, exec, controls)
    }
    let (ed, exec, mut controls) = build();
    let (twin_ed, twin_exec, mut twin_controls) = build();
    let mut live = Renderer::new(ed, exec);
    let mut twin = Renderer::new(twin_ed, twin_exec);
    let _ = controls.set(Setting::value(0.75).index(0));
    let _ = twin_controls.set(Setting::value(0.75).index(0));
    assert_eq!(bits(&live.render(640)), bits(&twin.render(640)));

    let (fe, fx) = live
        .editor()
        .fork(ForkTarget::Master, ForkMode::Live, prepare(64))
        .expect("forks");
    let worker = std::thread::spawn(move || render(fe, fx, 48_000));
    let mut a = Vec::new();
    let mut b = Vec::new();
    for _ in 0..200 {
        a.push(live.render(64));
        b.push(twin.render(64));
    }
    let forked = worker.join().expect("the fork renders");
    for (i, (a, b)) in a.iter().zip(&b).enumerate() {
        assert_eq!(bits(a), bits(b), "block {i}");
    }
    assert_eq!(forked[0][0], 0.0, "the fork's ramp starts over");
    assert_eq!(forked[0][47_999], 47_999.0, "and is its own");
    assert!(forked[1].iter().all(|&x| x == 0.75));
}
