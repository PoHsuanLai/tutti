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
    CrossfadeCurve, Editor, EventEdge, EventIn, EventOut, Fade, ForkCause, ForkError, ForkFault,
    ForkFaultKind, ForkHealth, ForkMode, ForkSource, ForkTarget, Forked, GraphBuilder, IntoNode,
    Legacy, Node, NodeParts, Renderer, Unforkable,
};
use tutti_node::buffer::{BufferMut, BufferRef, BufferVec};
use tutti_node::signal::{Signal, SignalFrame};
use tutti_node::{Address, AudioUnit, Parameter, Setting, MAX_BUFFER_SIZE};
use tutti_types::graph::{Edge, FeedbackFrom, InPort, OutPort, Source};
use tutti_types::{
    Beat, Bpm, ChannelLayout, NodeKey, OfflineClock, OfflineTransport, SampleRate, Samples,
    Timeline,
};

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
/// context (its timeline's beat), and `reset` re-reads `level` from the
/// binding — so only isolate → rebind → reset leaves `level` at the
/// context's value.
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
    fn rebind_offline(&mut self, ctx: &OfflineTransport) {
        self.bound = Some(ctx.beat().get() as f32);
    }
    fn reset(&mut self) {
        self.level = self.bound.unwrap_or(0.0);
    }
    probe_boilerplate!();
}

/// An offline context standing still at `beat`: what `Bindable` binds to.
struct At(f64);

