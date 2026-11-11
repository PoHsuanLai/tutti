//! What a session bounce renders, and what it forks: no Bevy, no pool, a
//! [`Session`] and its [`BounceJob`](tutti_session::BounceJob) driven by hand.
//!
//! An export forks the live graph (`Editor::fork`). The exports are held to
//! the same units wired fresh with `GraphBuilder` (a fork starts reset, which
//! a fresh graph is), to the graph's own latency and tail, to a prepare hook's
//! edit, and to golden digests (`tutti_fixtures::assert_golden`). The
//! rest pin what only a fork has: it shares nothing with the live graph, it
//! names a node it cannot copy, it is gathered when the export starts and
//! built wherever the job runs, and everything it held is dropped on the
//! main thread.
//!
//! These suites held the same properties through `bevy-tutti`'s export
//! systems before that crate was parked; the instruments, the disk voice and
//! the hosted plugin are `export_fork_synth.rs`, `export_fork_sampler.rs` and
//! `export_fork_plugin.rs`.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;
use std::time::Duration;

use tutti_export::{ChannelLayout, Produced};
use tutti_fixtures::assert_golden;
use tutti_graph::NodeKey;
use tutti_graph::{
    Cx, ForkBuild, ForkByClone, ForkCause, ForkMode, ForkSource, Forked, IntoNode, Io, Node,
    NodeParts, ParamNode, ParamSet, Prepare, Shape, Status, param_parts,
};
use tutti_nodes::testing::{Const, Osc, Sink};
use tutti_nodes::{SvfFilterNode, SvfType};
use tutti_session::{
    Bounce, BounceConfigError, BounceError, ExportClock, GraphSource, Latency, PreparedGraph,
    Range, Session,
};
use tutti_types::{Amplitude, Hz, Param, Q, SampleRate, Samples, Seconds, Tail, UnitParam};

const RATE: f64 = 48_000.0;

/// A detached stereo session whose graph is at the export's rate.
fn session() -> Session {
    session_with_outputs(2)
}

fn session_with_outputs(outputs: usize) -> Session {
    let mut session = Session::builder()
        .inputs(0)
        .outputs_floor(outputs)
        .build_detached(SampleRate::DEFAULT)
        .expect("a detached session builds");
    session.graph().set_sample_rate(SampleRate(RATE));
    session
}

/// Which audio a bounce renders: the mix, or one node alone.
#[derive(Debug, Clone, Copy)]
enum ExportSource {
    Mix,
    Node(NodeKey),
}

/// A bounce of `source` into memory for `seconds`: stereo at `rate`, on a frozen
/// clock, trimming nothing and keeping no tail (what the figures it does not
/// name come to is each test's to ask for).
fn bounce_at(source: ExportSource, seconds: f64, rate: f64) -> Bounce {
    let bounce = match source {
        ExportSource::Mix => Bounce::project(),
        ExportSource::Node(node) => Bounce::node(node),
    };
    bounce
        .master_buffers()
        .range(Range::Seconds(Seconds(0.0), Seconds(seconds)))
        .sample_rate(SampleRate(rate))
        .channels(ChannelLayout::STEREO)
        .clock(ExportClock::frozen())
        .latency(Latency::Keep)
        .tail(tutti_session::Tail::None)
}

fn buffers(source: ExportSource, seconds: f64) -> Bounce {
    bounce_at(source, seconds, RATE)
}

/// The planes `bounce` renders, run on this thread.
fn render(session: &mut Session, bounce: Bounce) -> Vec<Vec<f32>> {
    match session
        .bounce(bounce)
        .expect("the bounce renders")
        .finished()
        .map(|b| b.into_master())
    {
        Some(Produced::Buffers(rendered)) => rendered.into_planes(),
        other => panic!("expected rendered buffers, got {other:?}"),
    }
}

/// What the session refuses `bounce` with, started and run on this thread.
fn refused(session: &mut Session, bounce: Bounce) -> BounceError {
    match session.bounce(bounce) {
        Err(e) => e,
        Ok(out) => panic!(
            "expected a refusal, the bounce reported {:?}",
            out.finished()
        ),
    }
}

// ---------------------------------------------------------------------------
// What any export renders
// ---------------------------------------------------------------------------

/// A saw through a low-pass on channel 0, the raw saw on channel 1: a master
/// that differs from a node export of the filter on every channel, so the
/// two cannot pass for each other. Returns the filter's key.
fn chain(session: &mut Session) -> NodeKey {
    let graph = session.graph();
    let (osc, _) = graph.insert(Osc::saw(Hz(110.0)));
    let (filter, _params) = graph.insert(SvfFilterNode::<f64>::new(
        SvfType::LowPass,
        Hz(800.0),
        Q(1.0),
    ));
    graph.set_source(filter, 0, GraphSource::Node(osc, 0));
    graph.set_output_source(0, GraphSource::Node(filter, 0));
    graph.set_output_source(1, GraphSource::Node(osc, 0));
    filter
}

/// FNV-1a over the planes' little-endian `f32` bits, plane after plane.
fn digest(planes: &[Vec<f32>]) -> u64 {
    let mut h = tutti_fixtures::Fnv::new();
    for plane in planes {
        h.f32s(plane);
    }
    h.finish()
}

