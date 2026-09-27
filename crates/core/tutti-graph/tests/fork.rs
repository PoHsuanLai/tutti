//! `Editor::fork`: a copy of the graph (or of what feeds one node) that
//! shares no state with the live one — the replacement for fundsp's
//! `clone_isolated` → `isolate_for_offline` → `reset` (doc 013 Phase 3 PR 2).

mod common;

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use common::{bits, prepare, Kind, TestNode};
use tutti_graph::{
    param_parts, CrossfadeCurve, Cx, Editor, EventEdge, EventIn, EventOut, Fade, ForkByClone,
    ForkCause, ForkError, ForkFault, ForkFaultKind, ForkHealth, ForkMode, ForkSource, ForkTarget,
    Forked, GraphBuilder, IntoNode, Io, Node, NodeParts, ParamNode, ParamSet, Prepare, Renderer,
    Shape, Status, Unforkable,
};
use tutti_types::graph::{Edge, FeedbackFrom, InPort, OutPort, Source};
use tutti_types::{
    Amplitude, Beat, Bpm, ChannelLayout, NodeKey, OfflineClock, OfflineTransport, Param, Samples,
    Tail, Timeline, UnitParam,
};

/// Output `c` is `base + c`, so which port feeds a channel reads straight off
/// the render. Its `Clone` shares nothing, so it is inserted [`ForkByClone`].
#[derive(Clone)]
struct Consts {
    outs: usize,
    base: f32,
}

impl Node for Consts {
    fn shape(&self) -> Shape {
        Shape::audio(
            ChannelLayout::EMPTY,
            ChannelLayout::from_count(self.outs as u16),
        )
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        for c in 0..self.outs {
            io.output(c).fill(self.base + c as f32);
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// [`Consts`], forkable.
fn consts(outs: usize, base: f32) -> ForkByClone<Consts> {
    ForkByClone(Consts { outs, base })
}

/// A sine, its phase in a plain field (so a clone shares nothing): running
/// state a fork must not carry.
#[derive(Clone)]
struct Sine {
    hz: f32,
    phase: f32,
    dt: f32,
}

impl Sine {
    fn new(hz: f32) -> ForkByClone<Self> {
        ForkByClone(Self {
            hz,
            phase: 0.0,
            dt: 0.0,
        })
    }
}

impl Node for Sine {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO).with_tail(Tail::Unbounded)
    }
    fn prepare(&mut self, p: &Prepare) {
        self.dt = (1.0 / p.sample_rate().get()) as f32;
    }
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        for o in io.output(0) {
            *o = (self.phase * std::f32::consts::TAU).sin();
            self.phase = (self.phase + self.hz * self.dt).fract();
        }
        Status::Modified
    }
    fn reset(&mut self) {
        self.phase = 0.0;
    }
}

/// An offline context standing still at `beat`.
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

/// A ramp whose position lives in an `Arc` cell its clones **share** — the
/// shape of a node holding a live handle. Its fork source hands the fork a
/// cell of its own, from 0; the live node's cell is never the fork's.
struct Shared {
    pos: Arc<AtomicU32>,
}

impl Node for Shared {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO).with_tail(Tail::Unbounded)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        for x in io.output(0) {
            *x = self.pos.fetch_add(1, Ordering::Relaxed) as f32;
        }
        Status::Modified
    }
    fn reset(&mut self) {
        self.pos.store(0, Ordering::Relaxed);
    }
}

struct SharedFork;

impl ForkSource for SharedFork {
    fn fork(&self, _mode: ForkMode<'_>) -> Result<Forked, ForkCause> {
        Ok(Forked::new(Box::new(Shared {
            pos: Arc::new(AtomicU32::new(0)),
        })))
    }
}

impl IntoNode for Shared {
    type Controls = ();
    fn into_parts(self) -> NodeParts<()> {
        NodeParts {
            node: Box::new(self),
            controls: (),
            fork: Some(Box::new(SharedFork)),
        }
    }
}

/// Outputs its level, a `Param` a host sets by address through its
/// [`ParamSet`] — a [`ParamNode`], forked from the values last set.
#[derive(Clone)]
struct Level(Param<Amplitude>);

impl Level {
    fn new(level: f32) -> Self {
        Self(Param::new(Amplitude::new(level)))
    }
}