impl Timeline for At {
    fn beat(&self) -> Beat {
        Beat(self.0)
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

impl OfflineClock for At {}

fn at(beat: f64) -> OfflineTransport {
    OfflineTransport::new(Arc::new(At(beat)))
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
    fn fork(&self, _mode: ForkMode<'_>) -> Result<Forked, ForkCause> {
        Ok(Forked::new(Box::new(TestNode::new(self.0.clone()))))
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

    let ctx = at(0.75);
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

/// **A `Legacy` fork hook runs after the rebind and before the reset, and
/// its `Err` fails the fork by key with its cause.** The hook doubles
/// `Bindable`'s binding: offline, the fork renders twice the context (1.5)
/// only if the hook saw the rebound 0.75 and `reset` then read what it left.
/// A hook refusing the fork is `ForkError::Source` naming the node.
///
/// Mutation (run): the hook called before `rebind_offline` → it doubles no
/// binding, the fork renders 0.75. Mutation (run): after `reset` → `reset`
/// read the undoubled 0.75. Mutation (run): its `Err` ignored (`let _ =`) →
/// the refusing fork succeeds.
#[test]
fn a_legacy_fork_hook_runs_between_rebind_and_reset() {
    let pre = prepare(64);
    let doubling = |unit: &mut dyn AudioUnit, mode: ForkMode<'_>| {
        if let ForkMode::Offline(_) = mode {
            let b = unit.as_any_mut().downcast_mut::<Bindable>().unwrap();
            b.bound = b.bound.map(|v| v * 2.0);
        }
        Ok(())
    };
    let (mut ed, _exec) = Editor::new(pre);
    ed.insert(
        NodeKey(1),
        "hooked",
        Legacy::new(Bindable {
            outs: 1,
            level: 0.25,
            bound: Some(0.25),
        })
        .with_fork_hook(doubling),
    );
    ed.spec_mut().topology.outputs = vec![out(NodeKey(1), 0)];
    let ctx = at(0.75);
    let (fork, exec) = ed
        .fork(ForkTarget::Master, ForkMode::Offline(&ctx), pre)
        .expect("forks");
    let offline = render(fork, exec, 64);
    assert!(
        offline[0].iter().all(|&x| x == 1.5),
        "{:?}",
        &offline[0][..4]
    );

    let (mut ed, _exec) = Editor::new(pre);
    ed.insert(
        NodeKey(2),
        "refusing",
        Legacy::new(Bindable {
            outs: 1,
            level: 0.0,
            bound: None,
        })
        .with_fork_hook(|_, _| Err(ForkCause::new(RefusedState("hook")))),
    );
    ed.spec_mut().topology.outputs = vec![out(NodeKey(2), 0)];
    let err = ed
        .fork(ForkTarget::Master, ForkMode::Offline(&ctx), pre)
        .err()
        .expect("the hook refuses the fork");
    let ForkError::Source { key, cause } = &err else {
        panic!("expected ForkError::Source, got {err:?}");
    };
    assert_eq!(*key, NodeKey(2));
    assert_eq!(
        cause.downcast_ref::<RefusedState>(),
        Some(&RefusedState("hook"))
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
/// before anything is forked. A native node inserted `Unforkable`, a `Legacy`
/// inserted as an `Unforkable` `Box<dyn Node>`, and a forkable key replaced by an
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
        Unforkable(TestNode::new(Kind::Gain {
            gain: 1.0,
            width: 1,
        })),
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

    // Routed to an output: a master fork forks only what the outputs reach
    // (`a_master_fork_holds_only_what_the_outputs_reach`).
    let (mut ed, _exec) = Editor::new(pre);
    ed.insert(
        NodeKey(1),
        "boxed",
        Unforkable(Legacy::new(mul(2.0)).into_node().0),
    );
    ed.spec_mut().topology.outputs = vec![out(NodeKey(1), 0)];
    assert_eq!(
        ed.fork(ForkTarget::Master, ForkMode::Live, pre).err(),
        Some(ForkError::NotForkable { key: NodeKey(1) })
    );

    let (mut ed, _exec) = Editor::new(pre);
    ed.insert(NodeKey(3), "legacy", Legacy::new(mul(2.0)));
    ed.spec_mut().topology.outputs = vec![out(NodeKey(3), 0)];
    assert!(ed.fork(ForkTarget::Master, ForkMode::Live, pre).is_ok());
    ed.insert(
        NodeKey(3),
        "boxed",
        Unforkable(Legacy::new(mul(2.0)).into_node().0),
    );
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

/// A probe a test flips: 0 healthy, 1 crashed, 2 timed out.
struct Flag(std::sync::atomic::AtomicU8);

impl ForkHealth for Flag {
    fn fault(&self) -> Option<(ForkFaultKind, ForkCause)> {
        match self.0.load(Ordering::SeqCst) {
            0 => None,
            1 => Some((ForkFaultKind::Crashed, ForkCause::new(RefusedState("died")))),
            _ => Some((
                ForkFaultKind::TimedOut,
                ForkCause::new(RefusedState("hung")),
            )),
        }
    }
}

/// A source whose units carry the shared [`Flag`].
struct Watched(Arc<Flag>);

impl IntoNode for Watched {
    type Controls = ();
    fn into_node(self) -> (Box<dyn Node>, ()) {
        (
            Box::new(TestNode::new(Kind::Const {
                value: 1.0,
                width: 1,
            })),
            (),
        )
    }
    fn into_parts(self) -> NodeParts<()> {
        let flag = Arc::clone(&self.0);
        NodeParts {
            node: self.into_node().0,
            controls: (),
            fork: Some(Box::new(WatchedFork(flag))),
        }
    }
}

struct WatchedFork(Arc<Flag>);

impl ForkSource for WatchedFork {
    fn fork(&self, _mode: ForkMode<'_>) -> Result<Forked, ForkCause> {
        let node = Box::new(TestNode::new(Kind::Const {
            value: 1.0,
            width: 1,
        }));
        Ok(Forked::new(node).with_health(Arc::clone(&self.0) as Arc<dyn ForkHealth>))
    }
}

/// **A forked unit that fails while rendering is reported by the forked
/// editor**, with its key and how (crashed vs timed out); healthy is `Ok`,
/// and the live editor, which holds no forked units, is always `Ok`.
///
/// Mutation: drop `watch_fork` in `Editor::fork` → the fault is never seen →
/// fails. Mutation: `fork_health` returns `Ok(())` → fails.
#[test]
fn a_forked_unit_that_fails_while_rendering_is_a_fork_fault() {
    let pre = prepare(64);
    let flag = Arc::new(Flag(std::sync::atomic::AtomicU8::new(0)));
    let (mut ed, _exec) = Editor::new(pre);
    ed.insert(NodeKey(1), "legacy", Legacy::new(mul(2.0)));
    ed.insert(NodeKey(4), "watched", Watched(Arc::clone(&flag)));
    ed.spec_mut().topology.outputs = vec![out(NodeKey(1), 0), out(NodeKey(4), 0)];

    let (fork, _fork_exec) = ed
        .fork(ForkTarget::Master, ForkMode::Live, pre)
        .expect("forks");
    assert_eq!(fork.fork_health(), Ok(()));
    flag.0.store(1, Ordering::SeqCst);
    let fault: ForkFault = fork.fork_health().expect_err("crashed");
    assert_eq!(
        (fault.key, fault.kind),
        (NodeKey(4), ForkFaultKind::Crashed)
    );
    flag.0.store(2, Ordering::SeqCst);
    let fault = fork.fork_health().expect_err("timed out");
    assert_eq!(
        (fault.key, fault.kind),
        (NodeKey(4), ForkFaultKind::TimedOut)
    );
    assert_eq!(fault.cause.to_string(), "refused: hung");
    assert_eq!(ed.fork_health(), Ok(()), "the live editor watches nothing");
}

/// A unit whose offline copy fails on its first rendered block: a latch its
/// `isolate` makes fresh (a copy never reports the live unit's failures, nor
/// the live unit a copy's), handed over by `render_fault`.
#[derive(Clone)]
struct FailsOffline {
    outs: usize,
    latch: Arc<tutti_node::FaultLatch>,
}

impl AudioUnit for FailsOffline {
    probe_boilerplate!();
    fn isolate(&mut self) {
        self.latch = Arc::default();
    }
    fn render_fault(&self) -> Option<Arc<dyn tutti_node::RenderFault>> {
        Some(Arc::clone(&self.latch) as Arc<dyn tutti_node::RenderFault>)
    }
    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        output.fill(0.0);
    }
    fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
        self.latch.latch(RefusedState("unreadable"));
        for c in 0..self.outs {
            output.channel_f32_mut(c)[..size].fill(0.0);
        }
    }
}

/// **A `Legacy` unit that fails while rendering offline fails its fork**:
/// the copy's `render_fault` probe reaches the forked editor, which reports
/// it (`ForkFaultKind::Failed`, the unit's own cause) once the copy has
/// rendered, and not before; the live editor and the live unit see nothing.
///
/// Mutation (run): `LegacyFork::fork` not asking `render_fault` (a plain
/// `Forked::new`) → the fault is never seen → fails.
#[test]
fn a_legacy_unit_that_fails_offline_is_a_fork_fault() {
    let pre = prepare(64);
    let live = FailsOffline {
        outs: 1,
        latch: Arc::default(),
    };
    let live_latch = Arc::clone(&live.latch);
    let (mut ed, _exec) = Editor::new(pre);
    ed.insert(NodeKey(2), "disk", Legacy::new(live));
    ed.spec_mut().topology.outputs = vec![out(NodeKey(2), 0)];

    let (fork, fork_exec) = ed
        .fork(ForkTarget::Master, ForkMode::Live, pre)
        .expect("forks");
    assert_eq!(fork.fork_health(), Ok(()), "healthy before it renders");
    let mut renderer = Renderer::new(fork, fork_exec);
    renderer.render(64);
    let fault = renderer
        .editor()
        .fork_health()
        .expect_err("the copy failed");
    assert_eq!((fault.key, fault.kind), (NodeKey(2), ForkFaultKind::Failed));
    assert_eq!(fault.cause.to_string(), "refused: unreadable");
    assert_eq!(ed.fork_health(), Ok(()));
    assert!(
        tutti_node::RenderFault::fault(&*live_latch).is_none(),
        "the live unit saw the copy's failure"
    );
}

/// Why a [`FailingFork`] fails: a type the test can downcast back out.
#[derive(Debug, PartialEq)]
struct RefusedState(&'static str);

impl std::fmt::Display for RefusedState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "refused: {}", self.0)
    }
}

impl std::error::Error for RefusedState {}

/// A source that cannot produce its unit — the shape of a hosted plugin
/// whose fresh instance refuses the live one's state.
struct FailingFork;

impl ForkSource for FailingFork {
    fn fork(&self, _mode: ForkMode<'_>) -> Result<Forked, ForkCause> {
        Err(ForkCause::new(RefusedState("bad chunk")))
    }
}

struct Failing;

impl IntoNode for Failing {
    type Controls = ();
    fn into_node(self) -> (Box<dyn Node>, ()) {
        (
            Box::new(TestNode::new(Kind::Const {
                value: 1.0,
                width: 1,
            })),
            (),
        )
    }
    fn into_parts(self) -> NodeParts<()> {
        NodeParts {
            node: self.into_node().0,
            controls: (),
            fork: Some(Box::new(FailingFork)),
        }
    }
}

/// **A source that fails fails the fork, naming its key and keeping its
/// cause**: `ForkError::Source`, with the source's own error downcastable —
/// never a fork that quietly leaves the node out, or renders it as silence.
/// It is `Error::source` too, so a host printing the chain sees why.
///
/// Mutation: in `Editor::fork`, skip a node whose source fails (`continue`
/// instead of `?`) → the fork commits with the node missing → `.err()` is
/// `None` → fails. Mutation: return `NotForkable` instead → the cause is
/// lost → fails.
#[test]
fn a_failing_fork_source_is_a_named_error_with_its_cause() {
    let pre = prepare(64);
    let (mut ed, _exec) = Editor::new(pre);
    ed.insert(NodeKey(1), "legacy", Legacy::new(mul(2.0)));
    ed.insert(NodeKey(5), "plugin-like", Failing);
    ed.spec_mut().topology.outputs = vec![out(NodeKey(1), 0), out(NodeKey(5), 0)];

    let err = ed
        .fork(ForkTarget::Master, ForkMode::Live, pre)
        .err()
        .expect("a failing source fails the fork");
    let ForkError::Source { key, cause } = &err else {
        panic!("expected ForkError::Source, got {err:?}");
    };
    assert_eq!(*key, NodeKey(5));
    assert_eq!(
        cause.downcast_ref::<RefusedState>(),
        Some(&RefusedState("bad chunk"))
    );
    let source = std::error::Error::source(&err).expect("the cause is the source");
    assert_eq!(source.to_string(), "refused: bad chunk");
    assert_eq!(err.clone(), err, "a clone shares its cause");

    // The target's sub-graph excludes the failing node, so forking it works.
    assert!(ed
        .fork(ForkTarget::Node(NodeKey(1)), ForkMode::Live, pre)
        .is_ok());
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
        Unforkable(TestNode::new(Kind::Const {
            value: 1.0,
            width: 1,
        })),
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

/// **A replace moves forking to the new unit**: a fork after
/// `Editor::replace` forks the incoming unit, a replace with a unit that
/// hands over no fork source makes the key unforkable, and a *refused*
/// replace (a different width) changes nothing.
///
/// Mutation: pass no fork source to `place` from `Editor::replace` → the
/// fork after a forkable replace is `NotForkable` → fails. (Writing the
/// source before the shape check, which the refused replace would expose,
/// is no longer expressible: `place` writes it, past the checks.)
#[test]
fn a_replace_moves_forking_to_the_new_unit() {
    let consts = |outs, base| Legacy::new(Consts { outs, base });
    let fade = Fade::new(Samples(64), CrossfadeCurve::EqualPower);
    let key = NodeKey(1);
    let (mut ed, mut exec) = Editor::new(prepare(64));
    ed.insert(key, "consts", consts(1, 0.5));
    ed.spec_mut().topology.outputs = vec![out(key, 0)];
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();

    ed.replace(key, consts(1, 0.9), fade).expect("same shape");
    assert!(matches!(
        ed.replace(key, consts(2, 0.1), fade),
        Err(tutti_graph::CommitError::FadeShape { .. })
    ));
    let (fe, fx) = ed
        .fork(ForkTarget::Master, ForkMode::Live, prepare(64))
        .expect("forks");
    let forked = render(fe, fx, 64);
    assert!(forked[0].iter().all(|&x| x == 0.9), "{:?}", &forked[0][..4]);

    ed.replace(key, Unforkable(consts(1, 0.7).into_node().0), fade)
        .expect("same shape");
    assert_eq!(
        ed.fork(ForkTarget::Master, ForkMode::Live, prepare(64))
            .err(),
        Some(ForkError::NotForkable { key })
    );
}

/// Stands in for a mic monitor: a clone shares the consumer end of a ring
/// (`Arc<Mutex<VecDeque>>` here), so a fork would take live frames, and no
/// `isolate` can sever an SPSC consumer onto a second one. It says so.
#[derive(Clone)]
struct MicLike {
    outs: usize,
    ring: Arc<std::sync::Mutex<std::collections::VecDeque<f32>>>,
}

impl AudioUnit for MicLike {
    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        output[0] = self.ring.lock().unwrap().pop_front().unwrap_or(0.0);
    }
    fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
        let mut ring = self.ring.lock().unwrap();
        for x in &mut output.channel_f32_mut(0)[..size] {
            *x = ring.pop_front().unwrap_or(0.0);
        }
    }
    fn forkable(&self) -> bool {
        false
    }
    probe_boilerplate!();
}

