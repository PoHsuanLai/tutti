//! The executor's own behaviour: silence skipping, status handling, the
//! audio-thread mark, the block bound, and the editor's control-side
//! protocol (typed controls, generations, the commit queue and its back-pressure).

use tutti_graph::testing as common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tutti_fixtures::midi::{m1_note_off, m1_note_on};
use tutti_ump::{MidiChannel, NoteNumber, U7, Ump};

use common::{Kind, TestNode, prepare};
use tutti_graph::NodeKey;
use tutti_graph::{
    CommitError, Cx, Editor, Executor, IntoNode, Io, Node, NodeParts, Prepare, QUEUE_CAPACITY,
    Shape, SilenceMask, Status, Transport, Unforkable,
};
use tutti_graph::{Edge, InPort, OutPort, Source};
use tutti_types::{Amplitude, ChannelLayout, Latency, Param, Samples, Tail};

const SRC: NodeKey = NodeKey(1);
const FX: NodeKey = NodeKey(2);

fn wire(ed: &mut Editor, sink: NodeKey, from: NodeKey) {
    ed.spec_mut().topology.edges.insert(
        InPort {
            node: sink,
            port: 0,
        },
        vec![Edge::Direct(Source::Node(OutPort {
            node: from,
            port: 0,
        }))],
    );
}

fn render(exec: &mut Executor, frames: usize, outs: usize) -> Vec<Vec<f32>> {
    let mut bufs = vec![vec![0.0f32; frames]; outs];
    let mut refs: Vec<&mut [f32]> = bufs.iter_mut().map(Vec::as_mut_slice).collect();
    exec.process(frames, &Transport::default(), &[], &mut refs);
    bufs
}

/// A node that counts its calls and has a chosen tail, fed by a silent
/// constant.
fn counted_after_silence(tail: Tail) -> (Editor, Executor, Arc<AtomicUsize>) {
    counted_after_silence_with(Samples::ZERO, tail)
}

/// [`counted_after_silence`], the probe declaring `latency` too.
fn counted_after_silence_with(
    latency: Samples,
    tail: Tail,
) -> (Editor, Executor, Arc<AtomicUsize>) {
    let (mut ed, mut exec) = Editor::new(prepare(64));
    ed.insert(
        SRC,
        "const",
        Unforkable(TestNode::new(Kind::Const {
            value: 0.0,
            width: 1,
        })),
    );
    let calls = Arc::new(AtomicUsize::new(0));
    ed.insert(
        FX,
        "probe",
        Unforkable(TailProbe {
            latency,
            tail,
            calls: Arc::clone(&calls),
        }),
    );
    wire(&mut ed, FX, SRC);
    ed.spec_mut()
        .topology
        .set_outputs(vec![Source::Node(OutPort { node: FX, port: 0 })]);
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    (ed, exec, calls)
}

/// Passes its input through and counts calls. Honest about silence: a
/// silent input gives `Status::Silent`, which is what lets the skip trust it.
struct TailProbe {
    latency: Samples,
    tail: Tail,
    calls: Arc<AtomicUsize>,
}

impl Node for TailProbe {
    fn shape(&self) -> Shape {
        Shape::through_unassigned(ChannelLayout::MONO)
            .with_latency(Latency::new(self.latency))
            .with_tail(self.tail)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if io.silent().get(0) {
            return Status::Silent;
        }
        io.channel(0).map(|x| x);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// A node with no tail is called once on silence — a node never called is not
/// known to be quiet — and then skipped; one with a finite tail is called
/// until the tail has elapsed; one that has not said is always called.
///
/// `Const { value: 0.0 }` returns `Status::Constant` with `+0.0`, so its
/// output is *flagged* silent — which is what the skip reads.
#[test]
fn silent_inputs_skip_nodes_whose_tail_has_elapsed() {
    let (_ed, mut exec, calls) = counted_after_silence(Tail::None);
    for _ in 0..10 {
        let out = render(&mut exec, 64, 1);
        assert!(out[0].iter().all(|&x| x == 0.0));
    }
    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "Tail::None: called once, then skipped"
    );

    let (_ed, mut exec, calls) = counted_after_silence(Tail::Finite(Samples(130)));
    for _ in 0..10 {
        render(&mut exec, 64, 1);
    }
    // Quiet frames before each block: 0, 64, 128 (< 130), then 192 ≥ 130.
    assert_eq!(
        calls.load(Ordering::Relaxed),
        3,
        "called until the tail ran out"
    );

    let (_ed, mut exec, calls) = counted_after_silence(Tail::Unknown);
    for _ in 0..10 {
        render(&mut exec, 64, 1);
    }
    assert_eq!(
        calls.load(Ordering::Relaxed),
        10,
        "Tail::Unknown: always called"
    );
}

/// A tail excludes latency, so the skip waits for latency **plus** tail:
/// a node declaring latency `D` and tail `T` whose output reads silent from
/// the first quiet frame is still called on every one of the first `D + T`
/// quiet frames, and skipped from then on. In one-frame blocks the count is
/// exact: called at quiet counts `0..D + T`, so `D + T` calls.
///
/// The second case is a pure delay (`Tail::None`): still called for `D`
/// frames, because its line holds `D` frames of input when the input stops.
#[test]
fn the_skip_waits_for_latency_plus_tail() {
    const D: usize = 37;
    const T: usize = 23;
    for (tail, want) in [(Tail::Finite(Samples(T)), D + T), (Tail::None, D)] {
        let (_ed, mut exec, calls) = counted_after_silence_with(Samples(D), tail);
        for _ in 0..(D + T) * 3 {
            let out = render(&mut exec, 1, 1);
            assert_eq!(out[0][0], 0.0);
        }
        assert_eq!(
            calls.load(Ordering::Relaxed),
            want,
            "{tail:?} after latency {D}: called for the first {want} quiet frames"
        );
    }
}

/// A node that writes garbage and returns `Status::Silent` produces silence,
/// and its flag lets the next node be skipped.
#[test]
fn status_silent_zeroes_and_flags() {
    struct Liar;
    impl Node for Liar {
        fn shape(&self) -> Shape {
            Shape::source_unassigned(ChannelLayout::MONO)
        }
        fn prepare(&mut self, _: &Prepare) {}
        fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
            io.output(0).fill(123.0);
            Status::Silent
        }
        fn reset(&mut self) {}
    }
    let (mut ed, mut exec) = Editor::new(prepare(32));
    ed.insert(SRC, "liar", Unforkable(Liar));
    let calls = Arc::new(AtomicUsize::new(0));
    ed.insert(
        FX,
        "probe",
        Unforkable(TailProbe {
            latency: Samples::ZERO,
            tail: Tail::None,
            calls: Arc::clone(&calls),
        }),
    );
    wire(&mut ed, FX, SRC);
    ed.spec_mut().topology.set_outputs(vec![
        Source::Node(OutPort { node: SRC, port: 0 }),
        Source::Node(OutPort { node: FX, port: 0 }),
    ]);
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    for _ in 0..6 {
        let out = render(&mut exec, 32, 2);
        assert!(out[0].iter().all(|&x| x == 0.0));
    }
    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "downstream skipped on the flag"
    );
}

