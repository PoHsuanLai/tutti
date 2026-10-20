//! The session's graph properties, with no host: the edit flag, the
//! crossfade retry, the event feeds, compensation, and what a fork renders.
//!
//! The graph halves of what `bevy-tutti`'s `graph::runtime` tests pinned
//! through an `App` (design 015 §9, PR 4); the ECS halves stayed there.

use std::sync::Arc;

use tutti_core::{
    ChannelLayout, CrossfadeCurve, Drive, Hz, InterleavedMut, SampleRate, Samples, Seconds,
    MAX_ROOT_CHANNELS,
};
use tutti_graph::{
    Cx, ForkByClone, IntoNode, Io, Node, NodeParts, ParamNode, ParamSet, Prepare, Shape, Status,
    Unforkable,
};
use tutti_nodes::testing::{Const, Osc, Through};
use tutti_session::{
    root_width, Crossfade, EventSource, GraphSource, Session, SessionEvent, SessionNode,
};
use tutti_types::{Latency, Param, UnitParam};

const RATE: SampleRate = SampleRate(48_000.0);

/// A detached session `outputs` wide at 48 kHz: the executor on this side.
fn detached(outputs: usize) -> Session {
    Session::builder()
        .outputs_floor(outputs)
        .build_detached(RATE)
        .expect("builds")
}

/// The session is `Send`: a host may build it on one thread and hand it to
/// its main thread (design 015 §4.2). The `Device`'s `!Send` is its own
/// `compile_fail` doctest.
///
/// Mutation (run): a `std::rc::Rc<()>` field on `Session` → this fails to
/// compile.
#[test]
fn a_session_is_send() {
    fn send<T: Send>() {}
    send::<Session>();
}

// ---- stimulus nodes ---------------------------------------------------------

/// A source whose one output is its `Drive` param: its drive a `Param` cell
/// addressed by a `ParamSet`, inserted through `param_parts` (so its fork
/// starts from the values its cells hold).
#[derive(Clone)]
struct Knob {
    drive: Param<Drive>,
}

impl Knob {
    const BUILT_WITH: f32 = 1.0;

    fn new() -> Self {
        Self::at(Self::BUILT_WITH)
    }

    fn at(drive: f32) -> Self {
        Self {
            drive: Param::new(Drive(drive)),
        }
    }
}

impl Node for Knob {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO).with_params(&[UnitParam::Drive])
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        match io.param(0).frames() {
            Some(v) => {
                let v: Vec<f32> = v.to_vec();
                io.output(0).copy_from_slice(&v);
            }
            None => io.output(0).fill(self.drive.load().get()),
        }
        Status::Modified
    }
    fn reset(&mut self) {}
    fn param_base(&self, port: usize) -> Option<f32> {
        (port == 0).then(|| self.drive.load().get())
    }
}

impl ParamNode for Knob {
    fn param_set(&self) -> ParamSet {
        ParamSet::builder()
            .param(UnitParam::Drive, self.drive.as_atomic())
            .build()
    }
    fn fork_fresh(&self) -> Self {
        let mut fork = self.clone();
        fork.drive.detach();
        fork
    }
}

impl IntoNode for Knob {
    type Controls = ParamSet;
    fn into_parts(self) -> NodeParts<ParamSet> {
        tutti_graph::param_parts(self)
    }
}

impl SessionNode for Knob {
    fn params(controls: &ParamSet) -> Option<ParamSet> {
        Some(controls.clone())
    }
}

/// A node that cannot run above 90 kHz: its `prepare` panics there, which is
/// how a re-prepare poisons an editor.
struct Grenade;

impl Node for Grenade {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO)
    }
    fn prepare(&mut self, prepare: &Prepare) {
        assert!(prepare.sample_rate().get() < 90_000.0, "no such rate");
    }
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// A pure delay of `N` frames that declares `N` frames of latency: what a
/// lookahead is to PDC, with an exact figure.
#[derive(Clone)]
struct Late<const N: usize> {
    ring: Vec<f32>,
    at: usize,
}

impl<const N: usize> Late<N> {
    fn new() -> Self {
        Self {
            ring: vec![0.0; N],
            at: 0,
        }
    }
}