/// Stands in for a plugin client: a clone shares the bridge to the one
/// plugin process, and `reset` goes over it — a fork's reset would reach the
/// live plugin. It says so.
#[derive(Clone)]
struct PluginLike {
    outs: usize,
    bridge: Arc<AtomicU32>,
}

impl AudioUnit for PluginLike {
    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        output[0] = self.bridge.load(Ordering::Relaxed) as f32;
    }
    fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
        let v = self.bridge.load(Ordering::Relaxed) as f32;
        output.channel_f32_mut(0)[..size].fill(v);
    }
    fn reset(&mut self) {
        self.bridge.store(0, Ordering::Relaxed);
    }
    fn forkable(&self) -> bool {
        false
    }
    probe_boilerplate!();
}

/// **A unit that says it cannot be forked is not**, however it is wrapped:
/// a mic-like unit (shared ring consumer), a plugin-like unit (shared
/// bridge), a mic-like unit inside a `Net` used as a node, and any unit
/// built `Legacy::unforkable` are all `NotForkable` — and the refusal comes
/// before any fork is made, so the live plugin-like unit's bridge is never
/// reset.
///
/// Mutation: ignore `forkable()` in `Legacy::into_parts` → the mic and
/// plugin forks succeed (and the plugin's bridge is zeroed) → fails.
/// Mutation: drop `Net::forkable`'s forwarding → the wrapped mic forks →
/// fails. Mutation: ignore the `unforkable` flag → fails.
#[test]
fn a_unit_that_is_not_forkable_is_refused() {
    let pre = prepare(64);
    let refused = |node: Legacy| {
        let (mut ed, _exec) = Editor::new(pre);
        ed.insert(NodeKey(1), "unit", node);
        ed.spec_mut().topology.outputs = vec![out(NodeKey(1), 0)];
        ed.fork(ForkTarget::Master, ForkMode::Live, pre).err()
    };
    let not = Some(ForkError::NotForkable { key: NodeKey(1) });
    let mic = || MicLike {
        outs: 1,
        ring: Arc::new(std::sync::Mutex::new([0.5f32; 8].into())),
    };
    assert_eq!(refused(Legacy::new(mic())), not, "mic-like");

    let bridge = Arc::new(AtomicU32::new(3));
    let plugin = PluginLike {
        outs: 1,
        bridge: Arc::clone(&bridge),
    };
    assert_eq!(refused(Legacy::new(plugin)), not, "plugin-like");
    assert_eq!(
        bridge.load(Ordering::Relaxed),
        3,
        "the live bridge untouched"
    );

    let mut net = Net::new(0, 1);
    let id = net.push(Box::new(mic()));
    net.pipe_output(id);
    assert_eq!(refused(Legacy::new(net)), not, "inside a Net");

    let consts = || Consts { outs: 1, base: 0.5 };
    assert_eq!(
        refused(Legacy::new(consts()).unforkable()),
        not,
        "opted out"
    );
    assert_eq!(refused(Legacy::new(consts())), None);
}