/// A node reports per-channel silence with `Status::Masked`, and the
/// downstream node sees it in its `Io::silent` mask.
#[test]
fn masked_status_reaches_the_next_nodes_mask() {
    struct HalfSilent;
    impl Node for HalfSilent {
        fn shape(&self) -> Shape {
            Shape::source_unassigned(ChannelLayout::STEREO)
        }
        fn prepare(&mut self, _: &Prepare) {}
        fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
            io.output(0).fill(0.0);
            io.output(1).fill(0.5);
            Status::Masked {
                silent: SilenceMask::NONE.with(0),
                constant: tutti_graph::ConstantMask::NONE,
            }
        }
        fn reset(&mut self) {}
    }
    struct Observer(Arc<AtomicUsize>);
    impl Node for Observer {
        fn shape(&self) -> Shape {
            Shape::buses(
                tutti_graph::Buses::unassigned(ChannelLayout::STEREO),
                tutti_graph::Buses::unassigned(ChannelLayout::MONO),
            )
            .with_tail(Tail::Unknown)
        }
        fn prepare(&mut self, _: &Prepare) {}
        fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
            self.0.store(io.silent().0 as usize, Ordering::Relaxed);
            io.output(0).fill(0.0);
            Status::Modified
        }
        fn reset(&mut self) {}
    }
    let seen = Arc::new(AtomicUsize::new(99));
    let (mut ed, mut exec) = Editor::new(prepare(16));
    ed.insert(SRC, "half", Unforkable(HalfSilent));
    ed.insert(FX, "observer", Unforkable(Observer(Arc::clone(&seen))));
    for port in 0..2 {
        ed.spec_mut().topology.edges.insert(
            InPort { node: FX, port },
            vec![Edge::Direct(Source::Node(OutPort { node: SRC, port }))],
        );
    }
    ed.spec_mut()
        .topology
        .set_outputs(vec![Source::Node(OutPort { node: FX, port: 0 })]);
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    render(&mut exec, 16, 1);
    assert_eq!(seen.load(Ordering::Relaxed), 0b01);
}

/// Freeing a value whose `Drop` checks the audio-thread marker inside a
/// node's `process` panics in a debug build — the executor marks the audio
/// thread for the whole block. The probe is the check every control-freed
/// value in the graph makes (`NodeBox`, a crossfade, a commit):
/// `RtScope::check_outside` from its `Drop`.
#[test]
#[cfg(debug_assertions)]
fn freeing_a_control_freed_value_inside_process_panics_in_debug() {
    /// A buffer that must be freed on the control thread.
    struct ControlFreed(#[allow(dead_code, reason = "held only to be freed")] Vec<f32>);
    impl Drop for ControlFreed {
        fn drop(&mut self) {
            tutti_rt::RtScope::check_outside("a probe buffer");
        }
    }
    struct Freer(Option<ControlFreed>);
    impl Node for Freer {
        fn shape(&self) -> Shape {
            Shape::source_unassigned(ChannelLayout::MONO)
        }
        fn prepare(&mut self, _: &Prepare) {}
        fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
            self.0.take(); // frees on the audio thread
            io.output(0).fill(0.0);
            Status::Modified
        }
        fn reset(&mut self) {}
    }
    let (mut ed, mut exec) = Editor::new(prepare(16));
    ed.insert(
        SRC,
        "freer",
        Unforkable(Freer(Some(ControlFreed(vec![0.0; 8])))),
    );
    ed.spec_mut()
        .topology
        .set_outputs(vec![Source::Node(OutPort { node: SRC, port: 0 })]);
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| render(&mut exec, 16, 1)));
    let msg = r.expect_err("the drop panics");
    let msg = msg.downcast_ref::<String>().cloned().unwrap_or_default();
    assert!(msg.contains("dropped on the audio thread"), "{msg}");
}

/// A block longer than the prepared maximum is refused before any node sees
/// it — which is what lets a node skip the clamp.
#[test]
#[should_panic(expected = "against a")]
fn a_block_past_max_block_is_refused() {
    let (_ed, mut exec, _) = counted_after_silence(Tail::Unknown);
    render(&mut exec, 65, 1);
}

/// Two const outputs (`0.5`, `0.25`) at the globals, committed and queued
/// without a block run.
fn queue_two_outputs(ed: &mut Editor) {
    ed.insert(SRC, "a", Unforkable(const_node(0.5)));
    ed.insert(FX, "b", Unforkable(const_node(0.25)));
    ed.spec_mut().topology.set_outputs(vec![
        Source::Node(OutPort { node: SRC, port: 0 }),
        Source::Node(OutPort { node: FX, port: 0 }),
    ]);
    ed.commit().expect("commits");
}