impl<const N: usize> Node for Late<N> {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::MONO, ChannelLayout::MONO)
            .with_latency(Latency::new(Samples(N)))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let (i, mut o) = io.split();
        for (y, &x) in o.get(0).iter_mut().zip(i.get(0)) {
            *y = self.ring[self.at];
            self.ring[self.at] = x;
            self.at = (self.at + 1) % N;
        }
        Status::Modified
    }
    fn reset(&mut self) {
        self.ring.fill(0.0);
        self.at = 0;
    }
}

/// A node with `IN` event inputs and `OUT` event outputs and nothing else:
/// a sink or a source of event edges.
#[derive(Clone)]
struct Events<const IN: u16, const OUT: u16>;

impl<const IN: u16, const OUT: u16> Node for Events<IN, OUT> {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO).with_events(IN, OUT)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

// ---- the graph's own vocabulary ----------------------------------------------

/// Every [`GraphSource`] arm survives a write and a read-back, on a node
/// port and on a global output: the translation to the spec's spelling and
/// back is total and lossless. (Moved from `bevy-tutti`'s
/// `AudioGraphRes` tests, whose methods now delegate here.)
///
/// Mutation (run): lowering `Input(port)` to `Zero` fails the `Input` row;
/// lowering `Node` with a fixed port 0 fails the port-1 row.
#[test]
fn every_source_reads_back_as_written() {
    let mut s = Session::builder()
        .inputs(1)
        .outputs_floor(2)
        .build_detached(RATE)
        .expect("builds");
    let graph = s.graph();
    let (stereo, _) = graph.insert(Const::frame(&[1.0, 2.0]));
    let (sink, _) = graph.insert(Through::mono());
    for source in [
        GraphSource::Node(stereo, 1),
        GraphSource::Node(stereo, 0),
        GraphSource::Input(0),
        GraphSource::Silence,
    ] {
        graph.set_source(sink, 0, source);
        assert_eq!(graph.source(sink, 0), source, "node port");
        graph.set_output_source(1, source);
        assert_eq!(graph.output_source(1), source, "global output");
    }
}

/// `remove` says whether the node was there, so removing a node twice is a
/// no-op rather than a panic.
///
/// Mutation (run): `Graph::remove` dropping its `nodes.remove` guard → the
/// second call asks the editor again and answers `true`.
#[test]
fn removing_a_node_twice_is_a_no_op() {
    let mut s = detached(1);
    let (node, _) = s.graph().insert(Const::mono(1.0));
    assert!(s.graph_ref().contains(node));
    assert!(s.remove(node));
    assert!(!s.graph_ref().contains(node));
    assert!(!s.remove(node));
}

/// `root_width` is `max(project, device)` clamped to
/// `1..=MAX_ROOT_CHANNELS`; each row is one way the rule is load-bearing.
/// (Moved from `bevy-tutti`'s `engine::build`.)
///
/// Mutation (run): `root_width` returning `project.clamp(..)` (the device no
/// floor) → the first row fails.
#[test]
fn root_width_takes_the_wider_side_within_the_scratch_bounds() {
    let cases: &[(usize, ChannelLayout, usize, &str)] = &[
        (
            2,
            ChannelLayout::from(6u16),
            6,
            "a wider device widens the root: a stereo project on a 5.1 device \
             must render all six, or the top four are permanently silent",
        ),
        (
            6,
            ChannelLayout::STEREO,
            6,
            "a wider project is kept and folded, not clamped",
        ),
        (
            0,
            ChannelLayout::from(6u16),
            6,
            "an unset project width falls through to the device",
        ),
        (
            0,
            ChannelLayout::STEREO,
            2,
            "an unset project width falls through to the device",
        ),
        (
            64,
            ChannelLayout::from(32u16),
            MAX_ROOT_CHANNELS,
            "nothing exceeds the render scratch",
        ),
        (
            0,
            ChannelLayout::EMPTY,
            1,
            "the root is never zero wide: a zero-output root would render \
             nothing at all",
        ),
    ];
    for &(project, device, expected, why) in cases {
        assert_eq!(
            root_width(project, device),
            expected,
            "root_width({project}, {} channels): {why}",
            device.count()
        );
    }
}

// ---- the edit flag ------------------------------------------------------------