/// **A fork source from an older generation is refused**, loudly: if the
/// spec's generation at a key moved on without a new source (written
/// through `spec_mut`, or by a `package` placing units the editor never
/// saw), forking would copy a unit that is no longer there. Debug builds
/// assert; release builds return `NotForkable`.
///
/// Mutation: drop the generation comparison in `Editor::fork` → the fork
/// succeeds → fails (both builds).
#[test]
#[cfg_attr(debug_assertions, should_panic(expected = "is from generation 0"))]
fn a_fork_source_from_an_older_generation_is_refused() {
    let pre = prepare(64);
    let (mut ed, _exec) = Editor::new(pre);
    ed.insert(
        NodeKey(1),
        "consts",
        Legacy::new(Consts { outs: 1, base: 0.5 }),
    );
    ed.spec_mut().topology.outputs = vec![out(NodeKey(1), 0)];
    assert!(ed.fork(ForkTarget::Master, ForkMode::Live, pre).is_ok());
    ed.spec_mut().generations.insert(NodeKey(1), 7);
    assert_eq!(
        ed.fork(ForkTarget::Master, ForkMode::Live, pre).err(),
        Some(ForkError::NotForkable { key: NodeKey(1) })
    );
}

/// A parameter in an `Arc` cell a clone shares — `SvfFilterNode`'s shape:
/// its param handles write the cell, `isolate` copies the value into a cell
/// of its own. Outputs the value.
#[derive(Clone)]
struct CellParam {
    outs: usize,
    value: Arc<AtomicU32>,
}