/// [`chain`]'s units wired fresh, with `GraphBuilder`, and rendered in the
/// export's own blocks for `seconds`: the master (the filter on channel 0,
/// the saw on channel 1) and the filter alone on both channels (a mono
/// node clamped across a stereo root). A fork starts reset, which is what a
/// fresh graph is.
fn chain_fresh(seconds: f64) -> (Vec<Vec<f32>>, Vec<Vec<f32>>) {
    use tutti_export::{
        BufferTarget, Control, FrozenClock, Job, Output, Produced, RenderGraph, Setup, render_one,
    };
    use tutti_graph::GraphBuilder;
    let render = |master: bool| {
        let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
        let osc = g.add(Osc::saw(Hz(110.0)));
        let (filter, _params) = g.add_with_controls(SvfFilterNode::<f64>::new(
            SvfType::LowPass,
            Hz(800.0),
            Q(1.0),
        ));
        g.connect(osc, 0, filter, 0).connect_output(filter, 0, 0);
        g.connect_output(if master { osc } else { filter }, 0, 1);
        let (editor, executor) = g
            .build(RenderGraph::prepare(SampleRate(RATE)))
            .expect("builds");
        let mut graph =
            RenderGraph::assemble(editor, executor, Setup::new()).expect("built together");
        let rate = SampleRate(RATE);
        render_one(
            &mut graph,
            &Job::new(
                rate,
                Seconds(seconds).to_samples(rate),
                Samples(0),
                Samples(0),
            )
            .expect("a render"),
            &FrozenClock,
            Output::master(BufferTarget::new(ChannelLayout::STEREO)),
            &mut Control::none(),
        )
        .expect("renders")
        .finished()
        .and_then(Produced::buffers)
        .expect("nothing cancels a control with no token")
        .into_planes()
    };
    (render(true), render(false))
}

/// **An export renders its graph as wired fresh**, for the master and for
/// one node, on a graph whose live side has not run: bit for bit the same
/// units in a `GraphBuilder` graph ([`chain_fresh`]), and the golden digest.
///
/// A fork starts reset rather than copying the running state (an
/// oscillator's phase, a filter's memory); with the live side never rendered
/// that makes no difference, so the comparison is like for like.
///
/// The graph, the export and the digest are the ones the `bevy-tutti` test
/// of this name rendered: its export systems added nothing between the
/// session's `export` and the job's render.
///
/// Mutation (run, failed): the session's `Graph::gather_for_export` forking
/// `ForkTarget::Master` for a node export → the node's render is the master's,
/// and the node comparison fails on channel 1 (the saw, not the filter).
#[test]
fn exports_render_their_graph_as_wired_fresh() {
    let mut session = session();
    let filter = chain(&mut session);
    let forked_master = render(&mut session, buffers(ExportSource::Mix, 0.25));
    let forked_node = render(&mut session, buffers(ExportSource::Node(filter), 0.25));
    let (fresh_master, fresh_node) = chain_fresh(0.25);
    for (what, fresh, forked) in [
        ("master", &fresh_master, &forked_master),
        ("node", &fresh_node, &forked_node),
    ] {
        assert_eq!(fresh.len(), forked.len(), "{what}: width");
        let peak = forked[0].iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak > 0.1, "{what}: silent");
        for (c, (a, b)) in fresh.iter().zip(forked).enumerate() {
            assert_eq!(a.len(), b.len(), "{what}: length of channel {c}");
            if let Some(i) = a
                .iter()
                .zip(b)
                .position(|(x, y)| x.to_bits() != y.to_bits())
            {
                panic!(
                    "{what}: channel {c} parts at frame {i}: fresh {} export {}",
                    a[i], b[i]
                );
            }
        }
    }
    assert_golden(
        "the master export",
        digest(&forked_master),
        0xccbc_b417_89a0_36e4,
    );
    // And the node export is the node: both channels are the filter (a mono
    // node clamps across a stereo root), which is the master's channel 0.
    assert_eq!(forked_node[0], forked_master[0]);
    assert_eq!(forked_node[1], forked_master[0]);
    assert_ne!(forked_master[1], forked_master[0]);
}

/// A source that steps from 0 to 1 at frame `LATE` and declares `LATE`
/// frames of latency, with a finite tail of `TAIL` frames: a look-ahead
/// processor reduced to its bookkeeping.
#[derive(Clone)]
struct Late {
    pos: usize,
}

const LATE: usize = 100;
const TAIL: usize = 300;

impl Node for Late {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
            .with_latency(tutti_types::Latency::new(Samples(LATE)))
            .with_tail(Tail::Finite(Samples(TAIL)))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        for y in io.output(0) {
            *y = if self.pos >= LATE { 1.0 } else { 0.0 };
            self.pos += 1;
        }
        Status::Modified
    }
    fn reset(&mut self) {
        self.pos = 0;
    }
}

/// **`trim_reported_latency` and `with_reported_tail` take the figures from
/// the graph that is rendered**: the step a `LATE`-frame source makes at its
/// declared latency lands on frame 0, and the render runs `TAIL` frames
/// past its duration. Asked of the forked plan.
///
/// The tail is resolved by `GraphTail::resolve`: a finite tail under the cap
/// is rendered whole, and a graph whose only node never said (`Tail::Unknown`)
/// renders none — not the cap, which would
/// append silence.
///
/// Mutations (each run, each failed): `ExportJob::run` ignoring
/// `latency_from_graph` → frame 0 reads 0; ignoring `tail_from_graph` → the
/// render is `TAIL` frames short; resolving the tail as
/// `samples().unwrap_or(cap)` → the unknown graph renders `cap` extra frames.
#[test]
fn latency_and_tail_come_from_the_graph() {
    let mut session = session();
    let (late, _) = session.graph().insert(ForkByClone(Late { pos: 0 }));
    session.graph().set_outputs_from(late);
    let seconds = 0.01;
    let frames = (seconds * RATE) as usize;
    let planes = render(
        &mut session,
        buffers(ExportSource::Mix, seconds)
            .latency(Latency::Graph)
            .tail(tutti_session::Tail::Graph {
                cap: Samples(48_000).to_seconds(SampleRate(RATE)),
            }),
    );
    assert_eq!(planes[0].len(), frames + TAIL, "the tail");
    assert_eq!(planes[0][0], 1.0, "the latency was not trimmed");
    assert!(planes[0].iter().all(|&s| s == 1.0), "a step from frame 0");

    let mut session = self::session();
    let (unknown, _) = session.graph().insert(Counter {
        n: Arc::new(AtomicU64::new(0)),
    });
    session.graph().set_outputs_from(unknown);
    let planes = render(
        &mut session,
        buffers(ExportSource::Mix, seconds).tail(tutti_session::Tail::Graph {
            cap: Samples(4_800).to_seconds(SampleRate(RATE)),
        }),
    );
    assert_eq!(
        planes[0].len(),
        frames,
        "an unknown tail renders none, not the cap"
    );
}