impl Node for Level {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill(self.0.load().get());
        Status::Modified
    }
    fn reset(&mut self) {}
}

impl ParamNode for Level {
    fn param_set(&self) -> ParamSet {
        ParamSet::builder()
            .param(UnitParam::Volume, self.0.as_atomic())
            .build()
    }
    fn fork_fresh(&self) -> Self {
        let mut f = self.clone();
        f.0.detach();
        f
    }
}

impl IntoNode for Level {
    type Controls = ParamSet;
    fn into_parts(self) -> NodeParts<ParamSet> {
        param_parts(self)
    }
}

/// A test node made forkable the way the engine's nodes are: an `IntoNode`
/// whose `into_parts` hands over a `ForkSource`. The fork is a fresh node of
/// the same kind.
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

/// A forkable ×2 gain.
fn gain() -> Forkable {
    Forkable(Kind::Gain {
        gain: 2.0,
        width: 1,
    })
}

fn out(node: NodeKey, port: u16) -> Source {
    Source::Node(OutPort { node, port })
}

fn render(ed: Editor, exec: tutti_graph::Executor, frames: usize) -> Vec<Vec<f32>> {
    Renderer::new(ed, exec).render(frames)
}

/// sine → mix → smoother, the smoother fed back into the mix's second input
/// (256 frames, so the fork may run 256-frame blocks), and the smoother
/// fanned out to both outputs.
fn chain() -> GraphBuilder {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let osc = g.add(Sine::new(440.0));
    let mix = g.add(Forkable(Kind::Sum { inputs: 2 }));
    let lp = g.add(Forkable(Kind::Smooth));
    g.connect(osc, 0, mix, 0)
        .feedback(lp, 0, mix, 1, Samples(256))
        .connect(mix, 0, lp, 0)
        .pipe_output(lp);
    g
}

/// **A forked chain renders exactly what a freshly built copy of the same
/// graph renders**, however long the live graph has run: nothing of its
/// running state — oscillator phase, filter memory, the feedback edge's
/// captured block — reaches the fork, and all of its wiring does. The fork is
/// prepared for its own `Prepare` (a larger block here), not the live one's.
///
/// Mutation: drop the `edges` copy in `Editor::fork` → the fork's filter
/// reads silence → fails. Mutation: drop `topology.outputs = outputs` →
/// every fork output is `Zero` → fails. Mutation: prepare the fork with the
/// live editor's `Prepare` → the `prepare()` assertion fails.
#[test]
fn a_forked_chain_renders_like_a_fresh_build() {
    let mut live = chain().renderer(prepare(64)).expect("builds");
    let before = live.render(1_000);
    assert!(before[0].iter().any(|&x| x != 0.0), "the live graph runs");

    let pre = prepare(256);
    let (fork_ed, fork_exec) = live
        .editor()
        .fork(ForkTarget::Master, ForkMode::Live, pre)
        .expect("every node is forkable");
    assert_eq!(fork_ed.prepare(), &pre);
    let forked = render(fork_ed, fork_exec, 3_000);

    let fresh = chain().renderer(pre).expect("builds").render(3_000);
    assert_eq!(bits(&forked), bits(&fresh));
    assert!(fresh[1].iter().any(|&x| x != 0.0), "not vacuous");
}

/// Forks as its fork source was asked to: a constant of the offline
/// render's beat, or 0 for a live duplicate. The one thing a source reads
/// from the [`ForkMode`].
struct ReadsMode;

struct ModeFork;

impl ForkSource for ModeFork {
    fn fork(&self, mode: ForkMode<'_>) -> Result<Forked, ForkCause> {
        let value = match mode {
            ForkMode::Offline(t) => t.beat().get() as f32,
            ForkMode::Live => 0.0,
        };
        Ok(Forked::new(Box::new(Consts {
            outs: 1,
            base: value,
        })))
    }
}

impl IntoNode for ReadsMode {
    type Controls = ();
    fn into_parts(self) -> NodeParts<()> {
        NodeParts {
            node: Box::new(Consts {
                outs: 1,
                base: 0.25,
            }),
            controls: (),
            fork: Some(Box::new(ModeFork)),
        }
    }
}