impl AudioUnit for CellParam {
    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        output[0] = f32::from_bits(self.value.load(Ordering::Relaxed));
    }
    fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
        let v = f32::from_bits(self.value.load(Ordering::Relaxed));
        output.channel_f32_mut(0)[..size].fill(v);
    }
    fn isolate(&mut self) {
        let v = self.value.load(Ordering::Relaxed);
        self.value = Arc::new(AtomicU32::new(v));
    }
    probe_boilerplate!();
}

/// **A plain `Legacy` reads a shared cell at fork time**, not at insert:
/// its insert-time copy is deliberately not isolated, so a value a handle
/// writes into the unit's cell after insert is what the fork gets — and the
/// fork's own cell is severed, so a later write reaches only the live unit.
///
/// Mutation: isolate the insert-time clone in `Legacy::into_parts` → the
/// fork renders the insert-time 0.2 → fails. Mutation: drop the fork's
/// `isolate` → the write after the fork reaches it → fails.
#[test]
fn a_plain_legacy_reads_shared_cells_at_fork_time() {
    let cell = Arc::new(AtomicU32::new(0.2f32.to_bits()));
    let (mut ed, _exec) = Editor::new(prepare(64));
    ed.insert(
        NodeKey(1),
        "cell",
        Legacy::new(CellParam {
            outs: 1,
            value: Arc::clone(&cell),
        }),
    );
    ed.spec_mut().topology.outputs = vec![out(NodeKey(1), 0)];
    cell.store(0.8f32.to_bits(), Ordering::Relaxed);
    let (fe, fx) = ed
        .fork(ForkTarget::Master, ForkMode::Live, prepare(64))
        .expect("forks");
    let mut r = Renderer::new(fe, fx);
    assert!(r.render(64)[0].iter().all(|&x| x == 0.8));
    cell.store(0.1f32.to_bits(), Ordering::Relaxed);
    assert!(r.render(64)[0].iter().all(|&x| x == 0.8), "severed");
}