/// **The latency trimmed is the latency of the graph rendered — after the
/// `prepare` hook.** The live graph is a latency-free constant; the hook
/// puts a `LATE`-frame source on the output. Trimmed by what the hook left,
/// the source's step lands on frame 0; by the live graph's figure (0), it
/// would land on frame `LATE`.
///
/// Mutations (each run, each failed): `ExportJob::run` reading
/// `reported_latency()` before the edit runs and using that figure; and the
/// fork's commit after the edit skipped (`ForkedPair::commit` replaced by `Ok`),
/// so the plan is still the live graph's and nothing is trimmed.
#[test]
fn the_trim_is_read_after_the_prepare_hook() {
    let mut session = session();
    let (dc, _) = session.graph().insert(Const::mono(0.5));
    session.graph().set_outputs_from(dc);
    let request = buffers(ExportSource::Mix, 0.01)
        .latency(Latency::Graph)
        .edit(Box::new(|prepared: PreparedGraph<'_>| {
            let key = prepared.fresh_key();
            let editor = prepared.editor;
            editor.insert(key, "test:late", ForkByClone(Late { pos: 0 }));
            for out in editor.spec_mut().topology.outputs.iter_mut() {
                *out = vec![tutti_graph::Source::Node(tutti_graph::OutPort {
                    node: key,
                    port: 0,
                })];
            }
        }));
    let planes = render(&mut session, request);
    assert_eq!(
        planes[0][0], 1.0,
        "trimmed by the live graph's latency, not the rendered one's"
    );
}

/// **A prepare hook that swaps the fork's editor for another graph's fails the
/// export; it is not rendered**: the editor would never see what the fork's
/// executor retires. The hook holds `&mut Editor`, so `mem::replace` is
/// writable; the job checks the pair before it seals it.
#[test]
fn a_prepare_hook_that_swaps_the_editor_fails_the_export() {
    let mut session = session();
    let (dc, _) = session.graph().insert(Const::mono(0.5));
    session.graph().set_outputs_from(dc);
    let request = buffers(ExportSource::Mix, 0.01).edit(Box::new(|prepared: PreparedGraph<'_>| {
        let (other, _executor) = tutti_graph::Editor::new(Prepare::new(
            SampleRate(RATE),
            tutti_export::GRAPH_MAX_BLOCK,
        ));
        drop(std::mem::replace(prepared.editor, other));
    }));
    match refused(&mut session, request) {
        BounceError::Config(BounceConfigError::PrepareHookUnpaired) => {}
        other => panic!("expected the unpaired refusal, got {other:?}"),
    }
    assert!(!session.bounce_in_flight(), "the token is released");
}

/// **An export of a graph with no outputs says so**, for the master and for
/// a node — not that the node has none.
///
/// Mutation (run, failed): the `outputs() == 0` check in `Session::export`
/// disabled → neither export is refused as having no outputs.
#[test]
fn a_graph_with_no_outputs_says_so() {
    let mut session = session_with_outputs(0);
    let (node, _) = session.graph().insert(Osc::sine(Hz(220.0)));
    for source in [ExportSource::Mix, ExportSource::Node(node)] {
        match refused(&mut session, buffers(source, 0.01)) {
            BounceError::Config(BounceConfigError::GraphHasNoOutputs) => {}
            other => panic!("{source:?}: expected a refusal naming the outputs, got {other:?}"),
        }
    }
}

/// **A node with no outputs of its own cannot be exported**: a refusal, not
/// a silent drop or a panic on the render thread. "No outputs" means the
/// *node* produces none (a genuine sink); one that is merely unwired still
/// has an output and renders.
///
/// Mutation (run, failed): the `NoOutputs` arm of `Session::export` (which
/// maps the fork's `NoOutputs` to `NodeHasNoOutputs`) removed → the refusal
/// is `Error::Fork(..)`, not `NodeHasNoOutputs`.
#[test]
fn an_unrenderable_node_is_refused() {
    let mut session = session();
    let (wired, _) = session.graph().insert(Const::mono(0.5));
    session.graph().set_outputs_from(wired);
    // `Sink::mono()` consumes one channel and produces nothing.
    let (orphan, _) = session.graph().insert(Sink::mono());
    match refused(&mut session, buffers(ExportSource::Node(orphan), 0.01)) {
        BounceError::Config(BounceConfigError::NodeHasNoOutputs) => {}
        other => panic!("expected NodeHasNoOutputs, got {other:?}"),
    }
    // An unwired source still renders: it has an output.
    let (loose, _) = session.graph().insert(Const::mono(0.25));
    let planes = render(&mut session, buffers(ExportSource::Node(loose), 0.01));
    assert!(
        planes[0].iter().all(|&s| s == 0.25),
        "an unwired node renders"
    );
}

// ---------------------------------------------------------------------------
// What a prepare hook sees and reaches
// ---------------------------------------------------------------------------

/// A one-second-free session with a constant 0.5 on the outputs: a graph a
/// hook's edit is told apart from by its level.
fn graph_with_one_node_on() -> (Session, NodeKey) {
    let mut session = session();
    let (node, _) = session.graph().insert(Const::mono(0.5));
    session.graph().set_outputs_from(node);
    (session, node)
}