/// **An edit between two updates is committed by the next update.** One
/// commit, and the plan it sends is a new one.
///
/// Mutation (run): `Phase::Commit` committing only on a flag nobody sets
/// (`self.resumed && …` in place of `self.graph.is_edited()`) → the edit is
/// never sent → fails.
#[test]
fn an_edit_between_updates_is_committed_by_the_next() {
    let mut s = detached(1);
    let (a, _) = s.graph().insert(Const::mono(1.0));
    s.graph().set_output_source(0, GraphSource::Node(a, 0));
    s.update();
    let sent = s.graph_ref().commits_sent();
    let plan = s
        .graph_ref()
        .sent_plan()
        .expect("the first update sent a plan");

    let (b, _) = s.graph().insert(Const::mono(2.0));
    s.graph().set_output_source(0, GraphSource::Node(b, 0));
    assert!(s.graph_ref().is_edited());
    s.update();
    assert!(!s.graph_ref().is_edited(), "the update committed the edit");
    assert_eq!(s.graph_ref().commits_sent(), sent + 1, "one commit");
    let after = s.graph_ref().sent_plan().expect("a plan");
    assert!(!Arc::ptr_eq(&plan, &after), "and it sent a new plan");
    let mut out = [0.0f32];
    s.render_frame(&mut out);
    assert_eq!(out[0], 2.0, "the edit is what renders");
}

/// **An update with no edit sends no plan**: the plan the audio side holds
/// is the same `Arc`, and no commit was counted.
///
/// Mutation (run): `Phase::Commit` committing unconditionally (dropping the
/// `is_edited()` test) → each update sends a plan → fails.
#[test]
fn an_update_with_no_edit_sends_no_plan() {
    let mut s = detached(1);
    let (a, _) = s.graph().insert(Const::mono(1.0));
    s.graph().set_output_source(0, GraphSource::Node(a, 0));
    s.update();
    let sent = s.graph_ref().commits_sent();
    let plan = s
        .graph_ref()
        .sent_plan()
        .expect("the first update sent a plan");
    for _ in 0..3 {
        s.update();
    }
    assert_eq!(s.graph_ref().commits_sent(), sent, "no commit");
    let after = s.graph_ref().sent_plan().expect("a plan");
    assert!(Arc::ptr_eq(&plan, &after), "the same plan");
}

// ---- crossfades ---------------------------------------------------------------

/// **A crossfade refused `Busy` during a re-prepare lands on a later
/// update**, and its controls come with the landing: the graph an engine
/// runs (the audio side is taken) changing rate, and a crossfade before the
/// executor has handed its units back. The graph half of `bevy-tutti`'s
/// `a_crossfade_during_a_re_prepare_lands_after_it` (its entity half binds
/// the controls this hands back).
///
/// `Editor::replace` refuses while re-preparing and consumes its unit, so
/// without the wait the new unit would be lost.
///
/// Mutations (run):
/// - `Graph::crossfade` turning `Busy` into `CrossfadeRefused` (not parking)
///   → the request is refused → fails;
/// - `Phase::Collect` not retrying the parked crossfades → it never lands
///   → fails;
/// - `Swap::swap` not addressing the incoming node's params → the landing's
///   controls steer it, but `set_param` reaches nothing → the last check
///   fails.
#[test]
fn a_crossfade_refused_busy_during_a_re_prepare_lands_on_a_later_update() {
    let mut s = detached(1);
    let mut side = s.graph().take_audio_side();
    let (knob, old) = s.graph().add(Knob::new());
    s.graph().set_output_source(0, GraphSource::Node(knob, 0));
    assert!(s.update().is_empty());
    let mut out = [0.0f32];
    side.tick(&[], &mut out);
    assert_eq!(out[0], Knob::BUILT_WITH, "the knob plays");

    // The rate changes on the running graph: the first half is sent, and
    // the executor has not run it yet.
    s.graph().set_sample_rate(SampleRate(44_100.0));
    let fade = Seconds(0.005);
    let waiting =
        match s
            .graph()
            .crossfade(knob, Knob::at(3.0), fade, CrossfadeCurve::EqualAmplitude)
        {
            Ok(Crossfade::Waiting(id)) => id,
            other => panic!("the crossfade waits for the re-prepare: {other:?}"),
        };
    assert_eq!(s.graph_ref().waiting_crossfades(), 1);

    // Update by update through the re-prepare: the executor checks its units
    // out (a silent block), the editor sends them back, the executor
    // resumes. The crossfade waits until an update finds the graph resumed.
    assert!(s.update().is_empty());
    side.tick(&[], &mut out);
    assert!(s.update().is_empty());
    assert!(old.set(UnitParam::Drive, 7.0));
    side.tick(&[], &mut out);
    assert_eq!(
        out[0], 7.0,
        "the old unit plays on, steered by its controls"
    );

    let events = s.update();
    let controls = match events.as_slice() {
        [SessionEvent::CrossfadeLanded { node, id, controls }] => {
            assert_eq!((*node, *id), (knob, waiting));
            controls
                .downcast_ref::<ParamSet>()
                .expect("the incoming knob's controls")
                .clone()
        }
        other => panic!("landed once the graph resumed: {other:?}"),
    };
    assert_eq!(s.graph_ref().waiting_crossfades(), 0);
    for _ in 0..512 {
        side.tick(&[], &mut out);
    }
    assert_eq!(out[0], 3.0, "the incoming knob is what sounds");
    assert!(controls.set(UnitParam::Drive, 8.0));
    side.tick(&[], &mut out);
    assert_eq!(out[0], 8.0, "its controls steer it");
    s.graph().set_param(knob, UnitParam::Drive, 9.0);
    side.tick(&[], &mut out);
    assert_eq!(out[0], 9.0, "and its params are addressed");
}