/// **A fork carries the spec's parameter values, the editor's event
/// capacity, and only the resolution marks inside what it forks.** The
/// emitter writes an event every frame into slots the live editor sized for
/// 8 per block, so the fork drops some, as the live graph would; the mark
/// on the sibling edge (outside a `Node` fork) is left behind, or the fork
/// could not commit.
///
/// Mutation: drop the params copy in `Editor::fork` → fails. Mutation: fork
/// with `DEFAULT_EVENT_CAPACITY` → nothing dropped → fails. Mutation: copy
/// `required_resolution` unfiltered → the fork's commit fails
/// (`RequirementWithoutEdge`) → fails. Mutation: drop the copy → the kept
/// mark is missing → fails.
#[test]
fn a_fork_carries_params_event_capacity_and_its_own_marks() {
    use tutti_graph::Resolution;
    use tutti_types::graph::ParamValue;
    let (emit, fold, emit2, fold2) = (NodeKey(1), NodeKey(2), NodeKey(3), NodeKey(4));
    let pre = prepare(64);
    let (mut ed, _exec) = Editor::with_event_capacity(pre, 8);
    let emitter = || {
        Native(Kind::Emitter {
            period: 1,
            phase: 0,
        })
    };
    ed.insert(emit, "emit", emitter());
    ed.insert(fold, "fold", Native(Kind::Consumer { inputs: 1 }));
    ed.insert(emit2, "emit", emitter());
    ed.insert(fold2, "fold", Native(Kind::Consumer { inputs: 1 }));
    let spec = ed.spec_mut();
    spec.topology.outputs = vec![out(fold, 0), out(fold2, 0)];
    for (e, f) in [(emit, fold), (emit2, fold2)] {
        let at = EventIn { node: f, port: 0 };
        let from = EventOut { node: e, port: 0 };
        spec.connect_events(at, EventEdge::Direct(from));
        spec.require_resolution(at, from, Resolution::Sample);
    }
    spec.topology
        .nodes
        .get_mut(&fold)
        .expect("inserted")
        .params
        .insert("drive".into(), ParamValue::Scalar(0.3));

    let (fe, mut fx) = ed
        .fork(ForkTarget::Node(fold), ForkMode::Live, pre)
        .expect("forks");
    assert_eq!(
        fe.spec().topology.nodes[&fold].params,
        ed.spec().topology.nodes[&fold].params
    );
    let mark = (
        EventIn {
            node: fold,
            port: 0,
        },
        EventOut {
            node: emit,
            port: 0,
        },
    );
    assert_eq!(
        fe.spec()
            .required_resolution
            .keys()
            .copied()
            .collect::<Vec<_>>(),
        vec![mark]
    );
    let (mut l, mut r) = (vec![0.0f32; 64], vec![0.0f32; 64]);
    fx.process(
        64,
        &tutti_graph::Transport::default(),
        &[],
        &mut [&mut l[..], &mut r[..]],
    );
    assert!(fx.dropped_events() > 0, "the fork has the live capacity");
}