fn short() -> Bounce {
    bounce_at(ExportSource::Mix, 0.05, 44_100.0)
}

/// The `prepare` hook runs on the graph that is actually rendered, before
/// the render.
///
/// Pinned by *audio*, not by a call counter: a hook that runs against some
/// other graph, or after the job was built, would leave the rendered signal
/// at the graph's own 0.5 rather than the 0.25 this one writes. The hook
/// edits the fork's editor and does not commit: the session does.
///
/// Mutation (run, failed): the session's commit of the fork after the hook
/// (`ExportJob::run`, `pair.commit()` after the edit) skipped → renders 0.5.
#[test]
fn a_prepare_hook_reaches_the_graph_that_gets_rendered() {
    let (mut session, _node) = graph_with_one_node_on();
    let level = 0.25f32;
    let request = short().edit(Box::new(move |prepared: PreparedGraph<'_>| {
        // Replace the whole graph's output with a constant.
        let key = prepared.fresh_key();
        let editor = prepared.editor;
        editor.insert(key, "test:level", Const::mono(level));
        for out in editor.spec_mut().topology.outputs.iter_mut() {
            *out = vec![tutti_graph::Source::Node(tutti_graph::OutPort {
                node: key,
                port: 0,
            })];
        }
    }));
    let planes = render(&mut session, request);
    let peak = planes[0].iter().fold(0.0f32, |acc, s| acc.max(s.abs()));
    assert_eq!(
        (peak * 1000.0).round() as usize,
        250,
        "the edit must run on the graph that is rendered — an unmodified graph \
         would come back at the 0.5 it was built with"
    );
}

/// **The `prepare` hook is handed the caller's timeline**, for a node export.
///
/// `RenderClock` is advance-only, so the session cannot read a timeline back
/// out of the `clock` a caller supplies. Handing the hook a timeline of its
/// own (say 120 BPM at beat 0) while the renderer advanced the caller's would
/// make the two disagree: a node the hook places on a 90 BPM project would
/// land on a 120 BPM playhead that nothing advances.
///
/// Mutation (run, failed): `Session::export` handing the hook a context of
/// its own (`ExportClock::frozen().offline()`) instead of the request's
/// → the hook sees 120 BPM at beat 0, not 90 at beat 16.
#[test]
fn the_prepare_hook_sees_the_callers_timeline() {
    use tutti_transport::{OfflineTimeline, OfflineTimelineConfig};

    let (mut session, node) = graph_with_one_node_on();
    // A deliberately un-default transport: neither 120 BPM nor beat 0.
    let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        start_beat: tutti_types::Beat(16.0),
        tempo_map: Arc::new(
            tutti_tempo::TempoMap::constant(tutti_types::Bpm::of(90.0)).expect("a tempo"),
        ),
        sample_rate: SampleRate(44_100.0),
        loop_range: None,
    }));
    let seen: Arc<Mutex<Option<(usize, usize)>>> = Arc::default();
    let into = Arc::clone(&seen);
    let request = bounce_at(ExportSource::Node(node), 0.05, 44_100.0)
        .clock(ExportClock::timeline(timeline))
        .edit(Box::new(move |prepared: PreparedGraph<'_>| {
            let transport = prepared.ctx;
            *into.lock().unwrap() = Some((
                transport
                    .tempo_map()
                    .tempo_at(transport.beat())
                    .get()
                    .round() as usize,
                transport.beat().get().round() as usize,
            ));
        }));
    render(&mut session, request);
    assert_eq!(
        *seen.lock().unwrap(),
        Some((90, 16)),
        "the hook must see the caller's timeline — 90 BPM at beat 16 — not a manufactured 120 BPM one at beat 0"
    );
}

/// **A master export's `prepare` hook sees the caller's timeline too.**
///
/// Mutation (run, failed): `Session::export` handing the hook a context of
/// its own (`ExportClock::frozen().offline()`) instead of the request's →
/// the hook sees 120 BPM, not 90.
#[test]
fn a_master_exports_prepare_hook_sees_the_callers_timeline() {
    use tutti_transport::{OfflineTimeline, OfflineTimelineConfig};

    let (mut session, _node) = graph_with_one_node_on();
    let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        start_beat: tutti_types::Beat(4.0),
        tempo_map: Arc::new(
            tutti_tempo::TempoMap::constant(tutti_types::Bpm::of(90.0)).expect("a tempo"),
        ),
        sample_rate: SampleRate(44_100.0),
        loop_range: None,
    }));
    let seen: Arc<Mutex<Option<usize>>> = Arc::default();
    let into = Arc::clone(&seen);
    let request = short()
        .clock(ExportClock::timeline(timeline))
        .edit(Box::new(move |prepared: PreparedGraph<'_>| {
            *into.lock().unwrap() = Some(
                prepared
                    .ctx
                    .tempo_map()
                    .tempo_at(prepared.ctx.beat())
                    .get()
                    .round() as usize,
            );
        }));
    render(&mut session, request);
    assert_eq!(*seen.lock().unwrap(), Some(90), "the caller's 90 BPM");
}

// ---------------------------------------------------------------------------
// What a fork has and a plain clone would not
// ---------------------------------------------------------------------------

/// A source that counts frames in a cell its clones **share**, and whose
/// fork source gives a fork a fresh copy of: a node whose live state a copy
/// could move. Its output is the count, so a render that touched the live
/// cell shows as a jump in the live signal. Its tail is `Tail::Unknown`: it
/// never says.
#[derive(Clone)]
struct Counter {
    n: Arc<AtomicU64>,
}

/// [`Counter`]'s fork: a cell of its own, reset (a fork is reset).
struct CounterFork;