/// **On a poisoned graph a crossfade is refused**: nothing is parked (no
/// update will ever take it). The graph half of `bevy-tutti`'s
/// `a_crossfade_on_a_poisoned_graph_is_refused_and_keeps_the_controls`
/// (its entity half: the controls stay put).
///
/// Mutation (run): `Graph::replace` answering `Busy` on a poisoned graph →
/// the request waits for good → fails.
#[test]
fn a_crossfade_on_a_poisoned_graph_is_refused() {
    let mut s = detached(1);
    let (knob, _) = s.graph().add(Knob::new());
    s.graph().add(Unforkable(Grenade));
    s.update();
    // Both halves run here (the executor is local); the second panics in
    // the grenade's `prepare` and poisons the editor.
    s.graph().set_sample_rate(SampleRate(96_000.0));
    let refused = s.graph().crossfade(
        knob,
        Knob::at(3.0),
        Seconds(0.005),
        CrossfadeCurve::EqualAmplitude,
    );
    assert!(refused.is_err(), "{refused:?}");
    assert_eq!(s.graph_ref().waiting_crossfades(), 0, "nothing waits");
    assert!(s.update().is_empty());
}

// ---- params and forks -----------------------------------------------------------

/// **Every param write path the session has reaches a fork of a node**
/// (the constraint export by `Editor::fork` rests on): `Graph::set_param`
/// through the node's addressed `ParamSet`, and a write to the controls
/// `Graph::add` handed back. A fork starts from the values its cells hold.
/// The graph half of `bevy-tutti`'s `every_param_write_path_reaches_a_fork`
/// (its half: `AudioParam`, and the modulated paths).
///
/// Mutations (run; each fails its channel):
/// - `Graph::set_param` skipping the node's `ParamSet` → channel 0 stays
///   at 1, live and forked;
/// - `Graph::add` not addressing the params (`set_node_params`) → channel
///   0 stays at 1;
/// - `Knob::fork_fresh` detaching from a fresh `Param` (not the written one)
///   → both channels of the fork read 1.
#[test]
fn every_param_write_path_reaches_a_fork() {
    let mut s = detached(2);
    let (k0, _) = s.graph().add(Knob::new());
    let (k1, c1) = s.graph().add(Knob::new());
    s.graph().set_output_source(0, GraphSource::Node(k0, 0));
    s.graph().set_output_source(1, GraphSource::Node(k1, 0));
    s.update();

    s.graph().set_param(k0, UnitParam::Drive, 5.0);
    assert!(c1.set(UnitParam::Drive, 4.0));
    let mut live = [0.0f32; 2];
    s.render_frame(&mut live);
    assert_eq!(live, [5.0, 4.0], "each write reaches the live node");
    let mut fork = s.graph_ref().fork_live().expect("every knob forks");
    let mut forked = [0.0f32; 2];
    fork.tick(&[], &mut forked);
    assert_eq!(forked, [5.0, 4.0], "and the fork");
}

// ---- event feeds ----------------------------------------------------------------