/// **A node fork of a graph with no global outputs is `NoOutputs`**, not an
/// empty fork that renders nothing.
///
/// Mutation: drop the `outputs.is_empty()` check → `Ok` → fails.
#[test]
fn a_node_fork_with_no_global_outputs_is_no_outputs() {
    let (mut ed, _exec) = Editor::new(prepare(64));
    ed.insert(
        NodeKey(1),
        "consts",
        Legacy::new(Consts { outs: 1, base: 0.5 }),
    );
    assert_eq!(
        ed.fork(ForkTarget::Node(NodeKey(1)), ForkMode::Live, prepare(64))
            .err(),
        Some(ForkError::NoOutputs { key: NodeKey(1) })
    );
}

/// A native ramp: its output is the frame count since its last `reset`
/// (or the value it was built with), kept in a plain field, so a `Clone`
/// shares nothing.
#[derive(Clone)]
struct Ramp {
    n: f32,
}

impl Node for Ramp {
    fn shape(&self) -> tutti_graph::Shape {
        tutti_graph::Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO)
            .with_tail(tutti_types::Tail::Unbounded)
    }
    fn prepare(&mut self, _p: &tutti_graph::Prepare) {}
    fn process(
        &mut self,
        _cx: &tutti_graph::Cx<'_>,
        mut io: tutti_graph::Io<'_>,
    ) -> tutti_graph::Status {
        for s in io.output(0).iter_mut() {
            *s = self.n;
            self.n += 1.0;
        }
        tutti_graph::Status::Modified
    }
    fn reset(&mut self) {
        self.n = 0.0;
    }
}