impl ForkSource for CounterFork {
    fn fork(&self, _mode: ForkMode) -> Result<Forked, ForkCause> {
        let n = Arc::new(AtomicU64::new(0));
        Ok(Forked::new(Box::new(Counter { n })))
    }
}

impl IntoNode for Counter {
    type Controls = ();
    fn into_parts(self) -> NodeParts<()> {
        NodeParts {
            node: Box::new(self),
            controls: (),
            fork: Some(Box::new(CounterFork)),
        }
    }
}

impl Node for Counter {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO).with_tail(Tail::Unknown)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        for y in io.output(0) {
            *y = self.n.fetch_add(1, Ordering::Relaxed) as f32;
        }
        Status::Modified
    }
    fn reset(&mut self) {
        self.n.store(0, Ordering::Relaxed);
    }
}

/// **Live playback is untouched by an export rendering beside it.** The live
/// graph's audio side renders on its own thread, block after block, from
/// before the export starts until after it reports; the export renders a
/// fork of the same graph meanwhile. The live signal counts on without a
/// jump, and the export counts from 0 (a fork is reset) — the two never
/// shared the counter.
///
/// A plain clone would share the counter with the live graph, and fail this
/// test by design.
///
/// Mutation (run, failed): `Counter`'s `into_parts` made a plain clone
/// (`ForkByClone(self).into_parts()`) instead of [`CounterFork`] → the fork
/// shares the live cell, and its reset and render move the live count, which
/// jumps.
#[test]
fn live_playback_continues_unaffected_while_an_export_renders() {
    let mut session = session();
    let (node, _) = session.graph().insert(Counter {
        n: Arc::new(AtomicU64::new(0)),
    });
    session.graph().set_outputs_from(node);
    // Commit on this side, then hand the audio side to the "device".
    session.render_frame(&mut [0.0, 0.0]);
    let mut live = session.graph().take_audio_side();

    let done = AtomicBool::new(false);
    let (heard, rendered) = std::thread::scope(|s| {
        let device = s.spawn(|| {
            let mut heard = Vec::new();
            let mut block = vec![Vec::new(), Vec::new()];
            // At least a few blocks after the export reports, so the two
            // overlap from end to end.
            let mut after = 0;
            while after < 8 {
                live.render(256, 256, &mut block);
                heard.extend_from_slice(&block[0]);
                if done.load(Ordering::Acquire) {
                    after += 1;
                }
            }
            heard
        });
        let rendered = render(&mut session, buffers(ExportSource::Mix, 1.0));
        done.store(true, Ordering::Release);
        (device.join().expect("the live side"), rendered)
    });

    for (i, &s) in rendered[0].iter().enumerate() {
        assert_eq!(s, i as f32, "the export's count at frame {i}");
    }
    let first = heard[0];
    for (i, &s) in heard.iter().enumerate() {
        assert_eq!(
            s,
            first + i as f32,
            "the live count jumped at frame {i} while the export ran"
        );
    }
}

/// A source no fork may take, as a microphone monitor is inserted
/// (`tutti_graph::Unforkable`): its clones would share the live ring.
struct Mic;

impl Node for Mic {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill(0.5);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// **A master export forks what the outputs hear**: an unforkable node no
/// output reaches (an unrouted mic) does not refuse it, and the render is
/// the routed node's.
///
/// Mutation (run, failed): `ForkTarget::Master` forking every node of the
/// editor (`upstream(shapes().keys())`) → refused as not forkable, naming the
/// mic.
#[test]
fn an_unrouted_unforkable_node_does_not_refuse_a_master_export() {
    let mut session = session();
    let (_mic, _) = session.graph().insert(tutti_graph::Unforkable(Mic));
    let (dc, _) = session.graph().insert(Const::mono(0.25));
    session.graph().set_outputs_from(dc);
    let planes = render(&mut session, buffers(ExportSource::Mix, 0.01));
    assert!(planes[0].iter().all(|&s| s == 0.25));
}

/// **A node that cannot be forked refuses the export by its key**, which a
/// host maps back to whatever it bound to the node; and a node export of a
/// branch it does not feed is not refused (a fork copies only the sub-graph
/// feeding the node).
///
/// Mutation (run, failed): `tutti_export`'s `fork_error` wrapping
/// `NotForkable` in `Error::Fork` → the refusal is not `NotForkable { key }`.
#[test]
fn an_unforkable_node_refuses_the_export_by_key() {
    let mut session = session();
    let (mic, _) = session.graph().insert(tutti_graph::Unforkable(Mic));
    let (osc, _) = session.graph().insert(Osc::sine(Hz(220.0)));
    session
        .graph()
        .set_output_source(0, GraphSource::Node(mic, 0));
    session
        .graph()
        .set_output_source(1, GraphSource::Node(osc, 0));

    match refused(&mut session, buffers(ExportSource::Mix, 0.05)) {
        BounceError::NotForkable { node: key } => {
            assert_eq!(key, mic);
            assert!(
                tutti_export::Error::NotForkable { key }
                    .to_string()
                    .contains(&format!("{mic:?}")),
                "the message names it"
            );
        }
        other => panic!("expected a named refusal, got {other:?}"),
    }
    let planes = render(&mut session, buffers(ExportSource::Node(osc), 0.05));
    assert!(planes[0].iter().any(|&s| s != 0.0), "the sibling renders");
}

// ---------------------------------------------------------------------------
// The fork is gathered when the export starts and built where the job runs
// ---------------------------------------------------------------------------

/// A constant the fork renders: a value the gather read.
#[derive(Clone)]
struct Held(f32);

impl Node for Held {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill(self.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// A live node whose level is a cell the host moves (`f32` bits). Its
/// fork is slow, as a hosted plugin's `plugin-server` launch is: the
/// gather reads the cell, and the deferred build waits on `wait` (at
/// most three seconds) before it makes a [`Held`] of that level.
struct Slow {
    level: Arc<AtomicU32>,
    wait: Mutex<Option<Receiver<()>>>,
    built: Arc<AtomicBool>,
}

impl Node for Slow {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0)
            .fill(f32::from_bits(self.level.load(Ordering::SeqCst)));
        Status::Modified
    }
    fn reset(&mut self) {}
}

struct SlowFork {
    level: Arc<AtomicU32>,
    wait: Mutex<Option<Receiver<()>>>,
    built: Arc<AtomicBool>,
}

impl ForkSource for SlowFork {
    fn fork(&self, _mode: ForkMode) -> Result<Forked, ForkCause> {
        unreachable!("an export gathers")
    }