/// **`move_feeds` moves every feeder's sources** from one sink to another:
/// a host whose sink was re-bound to a new node keeps what each feeder fed
/// it, and the old sink is fed by nothing. Each feeder keeps its own entry,
/// and the graph's event edges are their union.
///
/// Mutations (run):
/// - `move_feeds` moving only the first feeder → the route's source is
///   lost → fails;
/// - `move_feeds` not rewriting `from` → the old sink keeps its edges →
///   fails;
/// - `set_feed` replacing the whole sink's map (not the feeder's entry) →
///   the clip's entry goes when the route's is set → fails.
#[test]
fn move_feeds_moves_every_feeders_sources() {
    let mut s = detached(1);
    let g = s.graph();
    let (old, _) = g.insert(ForkByClone(Events::<1, 0>));
    let (new, _) = g.insert(ForkByClone(Events::<1, 0>));
    let (clip, _) = g.insert(ForkByClone(Events::<0, 1>));
    let (route, _) = g.insert(ForkByClone(Events::<0, 1>));
    g.set_feed(old, "clip", vec![clip.into()]);
    g.set_feed(old, "route", vec![EventSource::new(route, 0)]);
    let both = {
        let mut v = vec![EventSource::from(clip), EventSource::from(route)];
        v.sort_by_key(|e| e.node.key().0);
        v
    };
    assert_eq!(g.fed(old), both, "each feeder kept its own entry");
    let mut edges = g.event_sources(old, 0);
    edges.sort_by_key(|e| e.node.key().0);
    assert_eq!(edges, both, "the union is the sink's event edges");

    g.move_feeds(old, new);
    assert_eq!(g.feed(new, "clip"), [EventSource::from(clip)]);
    assert_eq!(g.feed(new, "route"), [EventSource::from(route)]);
    assert!(g.fed(old).is_empty(), "the old sink is fed by nothing");
    let mut edges = g.event_sources(new, 0);
    edges.sort_by_key(|e| e.node.key().0);
    assert_eq!(edges, both, "the new sink's edges are the union");
    assert!(g.event_sources(old, 0).is_empty(), "and the old's are gone");
    assert_eq!(s.update().len(), 0);
    assert!(!s.graph_ref().is_edited(), "it commits");
}

// ---- rendering --------------------------------------------------------------------

/// Mean period, in frames, of the rising zero crossings in `x`
/// (interpolated between the two frames that straddle each).
fn period(x: &[f32]) -> f64 {
    let crossings: Vec<f64> = (1..x.len())
        .filter(|&i| x[i - 1] < 0.0 && x[i] >= 0.0)
        .map(|i| (i - 1) as f64 + f64::from(x[i - 1] / (x[i - 1] - x[i])))
        .collect();
    assert!(crossings.len() > 10, "a tone to measure");
    (crossings[crossings.len() - 1] - crossings[0]) / (crossings.len() - 1) as f64
}

/// `frames` frames of `s`, `layout` wide, in `block`-frame blocks.
fn pull(s: &Session, layout: ChannelLayout, frames: usize, block: usize) -> Vec<f32> {
    let width = usize::from(layout.count());
    let mut out = Vec::with_capacity(frames * width);
    let mut buf = vec![0.0f32; block * width];
    while out.len() < frames * width {
        s.render(&mut InterleavedMut::new(&mut buf, layout));
        out.extend_from_slice(&buf);
    }
    out.truncate(frames * width);
    out
}

/// **An offline session with a 1 kHz sine renders its analytic RMS**:
/// `1/√2` over a whole number of cycles (one second at 48 kHz is 1 000 of
/// them), at 48 frames a cycle (the graph runs at the rate it was built
/// at).
///
/// Mutations (run):
/// - `Session::update` running no phase → nothing is committed, the render
///   is silent → the RMS is 0 → fails;
/// - `build_offline` building the graph at `SampleRate::DEFAULT` (44.1 kHz)
///   → 44.1 frames a cycle → fails.
#[test]
fn an_offline_session_renders_a_sines_analytic_rms() {
    let mut s = Session::builder()
        .build_offline(RATE, ChannelLayout::MONO, Samples(512))
        .expect("builds");
    let (osc, _) = s.graph().add(ForkByClone(Osc::sine(Hz(1_000.0))));
    s.graph().set_outputs_from(osc);
    s.update();
    let out = pull(&s, ChannelLayout::MONO, 48_000, 512);
    let rms = (out.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>() / out.len() as f64).sqrt();
    assert!(
        (rms - std::f64::consts::FRAC_1_SQRT_2).abs() < 1e-4,
        "RMS {rms}, a unit sine's is 1/√2"
    );
    assert!(
        (period(&out) - 48.0).abs() < 1e-3,
        "{} frames a cycle",
        period(&out)
    );
}