/// `render_applied` does not apply: a commit queued before it waits for the
/// next block, whatever its width; `process` applies it first.
#[test]
fn render_applied_leaves_a_queued_commit_for_the_next_block() {
    let (mut ed, mut exec) = Editor::new(prepare(64));
    queue_two_outputs(&mut ed);
    exec.render_applied(
        16,
        &Transport::default(),
        &tutti_graph::TransportChanges::NONE,
        &[],
        &mut [],
    );
    assert!(exec.plan().is_none(), "still queued");
    let out = render(&mut exec, 16, 2);
    assert!(out[0].iter().all(|&x| x == 0.5));
    assert!(out[1].iter().all(|&x| x == 0.25));
}

/// Fewer slices than the plan has outputs is a caller bug: a debug build
/// asserts, a release build renders silence into what it was given and runs
/// no node. Neither panics on an index.
#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "for a plan with 0 global inputs and 2 outputs")]
fn too_few_output_slices_assert_in_debug() {
    let (mut ed, mut exec) = Editor::new(prepare(64));
    queue_two_outputs(&mut ed);
    render(&mut exec, 16, 1);
}

#[test]
#[cfg(not(debug_assertions))]
fn too_few_output_slices_render_silence_in_release() {
    let (mut ed, mut exec) = Editor::new(prepare(64));
    queue_two_outputs(&mut ed);
    let mut bufs = vec![vec![7.0f32; 16]; 1];
    let mut refs: Vec<&mut [f32]> = bufs.iter_mut().map(Vec::as_mut_slice).collect();
    exec.process(16, &Transport::default(), &[], &mut refs);
    assert!(bufs[0].iter().all(|&x| x == 0.0));
}

/// A builder that hands back a typed control: the replacement for
/// `node_as::<T>`.
struct GainBuilder;
struct GainNode(Param<Amplitude>);

impl IntoNode for GainBuilder {
    type Controls = Param<Amplitude>;
    fn into_parts(self) -> NodeParts<Self::Controls> {
        let p = Param::new(Amplitude(1.0));
        NodeParts {
            node: Box::new(GainNode(p.handle())),
            fork: None,
            controls: p,
        }
    }
}