    fn gather(&self, _mode: ForkMode) -> Result<ForkBuild, ForkCause> {
        // Read now, when the export starts.
        let level = f32::from_bits(self.level.load(Ordering::SeqCst));
        let wait = self.wait.lock().unwrap().take();
        let built = Arc::clone(&self.built);
        Ok(ForkBuild::deferred(move || {
            if let Some(wait) = wait {
                let _ = wait.recv_timeout(Duration::from_secs(3));
            }
            built.store(true, Ordering::SeqCst);
            Ok(Forked::new(Box::new(Held(level))))
        }))
    }
}

impl IntoNode for Slow {
    type Controls = ();
    fn into_parts(self) -> NodeParts<()> {
        let fork = SlowFork {
            level: Arc::clone(&self.level),
            wait: Mutex::new(self.wait.lock().unwrap().take()),
            built: Arc::clone(&self.built),
        };
        NodeParts {
            node: Box::new(self),
            controls: (),
            fork: Some(Box::new(fork)),
        }
    }
}

/// A slow node at `level`, its build released by the returned sender.
fn slow(level: f32) -> (Slow, Arc<AtomicU32>, Sender<()>, Arc<AtomicBool>) {
    let cell = Arc::new(AtomicU32::new(level.to_bits()));
    let built = Arc::new(AtomicBool::new(false));
    let (release, wait) = channel();
    let node = Slow {
        level: Arc::clone(&cell),
        wait: Mutex::new(Some(wait)),
        built: Arc::clone(&built),
    };
    (node, cell, release, built)
}

/// Outputs its volume, a param the host sets by address: a
/// `ParamNode`, whose fork reads the value last set at the gather.
#[derive(Clone)]
struct Level(Param<Amplitude>);

impl Node for Level {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
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
        let mut fork = self.clone();
        fork.0.detach();
        fork
    }
}

impl IntoNode for Level {
    type Controls = ParamSet;
    fn into_parts(self) -> NodeParts<ParamSet> {
        param_parts(self)
    }
}

/// **An export does not wait for a slow fork, and is the graph as it was
/// when it started.** A node whose fork build blocks until released (the
/// shape of a hosted plugin's `plugin-server` launch) is exported beside
/// a `ParamNode`. `Session::export` returns with the build not yet run (it
/// gathers; the job builds); there the live graph is edited — the slow
/// node's cell and the param moved, the outputs rewired to another node —
/// and only then is the build released and the job run on a thread of its
/// own. The export renders the graph at the start: the slow node's level and
/// the param's value then, on the outputs as they were.
///
/// Mutation (run, failed): `Editor::fork_gather` running a deferred build in
/// the gather (`Unit::Built(build())`) → the call waits out the build's
/// three-second timeout and the build has finished when it returns. Mutation
/// (run, failed): `Level::fork_fresh` not detaching its cell (the fork shares
/// the live param) → the volume set after the start reaches the fork. The
/// gather's own guarantee is pinned in tutti-graph's
/// `a_gathered_fork_is_the_live_graph_at_the_gather`.
#[test]
fn an_export_does_not_wait_for_a_slow_fork_and_takes_no_later_edit() {
    let mut session = session();
    let (node, cell, release, built) = slow(0.5);
    let (slow_node, _) = session.graph().insert(node);
    let (level, params) = session
        .graph()
        .insert(Level(Param::new(Amplitude::new(0.5))));
    session.graph().set_node_params(level, Some(params));
    session.graph().set_param(level, UnitParam::Volume, 0.75);
    let (other, _) = session.graph().insert(Const::mono(9.0));
    session
        .graph()
        .set_output_source(0, GraphSource::Node(slow_node, 0));
    session
        .graph()
        .set_output_source(1, GraphSource::Node(level, 0));

    let job = session
        .bounce_start(buffers(ExportSource::Mix, 0.05))
        .expect("the export starts");
    assert!(
        !built.load(Ordering::SeqCst),
        "the fork's build had run by the time the export started: it does not wait for the build"
    );

    // Edits after the start: none reaches the fork.
    cell.store(0.25f32.to_bits(), Ordering::SeqCst);
    session.graph().set_param(level, UnitParam::Volume, 0.25);
    session.graph().set_outputs_from(other);
    let _ = release.send(());

    let rendered = std::thread::spawn(move || job.run())
        .join()
        .expect("the render thread");
    let planes = match session
        .finish_bounce(rendered)
        .expect("the export renders")
        .finished()
        .map(|b| b.into_master())
    {
        Some(Produced::Buffers(r)) => r.into_planes(),
        other => panic!("expected buffers, got {other:?}"),
    };
    assert!(built.load(Ordering::SeqCst), "the build ran");
    assert!(
        planes[0].iter().all(|&s| s == 0.5),
        "the slow node's level at the start, on output 0: {:?}",
        &planes[0][..4]
    );
    assert!(
        planes[1].iter().all(|&s| s == 0.75),
        "the param's value at the start, on output 1: {:?}",
        &planes[1][..4]
    );
}