/// **A native node inserted as `ForkByClone` forks, from reset; the same
/// node inserted plainly does not.** The ramp is built at 7 and the live one
/// runs 300 frames first, so a fork that kept either would not start at 0.
///
/// Mutation (run): `ForkByClone::into_parts` handing no fork source → the
/// fork is `NotForkable`. Mutation (run): `CloneFork::fork` not calling
/// `reset` → the fork starts at 7.
#[test]
fn a_fork_by_clone_node_forks_from_reset() {
    let (mut ed, exec) = Editor::new(prepare(256));
    ed.insert(
        NodeKey(1),
        "ramp",
        tutti_graph::ForkByClone(Ramp { n: 7.0 }),
    );
    ed.spec_mut().topology.outputs = vec![out(NodeKey(1), 0)];
    ed.commit().unwrap();
    let mut live = Renderer::new(ed, exec);
    let before = live.render(300);
    assert_eq!(
        before[0][0], 7.0,
        "the live node runs from where it was built"
    );
    let (fork_ed, fork_exec) = live
        .editor()
        .fork(ForkTarget::Master, ForkMode::Live, prepare(256))
        .expect("a ForkByClone node forks");
    let forked = render(fork_ed, fork_exec, 4);
    assert_eq!(forked[0], vec![0.0, 1.0, 2.0, 3.0]);

    let (mut plain, _exec) = Editor::new(prepare(256));
    plain.insert(NodeKey(1), "ramp", Unforkable(Ramp { n: 0.0 }));
    plain.spec_mut().topology.outputs = vec![out(NodeKey(1), 0)];
    assert_eq!(
        plain
            .fork(ForkTarget::Master, ForkMode::Live, prepare(256))
            .err(),
        Some(ForkError::NotForkable { key: NodeKey(1) })
    );
}

/// **`ForkTarget::Master` forks what the outputs hear, and nothing else.** An
/// unforkable node no output reaches (an unrouted mic monitor, say) does not
/// refuse the fork and is not in it; a forkable one no output reaches is
/// not copied either. Once routed, the unforkable node refuses the fork.
///
/// Mutation (run): `ForkTarget::Master` forking every key in the spec (the
/// rule before) → `NotForkable { key: 2 }`.
#[test]
fn a_master_fork_holds_only_what_the_outputs_reach() {
    let (mut ed, _exec) = Editor::new(prepare(256));
    ed.insert(
        NodeKey(1),
        "ramp",
        tutti_graph::ForkByClone(Ramp { n: 0.0 }),
    );
    ed.insert(NodeKey(2), "mic", Legacy::new(sine_hz(440.0)).unforkable());
    ed.insert(
        NodeKey(3),
        "idle",
        tutti_graph::ForkByClone(Ramp { n: 0.0 }),
    );
    ed.spec_mut().topology.outputs = vec![out(NodeKey(1), 0)];
    let (fork, _fork_exec) = ed
        .fork(ForkTarget::Master, ForkMode::Live, prepare(256))
        .expect("the unrouted, unforkable node is not asked");
    let keys: Vec<NodeKey> = fork.spec().topology.nodes.keys().copied().collect();
    assert_eq!(keys, vec![NodeKey(1)]);

    ed.spec_mut().topology.outputs = vec![out(NodeKey(1), 0), out(NodeKey(2), 0)];
    assert_eq!(
        ed.fork(ForkTarget::Master, ForkMode::Live, prepare(256))
            .err(),
        Some(ForkError::NotForkable { key: NodeKey(2) })
    );
}