impl Node for GainNode {
    fn shape(&self) -> Shape {
        Shape::through_unassigned(ChannelLayout::MONO).with_in_place()
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        // Read once per block, as the RT rules ask.
        let g = self.0.load().0;
        io.channel(0).map(|x| x * g);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// `Editor::insert` returns the builder's typed controls, and writing them
/// reaches the running unit.
#[test]
fn insert_returns_typed_controls() {
    let (mut ed, mut exec) = Editor::new(prepare(8));
    ed.insert(
        SRC,
        "const",
        Unforkable(TestNode::new(Kind::Const {
            value: 2.0,
            width: 1,
        })),
    );
    let gain = ed.insert(FX, "gain", GainBuilder);
    wire(&mut ed, FX, SRC);
    ed.spec_mut()
        .topology
        .set_outputs(vec![Source::Node(OutPort { node: FX, port: 0 })]);
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    assert_eq!(render(&mut exec, 8, 1)[0][0], 2.0);
    gain.store(Amplitude(0.25));
    assert_eq!(render(&mut exec, 8, 1)[0][0], 0.5);
}

fn const_node(value: f32) -> TestNode {
    TestNode::new(Kind::Const { value, width: 1 })
}

/// Up to `QUEUE_CAPACITY` commits may be out; one more is `Backpressure`,
/// returned **before** anything is compiled or the plan advances. Once the
/// executor runs a block, every queued commit is applied in the order sent
/// and the next commit goes through.
#[test]
fn commits_queue_up_to_capacity_then_backpressure_without_advancing() {
    let (mut ed, mut exec) = Editor::new(prepare(8));
    ed.spec_mut()
        .topology
        .set_outputs(vec![Source::Node(OutPort { node: SRC, port: 0 })]);
    for i in 0..QUEUE_CAPACITY {
        ed.insert(SRC, "c", Unforkable(const_node(i as f32 + 1.0)));
        ed.commit().expect("room in the queue");
    }
    assert_eq!(ed.in_flight(), QUEUE_CAPACITY);
    let base = std::sync::Arc::clone(ed.base().unwrap());
    ed.insert(SRC, "c", Unforkable(const_node(99.0)));
    assert_eq!(ed.commit(), Err(CommitError::Backpressure));
    assert!(
        std::sync::Arc::ptr_eq(ed.base().unwrap(), &base),
        "a refused commit does not advance the plan"
    );
    assert!(
        exec.plan().is_none(),
        "nothing applied until the executor runs"
    );

    assert_eq!(
        render(&mut exec, 8, 1)[0][0],
        QUEUE_CAPACITY as f32,
        "the last one sent"
    );
    // Every generation but the last was retired, in the order sent.
    let retired = ed.collect();
    assert_eq!(retired, vec![SRC; QUEUE_CAPACITY - 1]);
    assert_eq!(ed.in_flight(), 0);
    ed.commit().expect("room again");
    assert_eq!(render(&mut exec, 8, 1)[0][0], 99.0);
}

/// Commits apply in the order they were sent, each on top of the last — a
/// linear chain — even when several are queued before a block runs.
#[test]
fn queued_commits_apply_in_fifo_order() {
    let (mut ed, mut exec) = Editor::new(prepare(8));
    ed.insert(SRC, "c", Unforkable(const_node(1.0)));
    ed.spec_mut()
        .topology
        .set_outputs(vec![Source::Node(OutPort { node: SRC, port: 0 })]);
    ed.commit().unwrap();
    // Second: a gain after it. Third: the gain changed. Each is compiled on
    // the one before, none of which has run.
    ed.insert(
        FX,
        "g",
        Unforkable(TestNode::new(Kind::Gain {
            gain: 0.5,
            width: 1,
        })),
    );
    wire(&mut ed, FX, SRC);
    ed.spec_mut()
        .topology
        .set_outputs(vec![Source::Node(OutPort { node: FX, port: 0 })]);
    ed.commit().unwrap();
    ed.insert(
        FX,
        "g",
        Unforkable(TestNode::new(Kind::Gain {
            gain: 0.25,
            width: 1,
        })),
    );
    ed.commit().unwrap();
    assert_eq!(render(&mut exec, 8, 1)[0][0], 0.25);
    assert_eq!(ed.collect(), vec![FX], "only the first gain was retired");
}

/// Commits in flight run in order: c1 applied; c2 (inserts node 2) in
/// flight; c3 (inserts node 3) sent behind it. The caller never holds a
/// commit, so none can be dropped unapplied: c2 and c3 queue, run in order,
/// and the next commit applies.
#[test]
fn probe_d_can_no_longer_be_expressed_and_its_interleaving_works() {
    let (a, b, c) = (NodeKey(1), NodeKey(2), NodeKey(3));
    let (mut ed, mut exec) = Editor::new(prepare(8));
    ed.insert(a, "a", Unforkable(const_node(1.0)));
    ed.spec_mut()
        .topology
        .set_outputs(vec![Source::Node(OutPort { node: a, port: 0 })]);
    ed.commit().unwrap(); // c1
    render(&mut exec, 8, 1); // applied
    ed.insert(b, "b", Unforkable(const_node(2.0)));
    ed.commit().unwrap(); // c2, in flight
    ed.insert(c, "c", Unforkable(const_node(3.0)));
    ed.commit().unwrap(); // c3, in flight behind it
    ed.spec_mut().topology.set_outputs(vec![
        Source::Node(OutPort { node: a, port: 0 }),
        Source::Node(OutPort { node: b, port: 0 }),
        Source::Node(OutPort { node: c, port: 0 }),
    ]);
    ed.commit()
        .expect("the next commit compiles against c3 and needs no lost unit");
    let out = render(&mut exec, 8, 3);
    assert_eq!([out[0][0], out[1][0], out[2][0]], [1.0, 2.0, 3.0]);
    ed.collect();
    ed.insert(a, "a", Unforkable(const_node(4.0)));
    ed.commit().expect("and later commits keep working");
    assert_eq!(render(&mut exec, 8, 3)[0][0], 4.0);
}

/// Nothing a commit carries is freed while the executor applies or renders:
/// the whole exchange runs under the audio-thread marker, and the old plan,
/// units and state come back to the editor to be freed.
#[test]
fn applying_frees_nothing_on_the_audio_thread() {
    let (mut ed, mut exec) = Editor::new(prepare(8));
    ed.insert(SRC, "c", Unforkable(const_node(1.0)));
    ed.insert(FX, "f", Unforkable(const_node(0.0)));
    ed.spec_mut()
        .topology
        .set_outputs(vec![Source::Node(OutPort { node: SRC, port: 0 })]);
    ed.commit().unwrap();
    let mut back = Vec::new();
    for v in 2..6 {
        render(&mut exec, 8, 1);
        back.extend(ed.collect());
        // Every commit replaces SRC; the first also removes FX outright, so
        // both the replace path and the retire path run under the marker.
        ed.insert(SRC, "c", Unforkable(const_node(v as f32)));
        if v == 2 {
            ed.remove(FX);
        }
        ed.commit().unwrap();
    }
    // `process` applies under the marker; any unit, commit or plan freed
    // there would panic in this debug build.
    render(&mut exec, 8, 1);
    back.extend(ed.collect());
    back.sort();
    let mut want = vec![SRC; 4];
    want.push(FX);
    want.sort();
    assert_eq!(back, want, "every replaced and removed unit came back");
}

/// Re-inserting at a key is a new generation: the unit is replaced, the old
/// one comes back to the control side, and removing then re-adding never
/// revives the old identity.
#[test]
fn reinserting_a_key_replaces_its_unit() {
    let (mut ed, mut exec) = Editor::new(prepare(8));
    ed.insert(
        SRC,
        "c",
        Unforkable(TestNode::new(Kind::Const {
            value: 1.0,
            width: 1,
        })),
    );
    ed.spec_mut()
        .topology
        .set_outputs(vec![Source::Node(OutPort { node: SRC, port: 0 })]);
    ed.commit().unwrap();
    exec.apply_pending();
    assert!(ed.collect().is_empty());
    assert_eq!(render(&mut exec, 8, 1)[0][0], 1.0);

    ed.insert(
        SRC,
        "c",
        Unforkable(TestNode::new(Kind::Const {
            value: 3.0,
            width: 1,
        })),
    );
    ed.commit().unwrap();
    exec.apply_pending();
    assert_eq!(ed.collect(), vec![SRC], "the old unit came back");
    assert_eq!(render(&mut exec, 8, 1)[0][0], 3.0);

    ed.remove(SRC);
    ed.insert(
        SRC,
        "c",
        Unforkable(TestNode::new(Kind::Const {
            value: 5.0,
            width: 1,
        })),
    );
    ed.spec_mut()
        .topology
        .set_outputs(vec![Source::Node(OutPort { node: SRC, port: 0 })]);
    ed.commit().unwrap();
    exec.apply_pending();
    assert_eq!(ed.collect(), vec![SRC]);
    assert_eq!(render(&mut exec, 8, 1)[0][0], 5.0);
}

/// A synth: a note-on starts a constant output after `attack_delay` frames
/// of silence, a note-off starts a linear release of `release` frames. It is
/// honest about the difference the executor cares about: silent *while a
/// note is held* is `Status::Silent` (busy); silent with nothing held and
/// nothing releasing is `Status::Idle` (park me).
struct Synth {
    release: usize,
    attack_delay: usize,
    level: f32,
    held: bool,
    wait: usize,
    releasing: Option<usize>,
    calls: Arc<AtomicUsize>,
}

impl Synth {
    fn new(release: usize, attack_delay: usize, calls: &Arc<AtomicUsize>) -> Self {
        Self {
            release,
            attack_delay,
            level: 0.0,
            held: false,
            wait: 0,
            releasing: None,
            calls: Arc::clone(calls),
        }
    }
}

impl Node for Synth {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
            .with_events(1, 0)
            .with_tail(Tail::Finite(Samples(self.release)))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let evs = io.events(0);
        let n = io.frames();
        let mut next = 0;
        for i in 0..n {
            while next < evs.len() && evs[next].offset.index() == i {
                if evs[next].is_note_off() {
                    self.held = false;
                    self.releasing = Some(self.release);
                } else {
                    self.held = true;
                    self.wait = self.attack_delay;
                    self.releasing = None;
                }
                next += 1;
            }
            if self.held {
                if self.wait > 0 {
                    self.wait -= 1;
                    self.level = 0.0;
                } else {
                    self.level = 1.0;
                }
            } else if let Some(left) = self.releasing.as_mut() {
                *left = left.saturating_sub(1);
                self.level = *left as f32 / self.release as f32;
                if *left == 0 {
                    self.releasing = None;
                }
            }
            io.output(0)[i] = self.level;
        }
        match (self.level == 0.0, self.held || self.releasing.is_some()) {
            (true, false) => Status::Idle,
            (true, true) => Status::Silent,
            (false, _) => Status::Modified,
        }
    }
    fn reset(&mut self) {}
}

/// Sends one event at a chosen frame.
struct OneShot {
    at: u64,
    words: [u32; 4],
}

impl Node for OneShot {
    fn shape(&self) -> Shape {
        Shape::EMPTY.with_events(0, 1)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        if let Some(at) = cx.env.offset_of(tutti_types::Frame(self.at)) {
            let _ = io
                .event_out(0)
                .push(tutti_graph::Event::midi(at, Ump(self.words)));
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

/// A held note on a finite-tail synth keeps sounding — its event input is
/// quiet for as long as the note is held, and that is not an idle node — and
/// once released and `Idle` it is skipped.
#[test]
fn a_held_note_keeps_sounding_and_a_released_one_is_skipped() {
    let calls = Arc::new(AtomicUsize::new(0));
    let out = run_synth(Synth::new(40, 0, &calls), 5, 1000, 60);
    assert!(
        out[6..1000].iter().all(|&x| x == 1.0),
        "the held note sounds from the note-on to the note-off"
    );
    assert!(out[1041..].iter().all(|&x| x == 0.0), "released and silent");
    let while_held = 1000 / 32 + 1;
    let total = calls.load(Ordering::Relaxed);
    assert!(total >= while_held, "called on every held block");
    assert!(
        total < 60,
        "and skipped once released and idle ({total} calls of 60 blocks)"
    );
}

/// An honest `Silent` does not mean parked forever: a synth whose attack
/// starts 200 frames after its note-on is silent, with quiet inputs, well past
/// its 40-frame tail — and must still be called, because it is not idle.
#[test]
fn a_silent_but_busy_node_is_not_parked() {
    let calls = Arc::new(AtomicUsize::new(0));
    let out = run_synth(Synth::new(40, 200, &calls), 5, 1000, 40);
    assert!(out[5..205].iter().all(|&x| x == 0.0), "the delayed attack");
    assert!(
        out[206..1000].iter().all(|&x| x == 1.0),
        "then the note sounds"
    );
}

/// Drive `synth` with a note-on at `on` and a note-off at `off` for `blocks`
/// blocks of 32.
fn run_synth(synth: Synth, on_at: u64, off_at: u64, blocks: usize) -> Vec<f32> {
    let (on, off, key) = (NodeKey(1), NodeKey(2), NodeKey(3));
    let (mut ed, mut exec) = Editor::new(prepare(32));
    ed.insert(
        on,
        "on",
        Unforkable(OneShot {
            at: on_at,
            words: m1_note_on(MidiChannel::FIRST, NoteNumber::of(60), U7::of(100)).0,
        }),
    );
    ed.insert(
        off,
        "off",
        Unforkable(OneShot {
            at: off_at,
            words: m1_note_off(MidiChannel::FIRST, NoteNumber::of(60), U7::of(0)).0,
        }),
    );
    ed.insert(key, "synth", Unforkable(synth));
    let spec = ed.spec_mut();
    for from in [on, off] {
        spec.connect_events(
            tutti_graph::EventIn { node: key, port: 0 },
            tutti_graph::EventEdge::Direct(tutti_graph::EventOut {
                node: from,
                port: 0,
            }),
        );
    }
    spec.topology
        .set_outputs(vec![Source::Node(OutPort { node: key, port: 0 })]);
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    let mut out = Vec::new();
    for _ in 0..blocks {
        out.extend(render(&mut exec, 32, 1).remove(0));
    }
    out
}

/// A plan compiled for another `Prepare` is refused when applied — the units
/// in it were prepared for a `MaxBlock` this executor does not keep.
#[test]
#[should_panic(expected = "another Prepare")]
fn applying_refuses_a_plan_for_another_prepare() {
    let (mut ed, mut exec) = Editor::new(prepare(64));
    ed.insert(SRC, "c", Unforkable(const_node(1.0)));
    let valid = ed.spec().validate().unwrap();
    let (plan, delta) = tutti_graph::compile(&valid, ed.shapes(), &prepare(128), None).unwrap();
    let units = common::units_for(
        &[(
            SRC,
            Kind::Const {
                value: 1.0,
                width: 1,
            },
        )]
        .into(),
        [SRC],
    );
    ed.package(plan, delta, units).unwrap();
    exec.apply_pending();
}

/// A merge never drops past one slot's capacity. The probe: capacity 32, two sources each sending 30 note-offs
/// on one frame into one port — 60 must arrive, none dropped.
#[test]
fn a_merge_holds_all_its_inputs_so_no_note_off_is_lost() {
    struct Offs;
    impl Node for Offs {
        fn shape(&self) -> Shape {
            Shape::EMPTY.with_events(0, 1)
        }
        fn prepare(&mut self, _: &Prepare) {}
        fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
            if cx.env.frame == tutti_types::Frame::ZERO {
                for n in 0..30 {
                    io.event_out(0)
                        .push(tutti_graph::Event::midi(
                            tutti_graph::Offset::ZERO,
                            m1_note_off(MidiChannel::FIRST, NoteNumber::of(n), U7::of(0)),
                        ))
                        .expect("30 fit in 32");
                }
            }
            Status::Idle
        }
        fn reset(&mut self) {}
    }
    struct Count(Arc<AtomicUsize>);
    impl Node for Count {
        fn shape(&self) -> Shape {
            Shape::EMPTY.with_events(1, 0)
        }
        fn prepare(&mut self, _: &Prepare) {}
        fn process(&mut self, _: &Cx<'_>, io: Io<'_>) -> Status {
            let offs = io.events(0).iter().filter(|e| e.is_note_off()).count();
            self.0.fetch_add(offs, Ordering::Relaxed);
            Status::Idle
        }
        fn reset(&mut self) {}
    }
    let seen = Arc::new(AtomicUsize::new(0));
    let (mut ed, mut exec) = Editor::with_event_capacity(prepare(8), 32);
    ed.insert(NodeKey(1), "a", Unforkable(Offs));
    ed.insert(NodeKey(2), "b", Unforkable(Offs));
    ed.insert(NodeKey(3), "count", Unforkable(Count(Arc::clone(&seen))));
    for from in [1, 2] {
        ed.spec_mut().connect_events(
            tutti_graph::EventIn {
                node: NodeKey(3),
                port: 0,
            },
            tutti_graph::EventEdge::Direct(tutti_graph::EventOut {
                node: NodeKey(from),
                port: 0,
            }),
        );
    }
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    render(&mut exec, 8, 0);
    assert_eq!(seen.load(Ordering::Relaxed), 60);
    assert_eq!(exec.dropped_event_count(), 0);
}

/// A node fed **out of band**: it adds `level` (a shared cell a control
/// thread writes, as a synth's own MIDI queue or a mic's ring feeds it) to
/// its audio inputs, and declares `Tail::None` — honest for what its
/// *inputs* can do, which is exactly why the silence skip would park it.
/// `claims_silence` makes it report the silent channels it wrote
/// (`Status::Masked`); otherwise it makes no claim (`Status::Modified`).
struct OutOfBand {
    inputs: u16,
    outputs: u16,
    claims_silence: bool,
    level: Arc<Param<Amplitude>>,
    calls: Arc<AtomicUsize>,
}

impl OutOfBand {
    fn new(inputs: u16, outputs: u16, claims_silence: bool) -> Self {
        Self {
            inputs,
            outputs,
            claims_silence,
            level: Arc::new(Param::new(Amplitude::new(0.0))),
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl Node for OutOfBand {
    fn shape(&self) -> Shape {
        Shape::buses(
            tutti_graph::Buses::unassigned(ChannelLayout::from_count(self.inputs)),
            tutti_graph::Buses::unassigned(ChannelLayout::from_count(self.outputs)),
        )
        .with_tail(Tail::None)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let level = self.level.load().get();
        let (ins, mut outs) = io.split();
        let mut silent = 0u64;
        for c in 0..usize::from(self.outputs) {
            let out = outs.get(c);
            for (i, o) in out.iter_mut().enumerate() {
                let x = if c < usize::from(self.inputs) {
                    ins.get(c)[i]
                } else {
                    0.0
                };
                *o = level + x;
            }
            if out.iter().all(|&x| x == 0.0) {
                silent |= 1 << c;
            }
        }
        if self.claims_silence {
            Status::Masked {
                silent: SilenceMask(silent),
                constant: tutti_graph::ConstantMask::NONE,
            }
        } else {
            Status::Modified
        }
    }
    fn reset(&mut self) {}
}

/// `node` at key 1, its inputs wired to silence (`Source::Zero`, which the
/// executor knows is silent), its first output (if any) the graph's.
fn out_of_band_graph(node: OutOfBand) -> Executor {
    let (mut ed, mut exec) = Editor::new(prepare(64));
    let key = NodeKey(1);
    let (inputs, outputs) = (node.inputs, node.outputs);
    ed.insert(key, "oob", Unforkable(node));
    // The inputs are left unwired: an unwired port reads silence.
    let _ = inputs;
    if outputs > 0 {
        ed.spec_mut()
            .topology
            .set_outputs(vec![Source::Node(OutPort { node: key, port: 0 })]);
    }
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    exec
}

/// **A node that makes no silence claim is heard after any amount of
/// silence**, fed out of band: silent for ten blocks, then its cell is
/// written, and the next block carries the level. Run for a node with an
/// audio input wired to silence (the live hazard: a plugin instrument with a
/// sidechain) and for a 0-input source (a mic monitor).
#[test]
fn a_node_that_claims_no_silence_is_heard_after_silence() {
    for inputs in [1u16, 0] {
        let node = OutOfBand::new(inputs, 1, false);
        let (level, calls) = (Arc::clone(&node.level), Arc::clone(&node.calls));
        let mut exec = out_of_band_graph(node);
        let mut out = vec![0.0f32; 64];
        for _ in 0..10 {
            exec.process(64, &Transport::default(), &[], &mut [&mut out[..]]);
            assert!(out.iter().all(|&x| x == 0.0));
        }
        level.store(Amplitude::new(0.5));
        exec.process(64, &Transport::default(), &[], &mut [&mut out[..]]);
        assert!(
            out.iter().all(|&x| x == 0.5),
            "{inputs}-input node: the out-of-band level must reach the output \
             on the next block; got {:?}",
            &out[..4]
        );
        assert_eq!(
            calls.load(Ordering::Relaxed),
            11,
            "{inputs}-input node: called every block"
        );
    }
}

/// The same node, **claiming** the silence it wrote, is parked — which is
/// what the claim means. Pins that the claim is the switch, so the test
/// above is not passing because nothing is ever skipped.
#[test]
fn a_node_that_claims_silence_is_parked() {
    let node = OutOfBand::new(1, 1, true);
    let (level, calls) = (Arc::clone(&node.level), Arc::clone(&node.calls));
    let mut exec = out_of_band_graph(node);
    let mut out = vec![0.0f32; 64];
    for _ in 0..10 {
        exec.process(64, &Transport::default(), &[], &mut [&mut out[..]]);
    }
    level.store(Amplitude::new(0.5));
    exec.process(64, &Transport::default(), &[], &mut [&mut out[..]]);
    assert_eq!(calls.load(Ordering::Relaxed), 1, "called once, then parked");
    assert!(out.iter().all(|&x| x == 0.0), "and so never heard again");
}

/// A node with **no outputs** is never parked, even claiming silence with a
/// silent input and `Tail::None`: it is a sink, called for its side effects
/// (a meter, a tap), and "every output silent" is vacuous for it.
#[test]
fn a_sink_is_never_parked() {
    let node = OutOfBand::new(1, 0, true);
    let calls = Arc::clone(&node.calls);
    let mut exec = out_of_band_graph(node);
    for _ in 0..10 {
        exec.process(64, &Transport::default(), &[], &mut []);
    }
    assert_eq!(calls.load(Ordering::Relaxed), 10, "a sink runs every block");
}

/// **A commit that changes nothing sends nothing**, and every kind of change
/// still sends: an edit through `spec_mut`, a new unit, a latency set by
/// hand. The unchanged commits leave `in_flight` and the base plan where
/// they were, so a host committing every frame compiles only when it edits.
#[test]
fn an_unchanged_commit_sends_nothing_and_a_change_still_sends() {
    let (mut ed, mut exec) = Editor::new(prepare(8));
    ed.insert(SRC, "c", Unforkable(const_node(1.0)));
    ed.spec_mut()
        .topology
        .set_outputs(vec![Source::Node(OutPort { node: SRC, port: 0 })]);
    ed.commit().expect("commits");
    assert_eq!(ed.in_flight(), 1);
    let base = Arc::clone(ed.base().expect("sent"));

    ed.commit().expect("an unchanged commit is Ok");
    assert_eq!(ed.in_flight(), 1, "an unchanged commit sends nothing");
    assert!(Arc::ptr_eq(ed.base().unwrap(), &base), "nor moves the base");

    // Drained, and still nothing to send.
    exec.apply_pending();
    ed.collect();
    ed.commit().expect("commits");
    assert_eq!(
        ed.in_flight(),
        0,
        "nothing changed since the executor took it"
    );

    // An edit through `spec_mut` sends.
    ed.spec_mut().topology.outputs.push(Vec::new());
    ed.commit().expect("commits");
    assert_eq!(ed.in_flight(), 1, "a spec edit sends");

    // A new unit at the same key sends.
    ed.insert(SRC, "c", Unforkable(const_node(2.0)));
    ed.commit().expect("commits");
    assert_eq!(ed.in_flight(), 2, "a pending unit sends");

    // A latency set by hand sends.
    ed.set_latency(SRC, tutti_types::Latency::new(Samples(3)))
        .expect("a node");
    ed.commit().expect("commits");
    assert_eq!(ed.in_flight(), 3, "a latency change sends");
    ed.commit().expect("commits");
    assert_eq!(ed.in_flight(), 3, "and once sent, is unchanged again");
}

/// An input the plan reads, with no slice for it: the same caller bug as a
/// short `outputs`, caught before `global_in` indexes `inputs`.
#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "for a plan with 1 global inputs and 1 outputs")]
fn too_few_input_slices_assert_in_debug() {
    let (mut ed, mut exec) = Editor::new(prepare(64));
    ed.insert(SRC, "a", Unforkable(const_node(0.5)));
    ed.spec_mut().topology.inputs = tutti_types::ChannelLayout::MONO;
    ed.spec_mut().topology.set_outputs(vec![Source::Global(0)]);
    ed.commit().expect("commits");
    render(&mut exec, 16, 1);
}

#[test]
#[cfg(not(debug_assertions))]
fn too_few_input_slices_render_silence_in_release() {
    let (mut ed, mut exec) = Editor::new(prepare(64));
    ed.insert(SRC, "a", Unforkable(const_node(0.5)));
    ed.spec_mut().topology.inputs = tutti_types::ChannelLayout::MONO;
    ed.spec_mut().topology.set_outputs(vec![Source::Global(0)]);
    ed.commit().expect("commits");
    let out = render(&mut exec, 16, 1);
    assert!(out[0].iter().all(|&x| x == 0.0));
}

/// A [`common::Ringer`] fed by a silent constant, in one-frame blocks' reach:
/// the calls it gets are the quiet frames the skip waited. Returns the
/// report word the test can change between blocks.
fn ringer_after_silence(
    ringer: common::Ringer,
) -> (
    Editor,
    Executor,
    Arc<AtomicUsize>,
    Arc<std::sync::atomic::AtomicU64>,
) {
    let (calls, report) = (Arc::clone(&ringer.calls), Arc::clone(&ringer.report));
    let width = ringer.width;
    let (mut ed, mut exec) = Editor::new(prepare(64));
    ed.insert(
        SRC,
        "const",
        Unforkable(TestNode::new(Kind::Const { value: 0.0, width })),
    );
    ed.insert(FX, "ringer", Unforkable(ringer));
    for port in 0..width as u16 {
        ed.spec_mut().topology.edges.insert(
            InPort { node: FX, port },
            vec![Edge::Direct(Source::Node(OutPort { node: SRC, port }))],
        );
    }
    ed.spec_mut()
        .topology
        .set_outputs(vec![Source::Node(OutPort { node: FX, port: 0 })]);
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    (ed, exec, calls, report)
}

/// Calls a [`common::Ringer`] gets over `frames` quiet one-frame blocks.
fn ringer_calls(ringer: common::Ringer, frames: usize) -> usize {
    let (_ed, mut exec, calls, _report) = ringer_after_silence(ringer);
    for _ in 0..frames {
        render(&mut exec, 1, 1);
    }
    calls.load(Ordering::Relaxed)
}

/// A node that reports a tail shorter than its ceiling is skipped after the
/// reported one: the ceiling `Shape::tail` is only the upper bound. Latency
/// still comes first (`D + reported`), `Unknown` reported says nothing, and
/// an `Unbounded` ceiling narrows to a finite report.
#[test]
fn a_reported_tail_shorter_than_the_ceiling_is_what_the_skip_waits() {
    use common::{REPORT_UNKNOWN, Ringer};
    let ceiling = Tail::Finite(Samples(100));
    assert_eq!(ringer_calls(Ringer::new(ceiling, 5, 10), 300), 10);
    assert_eq!(
        ringer_calls(Ringer::new(ceiling, 5, 10).with_latency(Samples(7)), 300),
        7 + 10,
        "latency, then the reported tail"
    );
    assert_eq!(
        ringer_calls(Ringer::new(ceiling, 5, 0), 300),
        1,
        "a reported `None` is skipped after the first call"
    );
    assert_eq!(
        ringer_calls(Ringer::new(ceiling, 5, REPORT_UNKNOWN), 300),
        100,
        "Unknown reported never narrows"
    );
    assert_eq!(
        ringer_calls(Ringer::new(Tail::Unbounded, 5, 25), 300),
        25,
        "an unbounded ceiling narrows to a finite report"
    );
}

/// A report above the ceiling is clamped to it, and `Unbounded` reported
/// under a finite ceiling is no more than the ceiling; `Unbounded` under an
/// `Unbounded` ceiling is never skipped.
#[test]
fn a_reported_tail_above_the_ceiling_is_clamped() {
    use common::{REPORT_UNBOUNDED, Ringer};
    let ceiling = Tail::Finite(Samples(20));
    assert_eq!(ringer_calls(Ringer::new(ceiling, 5, 500), 300), 20);
    assert_eq!(
        ringer_calls(Ringer::new(ceiling, 5, REPORT_UNBOUNDED), 300),
        20
    );
    assert_eq!(
        ringer_calls(Ringer::new(Tail::None, 5, 500), 300),
        1,
        "no report outlasts a ceiling of none"
    );
    assert_eq!(
        ringer_calls(Ringer::new(Tail::Unbounded, 5, REPORT_UNBOUNDED), 300),
        300,
        "an unbounded report is never skipped"
    );
}

/// A report holds for one call: a call that reports nothing restores the
/// ceiling, so a node cannot leave a stale short figure behind.
///
/// Five calls report 10, then the node stops reporting; the ceiling's 100
/// quiet frames are then what the skip waits.
#[test]
fn a_call_that_reports_nothing_restores_the_ceiling() {
    use common::{NO_REPORT, Ringer};
    let (_ed, mut exec, calls, report) =
        ringer_after_silence(Ringer::new(Tail::Finite(Samples(100)), 5, 10));
    for _ in 0..5 {
        render(&mut exec, 1, 1);
    }
    report.store(NO_REPORT, Ordering::Relaxed);
    for _ in 0..300 {
        render(&mut exec, 1, 1);
    }
    assert_eq!(calls.load(Ordering::Relaxed), 100);
}

/// A new unit starts at its ceiling: a node reporting a short tail, then
/// replaced by one that reports nothing, waits the new unit's ceiling, not
/// the old unit's report (the report is the unit's, and a replace is a new
/// unit).
///
/// The test pins the observable rule, not the individual resets.
#[test]
fn a_new_unit_starts_at_its_ceiling() {
    use common::{NO_REPORT, Ringer};
    let (mut ed, mut exec, _calls, _report) =
        ringer_after_silence(Ringer::new(Tail::Finite(Samples(100)), 5, 0));
    for _ in 0..10 {
        render(&mut exec, 1, 1);
    }
    let fresh = Ringer::new(Tail::Finite(Samples(100)), 5, NO_REPORT);
    let calls = Arc::clone(&fresh.calls);
    ed.insert(FX, "ringer", Unforkable(fresh));
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    for _ in 0..300 {
        render(&mut exec, 1, 1);
    }
    assert_eq!(calls.load(Ordering::Relaxed), 100);
}

/// The report reaches the unit through every form the executor calls a node
/// in: mono (the split form), stereo in and out (`Direct`), three channels
/// (the general audio form) and a node with an event output (the general
/// form, `Call::run`).
#[test]
fn a_report_reaches_every_call_form() {
    use common::Ringer;
    let ceiling = Tail::Finite(Samples(100));
    let ringer = |width: usize| Ringer::new(ceiling, 5, 10).wide(width);
    for (what, node) in [
        ("mono", ringer(1)),
        ("stereo (Direct)", ringer(2)),
        ("three channels (Audio)", ringer(3)),
        ("event output (General)", ringer(1).with_event_out()),
        ("stereo and event output", ringer(2).with_event_out()),
    ] {
        assert_eq!(ringer_calls(node, 300), 10, "{what}");
    }
}

/// An `Unknown` ceiling bounds nothing, so no report narrows it: a node that
/// declares no tail is never skipped.
#[test]
fn nothing_narrows_an_unknown_ceiling() {
    use common::Ringer;
    assert_eq!(ringer_calls(Ringer::new(Tail::Unknown, 5, 10), 300), 300);
}