/// **A latent node on one branch is compensated on the other, to the
/// sample**: a sine straight to output 0 and through a 37-frame delay that
/// declares its 37 frames to output 1. The dry branch is delayed by exactly
/// 37 frames, so the two outputs are the same samples, bit for bit, both
/// silent for the first 37 frames (the sine's first sample is `sin(0)`, so
/// the first sound is frame 38); the session publishes the figures before
/// the first block: a total of 37, and a pre-roll of 37 on the dry channel
/// (a source outside the graph seeks by it) and 0 on the latent one.
///
/// Mutations (run):
/// - `Late` declaring no latency → the dry channel leads by 37 frames →
///   fails;
/// - `Session::publish_sent` not publishing the table → empty → fails;
/// - `Session::commit` not publishing on `Sent` → the total reads 0 →
///   fails.
#[test]
fn a_latent_branch_is_compensated_on_the_other_to_the_sample() {
    const N: usize = 37;
    let mut s = Session::builder()
        .build_offline(RATE, ChannelLayout::STEREO, Samples(256))
        .expect("builds");
    let (osc, _) = s.graph().add(ForkByClone(Osc::sine(Hz(440.0))));
    let (late, _) = s.graph().add(ForkByClone(Late::<N>::new()));
    s.graph().set_source(late, 0, GraphSource::Node(osc, 0));
    s.graph().set_output_source(0, GraphSource::Node(osc, 0));
    s.graph().set_output_source(1, GraphSource::Node(late, 0));
    s.update();
    assert_eq!(s.latency(), Samples(N), "the total is published");
    assert_eq!(
        s.compensation().lend().as_slice(),
        [Samples(N), Samples(0)],
        "the dry channel pre-rolls by the latent one's latency"
    );

    let out = pull(&s, ChannelLayout::STEREO, 4_096, 256);
    let (dry, wet): (Vec<f32>, Vec<f32>) =
        out.as_chunks::<2>().0.iter().map(|f| (f[0], f[1])).unzip();
    let first = |x: &[f32]| x.iter().position(|&v| v != 0.0);
    assert_eq!(first(&wet), Some(N + 1), "the latent branch starts late");
    assert_eq!(first(&dry), Some(N + 1), "the dry branch waits for it");
    assert!(
        dry.iter()
            .zip(&wet)
            .all(|(a, b)| a.to_bits() == b.to_bits()),
        "the same samples on both, to the bit"
    );
}

/// **The main-thread guard sits where a unit is freed, not on an edit**
/// (design 015 §4.2). A removal frees nothing, so a session owned by another
/// thread may remove a node (Bevy removes a clip's node from a parallel
/// system); the free happens at a collect, here a detached commit's, and
/// that is what asserts. Debug builds with `plugin` only: the guard is a
/// `debug_assert!` behind that feature.
///
/// The session goes back to this thread to be dropped: its drop asserts too.
///
/// Mutations (run):
/// - `Session::remove` asserting the main thread again → the removal
///   panics → fails;
/// - `Graph::pump_local` without its guard → the commit off the main
///   thread frees unchecked and does not panic → fails.
#[cfg(all(feature = "plugin", debug_assertions))]
#[test]
fn a_removal_is_any_threads_and_a_free_is_the_main_threads() {
    use std::panic::{catch_unwind, AssertUnwindSafe};

    tutti_plugin_types::mark_main_thread();
    let mut s = detached(1);
    let (dc, _) = s.graph().insert(Const::mono(0.5));
    s.update();

    let (s, removed) = std::thread::spawn(move || {
        let mut s = s;
        let removed = catch_unwind(AssertUnwindSafe(|| s.remove(dc)));
        (s, removed)
    })
    .join()
    .expect("the session comes back");
    assert!(
        removed.expect("a removal off the main thread does not assert"),
        "the node was in the graph"
    );

    let (s, panicked) = std::thread::spawn(move || {
        let mut s = s;
        let freed = catch_unwind(AssertUnwindSafe(|| {
            s.commit();
        }));
        (s, freed.is_err())
    })
    .join()
    .expect("the session comes back");
    assert!(panicked, "a commit off the main thread frees, and asserts");
    drop(s);
}