/// Where a [`Pinned`] unit was built, prepared and dropped.
#[derive(Default)]
struct Threads {
    built: Mutex<Vec<ThreadId>>,
    prepared: Mutex<Vec<ThreadId>>,
    dropped: Mutex<Vec<ThreadId>>,
}

fn here(at: &Mutex<Vec<ThreadId>>) {
    at.lock()
        .unwrap_or_else(|p| p.into_inner())
        .push(std::thread::current().id());
}

/// A unit confined to the main thread, as an in-process VST2 fork is:
/// its source is pinned (`ForkBuild::pinned`), and it records where it
/// is built, prepared and dropped.
struct Pinned {
    threads: Arc<Threads>,
}

impl Node for Pinned {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {
        here(&self.threads.prepared);
    }
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill(1.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

impl Drop for Pinned {
    fn drop(&mut self) {
        here(&self.threads.dropped);
    }
}

struct PinnedFork(Arc<Threads>);

impl ForkSource for PinnedFork {
    fn fork(&self, _mode: ForkMode) -> Result<Forked, ForkCause> {
        here(&self.0.built);
        Ok(Forked::new(Box::new(Pinned {
            threads: Arc::clone(&self.0),
        })))
    }

    fn gather(&self, mode: ForkMode) -> Result<ForkBuild, ForkCause> {
        self.fork(mode).map(ForkBuild::pinned)
    }
}

/// A live [`Pinned`] (recording nothing) whose forks record into
/// `threads`.
struct PinnedNode(Arc<Threads>);

impl IntoNode for PinnedNode {
    type Controls = ();
    fn into_parts(self) -> NodeParts<()> {
        NodeParts {
            node: Box::new(Pinned {
                threads: Arc::default(),
            }),
            controls: (),
            fork: Some(Box::new(PinnedFork(self.0))),
        }
    }
}

fn all_main(what: &str, at: &Mutex<Vec<ThreadId>>) {
    let main = std::thread::current().id();
    let at = at.lock().unwrap();
    assert!(!at.is_empty(), "the fork was never {what}");
    assert!(
        at.iter().all(|&t| t == main),
        "the fork was {what} off the main thread: {at:?} (main {main:?})"
    );
}

/// **A main-thread-only fork source builds on the main thread** though the
/// job builds the rest of its fork on the thread that runs it: a pinned
/// source (an in-process VST2 plugin's) is built and prepared in the gather,
/// in `Session::export`, and the unit it built is dropped by
/// `finish_export`, on the main thread, after the render on a thread of its
/// own. A slow deferred source beside it keeps the job busy for a while.
///
/// Mutation (run, failed): `fork_gather` treating a pinned unit as a
/// `ForkBuild::ready` one (not prepared in the gather) → prepared on the
/// render thread. (The session's `ToMain` drop sending nothing back does not
/// fail this test, which finishes through `finish_export`; it fails the three
/// below.)
#[test]
fn a_main_thread_only_fork_builds_and_drops_on_the_main_thread() {
    let mut session = session();
    let threads = Arc::<Threads>::default();
    let (pinned, _) = session.graph().insert(PinnedNode(Arc::clone(&threads)));
    let (node, _cell, release, _built) = slow(0.5);
    let (slow_node, _) = session.graph().insert(node);
    session
        .graph()
        .set_output_source(0, GraphSource::Node(slow_node, 0));
    session
        .graph()
        .set_output_source(1, GraphSource::Node(pinned, 0));
    drop(release); // the slow build goes straight through

    let job = session
        .bounce_start(buffers(ExportSource::Mix, 0.05))
        .expect("the export starts");
    let rendered = std::thread::spawn(move || job.run())
        .join()
        .expect("the render thread");
    let planes = match session
        .finish_bounce(rendered)
        .expect("the export renders")
        .finished()
        .map(|b| b.into_master())
    {
        Some(Produced::Buffers(r)) => r.into_planes(),
        other => panic!("expected buffers, got {other:?}"),
    };
    assert!(
        planes[1].iter().all(|&s| s == 1.0),
        "the pinned unit renders"
    );
    all_main("built", &threads.built);
    all_main("prepared", &threads.prepared);
    all_main("dropped", &threads.dropped);
}

/// **A job dropped unrun drops its fork on the main thread**, wherever it is
/// dropped: an export whose fork holds a pinned unit (built in the gather)
/// is handed to a thread that drops it unrun (as a cancelled task is dropped
/// by a pool); whatever the job held goes back to the session, and
/// `drop_bounce_leftovers` drops it on the main thread. No render happened.
///
/// Mutations (each run, each failed): the session's `ToMain` drop dropping
/// its value where it is (`drop(value)`) → the pinned unit drops on the
/// thread that dropped the job; the job's pending fork held in a `ToMain`
/// whose receiver is gone → the same.
#[test]
fn a_dropped_export_job_drops_its_fork_on_the_main_thread() {
    let mut session = session();
    let threads = Arc::<Threads>::default();
    let (pinned, _) = session.graph().insert(PinnedNode(Arc::clone(&threads)));
    session
        .graph()
        .set_output_source(0, GraphSource::Node(pinned, 0));
    session
        .graph()
        .set_output_source(1, GraphSource::Node(pinned, 0));

    let job = session
        .bounce_start(buffers(ExportSource::Mix, 0.05))
        .expect("the export starts");
    // Dropped on another thread, never run.
    std::thread::spawn(move || drop(job))
        .join()
        .expect("the dropping thread");
    assert!(
        threads.dropped.lock().unwrap().is_empty(),
        "the pinned unit is held for the main thread, not dropped where the job was"
    );
    session.drop_bounce_leftovers();
    all_main("built", &threads.built);
    all_main("dropped", &threads.dropped);
    assert!(!session.bounce_in_flight(), "the token went with the job");
}

/// Why a [`Broken`] fork's build fails: a type to downcast back out.
#[derive(Debug, PartialEq)]
struct NoServer;

impl std::fmt::Display for NoServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the server would not start")
    }
}