/// **An offline fork hands each fork source the render's timeline; a live
/// fork hands it none.** The live node keeps its own value throughout.
///
/// Mutation (run): `Editor::fork` handing every source `ForkMode::Live` →
/// the offline fork renders 0 → fails.
#[test]
fn an_offline_fork_hands_its_sources_the_render_timeline() {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
    let key = g.add(ReadsMode);
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

/// **A value set after insert is in the fork**: a [`ParamNode`]'s fork
/// reads its [`ParamSet`] when the fork is taken, so a `set` made after
/// insert — before any block has run — is what the fork renders. A value set
/// after the fork does not reach it: the fork has no link back.
///
/// Mutation (run): `ParamFork::new` keeping a template detached at insert
/// (`node.fork_fresh()`) and `fork_node` not applying the authored values →
/// the fork renders the constructed 0.5 → fails.
#[test]
fn a_value_set_after_insert_reaches_the_fork() {
    let (mut ed, mut exec) = Editor::new(prepare(64));
    let key = NodeKey(1);
    let controls = ed.insert(key, "level", Level::new(0.5));
    ed.spec_mut().topology.outputs = vec![out(key, 0)];
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();

    assert!(controls.set(UnitParam::Volume, 0.75));
    let (fork_ed, fork_exec) = ed
        .fork(ForkTarget::Master, ForkMode::Live, prepare(64))
        .expect("forks");
    assert!(controls.set(UnitParam::Volume, 0.9));
    let forked = render(fork_ed, fork_exec, 128);
    assert!(
        forked[0].iter().all(|&x| x == 0.75),
        "{:?}",
        &forked[0][..4]
    );
}

/// **A node without a fork source makes the fork refuse, naming it** —
/// before anything is forked. A node inserted `Unforkable`, a forkable node
/// boxed and inserted as an `Unforkable` `Box<dyn Node>`, and a forkable key
/// replaced by an unforkable unit are all unforkable; a target that does not exist or has
/// no outputs is refused too.
///
/// Mutation: skip keys without a source in `Editor::fork` → `Ok` → fails.
/// Mutation: keep the old fork source when `insert` replaces a unit with
/// one that has none → key 3 forks → fails.
#[test]
fn a_node_without_a_fork_source_is_not_forkable() {
    let (mut ed, _exec) = Editor::new(prepare(64));
    ed.insert(NodeKey(1), "gain", gain());
    ed.insert(
        NodeKey(2),
        "unforkable",
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
    ed.insert(NodeKey(1), "boxed", Unforkable(gain().into_node().0));
    ed.spec_mut().topology.outputs = vec![out(NodeKey(1), 0)];
    assert_eq!(
        ed.fork(ForkTarget::Master, ForkMode::Live, pre).err(),
        Some(ForkError::NotForkable { key: NodeKey(1) })
    );

    let (mut ed, _exec) = Editor::new(pre);
    ed.insert(NodeKey(3), "gain", gain());
    ed.spec_mut().topology.outputs = vec![out(NodeKey(3), 0)];
    assert!(ed.fork(ForkTarget::Master, ForkMode::Live, pre).is_ok());
    ed.insert(NodeKey(3), "boxed", Unforkable(gain().into_node().0));
    assert_eq!(
        ed.fork(ForkTarget::Master, ForkMode::Live, pre).err(),
        Some(ForkError::NotForkable { key: NodeKey(3) })
    );
    assert_eq!(
        ed.fork(ForkTarget::Node(NodeKey(9)), ForkMode::Live, pre)
            .err(),
        Some(ForkError::NoSuchNode { key: NodeKey(9) })
    );
    ed.insert(NodeKey(4), "sink", consts(0, 0.0));
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
    ed.insert(NodeKey(1), "gain", gain());
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
    ed.insert(NodeKey(1), "gain", gain());
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

/// **A node fork's outputs follow fundsp's `Net::clone_isolated`**, the
/// rule it replaced: channel `c` reads the node's port `min(c, outs - 1)` —
/// a mono node on every channel, and a wider graph *clamped* to the node's
/// last port (stereo into six is L R R R R R), not wrapped as `pipe_output`
/// wraps. Walked over a grid of node and graph widths; since port `p` of
/// [`Consts`] carries `base + p`, channel `c` must carry
/// `base + min(c, outs - 1)` on every frame. (Until doc 013 Phase 5 this was
/// checked against a `Net`'s render; `Net` is gone, so the rule is the
/// figure.)
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
            let want: Vec<Vec<f32>> = (0..graph_outs)
                .map(|c| vec![10.0 + c.min(node_outs - 1) as f32; 64])
                .collect();

            let (mut ed, _exec) = Editor::new(prepare(64));
            let key = NodeKey(7);
            ed.insert(key, "consts", ForkByClone(unit));
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
    ed.insert(osc, "osc", Sine::new(220.0));
    ed.insert(a, "mix", Forkable(Kind::Sum { inputs: 2 }));
    ed.insert(fb, "fb", consts(1, 0.25));
    ed.insert(
        emit,
        "emit",
        Forkable(Kind::Emitter {
            period: 50,
            phase: 7,
        }),
    );
    ed.insert(
        target,
        "target",
        Forkable(Kind::Mixed {
            width: 1,
            events_in: 1,
            events_out: 0,
        }),
    );
    ed.insert(after, "after", gain());
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
/// was, while the fork renders concurrently. The graph holds a node whose
/// clones share a cell with it (`Shared`), which its fork source severs, and
/// a [`ParamNode`] whose fork is taken from its [`ParamSet`].
///
/// Mutation (run): `ParamFork::fork_node` writing the authored values
/// through the live set instead of the fork's → the live level is written
/// back to its authored value, which a live-only write (a modulation
/// driver's) had moved → the live output leaves the twin's → fails.
/// Mutation (run): `SharedFork` handing the fork the live node's cell → the
/// fork's rendering advances the live ramp → fails.
#[test]
fn the_live_graph_is_unaffected_while_a_fork_renders() {
    fn build() -> (Editor, tutti_graph::Executor, ParamSet, Param<Amplitude>) {
        let (mut ed, mut exec) = Editor::new(prepare(64));
        let level = Level::new(0.5);
        let cell = level.0.clone();
        ed.insert(
            NodeKey(1),
            "ramp",
            Shared {
                pos: Arc::new(AtomicU32::new(0)),
            },
        );
        let controls = ed.insert(NodeKey(2), "level", level);
        ed.spec_mut().topology.outputs = vec![out(NodeKey(1), 0), out(NodeKey(2), 0)];
        ed.commit().expect("commits");
        exec.apply_pending();
        ed.collect();
        (ed, exec, controls, cell)
    }
    let (ed, exec, controls, cell) = build();
    let (twin_ed, twin_exec, twin_controls, twin_cell) = build();
    let mut live = Renderer::new(ed, exec);
    let mut twin = Renderer::new(twin_ed, twin_exec);
    assert!(controls.set(UnitParam::Volume, 0.75));
    assert!(twin_controls.set(UnitParam::Volume, 0.75));
    // A live-only move (what a modulation driver writes): not authored.
    cell.store(Amplitude::new(0.6));
    twin_cell.store(Amplitude::new(0.6));
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
    assert!(
        forked[1].iter().all(|&x| x == 0.75),
        "the fork renders the authored level"
    );
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
    ed.insert(NodeKey(1), "consts", consts(1, 0.5));
    ed.spec_mut().topology.outputs = vec![out(NodeKey(1), 0)];
    assert!(ed.fork(ForkTarget::Master, ForkMode::Live, pre).is_ok());
    ed.spec_mut().generations.insert(NodeKey(1), 7);
    assert_eq!(
        ed.fork(ForkTarget::Master, ForkMode::Live, pre).err(),
        Some(ForkError::NotForkable { key: NodeKey(1) })
    );
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
        Forkable(Kind::Emitter {
            period: 1,
            phase: 0,
        })
    };
    ed.insert(emit, "emit", emitter());
    ed.insert(fold, "fold", Forkable(Kind::Consumer { inputs: 1 }));
    ed.insert(emit2, "emit", emitter());
    ed.insert(fold2, "fold", Forkable(Kind::Consumer { inputs: 1 }));
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
    ed.insert(NodeKey(1), "consts", consts(1, 0.5));
    assert_eq!(
        ed.fork(ForkTarget::Node(NodeKey(1)), ForkMode::Live, prepare(64))
            .err(),
        Some(ForkError::NoOutputs { key: NodeKey(1) })
    );
}

/// A ramp: its output is the frame count since its last `reset`
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

/// **A node inserted as `ForkByClone` forks, from reset; the same
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
    ed.insert(NodeKey(2), "mic", Unforkable(Ramp { n: 0.0 }));
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