impl std::error::Error for NoServer {}

/// A node whose fork's deferred build fails, as a hosted plugin's does
/// when its `plugin-server` will not start.
struct Broken;

impl Node for Broken {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, _: Io<'_>) -> Status {
        Status::Silent
    }
    fn reset(&mut self) {}
}

struct BrokenFork;

impl ForkSource for BrokenFork {
    fn fork(&self, _mode: ForkMode) -> Result<Forked, ForkCause> {
        unreachable!("an export gathers")
    }
    fn gather(&self, _mode: ForkMode) -> Result<ForkBuild, ForkCause> {
        Ok(ForkBuild::deferred(|| Err(ForkCause::new(NoServer))))
    }
}

impl IntoNode for Broken {
    type Controls = ();
    fn into_parts(self) -> NodeParts<()> {
        NodeParts {
            node: Box::new(self),
            controls: (),
            fork: Some(Box::new(BrokenFork)),
        }
    }
}

/// **A fork whose build fails on the job's thread refuses the export by its
/// node's key**, its cause kept whole, and what the build had made (a pinned
/// unit, built in the gather) is dropped on the main thread, as a rendered
/// graph is.
///
/// Mutations (each run, each failed): `ExportJob::run` dropping a failed
/// build's `ForkRemains` where it is (`drop(remains)`) instead of sending it
/// back → the pinned unit drops on the render thread; the `ToMain` drop
/// dropping its value where it is → the same; `fork_gather` not preparing a
/// pinned unit in the gather → prepared on the render thread.
#[test]
fn a_fork_build_that_fails_names_its_node_and_drops_on_the_main_thread() {
    let mut session = session();
    let threads = Arc::<Threads>::default();
    let (pinned, _) = session.graph().insert(PinnedNode(Arc::clone(&threads)));
    let (broken, _) = session.graph().insert(Broken);
    session
        .graph()
        .set_output_source(0, GraphSource::Node(pinned, 0));
    session
        .graph()
        .set_output_source(1, GraphSource::Node(broken, 0));

    let job = session
        .bounce_start(buffers(ExportSource::Mix, 0.05))
        .expect("the export starts: the build fails in the job");
    let rendered = std::thread::spawn(move || job.run())
        .join()
        .expect("the render thread");
    match session.finish_bounce(rendered) {
        Err(BounceError::Fork(tutti_graph::ForkError::Source { key, cause })) => {
            assert_eq!(key, broken);
            assert_eq!(cause.downcast_ref::<NoServer>(), Some(&NoServer));
        }
        other => panic!("expected the failing build named, got {other:?}"),
    }
    all_main("built", &threads.built);
    all_main("prepared", &threads.prepared);
    all_main("dropped", &threads.dropped);
}

/// Records where it is dropped: captured by a `prepare` hook's edit, it
/// drops when the edit does.
struct EditWitness(Arc<Mutex<Vec<ThreadId>>>);

impl Drop for EditWitness {
    fn drop(&mut self) {
        here(&self.0);
    }
}

/// A request for `source` whose `prepare` edit captures an
/// [`EditWitness`] recording into `drops`.
fn witnessed(source: ExportSource, drops: &Arc<Mutex<Vec<ThreadId>>>) -> Bounce {
    let witness = EditWitness(Arc::clone(drops));
    buffers(source, 0.05).edit(Box::new(move |_prepared: PreparedGraph<'_>| {
        let _ = &witness;
    }))
}

/// **A `prepare` edit that never runs drops on the main thread**, as the
/// fork does: one whose fork's build fails (the job returns before the
/// edit), and one whose job is dropped unrun on another thread (as a
/// cancelled task is dropped by whichever pool thread reaches it). An edit
/// may capture anything the caller read on the main thread; until it runs it
/// is that state.
///
/// Mutations (each run, each failed): the job's edit held in a `ToMain`
/// whose receiver is gone (the edit effectively bare) → the witness drops on
/// the render thread, or on the thread that dropped the job; the `ToMain`
/// drop dropping its value where it is → the same.
#[test]
fn a_prepare_edit_that_never_runs_drops_on_the_main_thread() {
    // A build that fails.
    let mut session = session();
    let (broken, _) = session.graph().insert(Broken);
    session.graph().set_outputs_from(broken);
    let drops: Arc<Mutex<Vec<ThreadId>>> = Arc::default();
    let job = session
        .bounce_start(witnessed(ExportSource::Mix, &drops))
        .expect("the export starts");
    let rendered = std::thread::spawn(move || job.run())
        .join()
        .expect("the render thread");
    match session.finish_bounce(rendered) {
        Err(BounceError::Fork(tutti_graph::ForkError::Source { .. })) => {}
        other => panic!("expected the failing build, got {other:?}"),
    }
    all_main("dropped (failed build)", &drops);

    // A job dropped before it ran.
    let mut session = self::session();
    let (dc, _) = session.graph().insert(Const::mono(0.5));
    session.graph().set_outputs_from(dc);
    let drops: Arc<Mutex<Vec<ThreadId>>> = Arc::default();
    let job = session
        .bounce_start(witnessed(ExportSource::Mix, &drops))
        .expect("the export starts");
    std::thread::spawn(move || drop(job))
        .join()
        .expect("the dropping thread");
    assert!(
        drops.lock().unwrap().is_empty(),
        "the edit is held for the main thread, not dropped where the job was"
    );
    session.drop_bounce_leftovers();
    all_main("dropped (job dropped before it ran)", &drops);
}
