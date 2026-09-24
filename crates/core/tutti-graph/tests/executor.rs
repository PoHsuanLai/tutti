//! The executor's own behaviour: silence skipping, status handling, the
//! audio-thread mark, the block bound, and the editor's control-side
//! protocol (typed controls, generations, the commit queue and its back-pressure).

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use common::{prepare, Kind, TestNode};
use tutti_graph::{
    CommitError, Cx, Editor, Executor, IntoNode, Io, Node, Prepare, Shape, SilenceMask, Status,
    Transport, QUEUE_CAPACITY,
};
use tutti_types::graph::{Edge, InPort, OutPort, Source};
use tutti_types::{Amplitude, ChannelLayout, NodeKey, Param, Samples, Tail};

const SRC: NodeKey = NodeKey(1);
const FX: NodeKey = NodeKey(2);

fn wire(ed: &mut Editor, sink: NodeKey, from: NodeKey) {
    ed.spec_mut().topology.edges.insert(
        InPort {
            node: sink,
            port: 0,
        },
        Edge::Direct(Source::Node(OutPort {
            node: from,
            port: 0,
        })),
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
    let (mut ed, mut exec) = Editor::new(prepare(64));
    ed.insert(
        SRC,
        "const",
        TestNode::new(Kind::Const {
            value: 0.0,
            width: 1,
        }),
    );
    let calls = Arc::new(AtomicUsize::new(0));
    ed.insert(
        FX,
        "probe",
        TailProbe {
            tail,
            calls: Arc::clone(&calls),
        },
    );
    wire(&mut ed, FX, SRC);
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort { node: FX, port: 0 })];
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    (ed, exec, calls)
}

/// Passes its input through and counts calls. Honest about silence: a
/// silent input gives `Status::Silent`, which is what lets the skip trust it.
struct TailProbe {
    tail: Tail,
    calls: Arc<AtomicUsize>,
}

impl Node for TailProbe {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::MONO, ChannelLayout::MONO).with_tail(self.tail)
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
///
/// Mutation: in `tail_elapsed`, return `false` for `Tail::None` → the probe is
/// called every block → the first case fails. Return `true` for
/// `Tail::Unknown` → the last case fails.
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

/// A node that writes garbage and returns `Status::Silent` produces silence,
/// and its flag lets the next node be skipped.
///
/// Mutation: in `finish`, make `Status::Silent` only set the flags without
/// zero-filling → the garbage reaches the output → fails.
#[test]
fn status_silent_zeroes_and_flags() {
    struct Liar;
    impl Node for Liar {
        fn shape(&self) -> Shape {
            Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO)
        }
        fn prepare(&mut self, _: &Prepare) {}
        fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
            io.output(0).fill(123.0);
            Status::Silent
        }
        fn reset(&mut self) {}
    }
    let (mut ed, mut exec) = Editor::new(prepare(32));
    ed.insert(SRC, "liar", Liar);
    let calls = Arc::new(AtomicUsize::new(0));
    ed.insert(
        FX,
        "probe",
        TailProbe {
            tail: Tail::None,
            calls: Arc::clone(&calls),
        },
    );
    wire(&mut ed, FX, SRC);
    ed.spec_mut().topology.outputs = vec![
        Source::Node(OutPort { node: SRC, port: 0 }),
        Source::Node(OutPort { node: FX, port: 0 }),
    ];
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
///
/// Mutation: in `finish`, ignore the `Masked` silent mask (treat as
/// `Modified`) → the observer sees no silent channel → fails.
#[test]
fn masked_status_reaches_the_next_nodes_mask() {
    struct HalfSilent;
    impl Node for HalfSilent {
        fn shape(&self) -> Shape {
            Shape::audio(ChannelLayout::EMPTY, ChannelLayout::STEREO)
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
            Shape::audio(ChannelLayout::STEREO, ChannelLayout::MONO).with_tail(Tail::Unknown)
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
    ed.insert(SRC, "half", HalfSilent);
    ed.insert(FX, "observer", Observer(Arc::clone(&seen)));
    for port in 0..2 {
        ed.spec_mut().topology.edges.insert(
            InPort { node: FX, port },
            Edge::Direct(Source::Node(OutPort { node: SRC, port })),
        );
    }
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort { node: FX, port: 0 })];
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    render(&mut exec, 16, 1);
    assert_eq!(seen.load(Ordering::Relaxed), 0b01);
}

/// Dropping a `Retire` inside a node's `process` panics in a debug build —
/// the executor marks the audio thread for the whole block.
///
/// Mutation: delete `let _rt = AudioThread::enter();` from
/// `Executor::process` → the drop is silent → fails.
#[test]
#[cfg(debug_assertions)]
fn freeing_a_retire_inside_process_panics_in_debug() {
    struct Freer(Option<tutti_types::Retire<Vec<f32>>>);
    impl Node for Freer {
        fn shape(&self) -> Shape {
            Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO)
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
        Freer(Some(tutti_types::Retire::new(vec![0.0; 8]))),
    );
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort { node: SRC, port: 0 })];
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
///
/// Mutation: delete the `frames <= max` half of the assert in `process` →
/// `Io::new` still refuses (its own assert) — so the test pins that *some*
/// check stands between the caller and the node; delete both → fails.
#[test]
#[should_panic(expected = "against a")]
fn a_block_past_max_block_is_refused() {
    let (_ed, mut exec, _) = counted_after_silence(Tail::Unknown);
    render(&mut exec, 65, 1);
}

/// A builder that hands back a typed control: the replacement for
/// `node_as::<T>`.
struct GainBuilder;
struct GainNode(Param<Amplitude>);

impl IntoNode for GainBuilder {
    type Controls = Param<Amplitude>;
    fn into_node(self) -> (Box<dyn Node>, Self::Controls) {
        let p = Param::new(Amplitude(1.0));
        (Box::new(GainNode(p.handle())), p)
    }
}

impl Node for GainNode {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::MONO, ChannelLayout::MONO).with_in_place()
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
///
/// Mutation: have `GainBuilder::into_node` give the node its own fresh
/// `Param` → the write never reaches it → fails.
#[test]
fn insert_returns_typed_controls() {
    let (mut ed, mut exec) = Editor::new(prepare(8));
    ed.insert(
        SRC,
        "const",
        TestNode::new(Kind::Const {
            value: 2.0,
            width: 1,
        }),
    );
    let gain = ed.insert(FX, "gain", GainBuilder);
    wire(&mut ed, FX, SRC);
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort { node: FX, port: 0 })];
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
///
/// Mutation: in `Editor::commit`, advance `self.plan` before the capacity
/// check (or drop the check) → the refused commit changes the base, or the
/// push hits a full queue → fails.
#[test]
fn commits_queue_up_to_capacity_then_backpressure_without_advancing() {
    let (mut ed, mut exec) = Editor::new(prepare(8));
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort { node: SRC, port: 0 })];
    for i in 0..QUEUE_CAPACITY {
        ed.insert(SRC, "c", const_node(i as f32 + 1.0));
        ed.commit().expect("room in the queue");
    }
    assert_eq!(ed.in_flight(), QUEUE_CAPACITY);
    let base = std::sync::Arc::clone(ed.base().unwrap());
    ed.insert(SRC, "c", const_node(99.0));
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
///
/// Mutation: pop the queue in reverse (collect, then apply newest first) →
/// the final plan is the first one sent → fails.
#[test]
fn queued_commits_apply_in_fifo_order() {
    let (mut ed, mut exec) = Editor::new(prepare(8));
    ed.insert(SRC, "c", const_node(1.0));
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort { node: SRC, port: 0 })];
    ed.commit().unwrap();
    // Second: a gain after it. Third: the gain changed. Each is compiled on
    // the one before, none of which has run.
    ed.insert(
        FX,
        "g",
        TestNode::new(Kind::Gain {
            gain: 0.5,
            width: 1,
        }),
    );
    wire(&mut ed, FX, SRC);
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort { node: FX, port: 0 })];
    ed.commit().unwrap();
    ed.insert(
        FX,
        "g",
        TestNode::new(Kind::Gain {
            gain: 0.25,
            width: 1,
        }),
    );
    ed.commit().unwrap();
    assert_eq!(render(&mut exec, 8, 1)[0][0], 0.25);
    assert_eq!(ed.collect(), vec![FX], "only the first gain was retired");
}

/// Review probe d, which the old API allowed: c1 applied; c2 (inserts node
/// 2) in flight; c3 (inserts node 3) dropped unapplied; then every later
/// commit failed with `MissingUnit { 2 }`. A commit can no longer be dropped
/// — the caller never holds one — so the same interleaving, expressed in
/// the API that remains, just works: c2 and c3 queue, run in order, and the
/// next commit applies.
///
/// Mutation: in `Editor::send`, set `self.plan` to the plan the executor
/// last applied instead of the one just sent (a rebase) → the next commit's
/// delta re-inserts node 2, whose unit is gone → `MissingUnit` → fails.
#[test]
fn probe_d_can_no_longer_be_expressed_and_its_interleaving_works() {
    let (a, b, c) = (NodeKey(1), NodeKey(2), NodeKey(3));
    let (mut ed, mut exec) = Editor::new(prepare(8));
    ed.insert(a, "a", const_node(1.0));
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort { node: a, port: 0 })];
    ed.commit().unwrap(); // c1
    render(&mut exec, 8, 1); // applied
    ed.insert(b, "b", const_node(2.0));
    ed.commit().unwrap(); // c2, in flight
    ed.insert(c, "c", const_node(3.0));
    ed.commit().unwrap(); // c3, in flight behind it
    ed.spec_mut().topology.outputs = vec![
        Source::Node(OutPort { node: a, port: 0 }),
        Source::Node(OutPort { node: b, port: 0 }),
        Source::Node(OutPort { node: c, port: 0 }),
    ];
    ed.commit()
        .expect("the next commit compiles against c3 and needs no lost unit");
    let out = render(&mut exec, 8, 3);
    assert_eq!([out[0][0], out[1][0], out[2][0]], [1.0, 2.0, 3.0]);
    ed.collect();
    ed.insert(a, "a", const_node(4.0));
    ed.commit().expect("and later commits keep working");
    assert_eq!(render(&mut exec, 8, 3)[0][0], 4.0);
}

/// Nothing a commit carries is freed while the executor applies or renders:
/// the whole exchange runs under the audio-thread marker, and the old plan,
/// units and state come back to the editor to be freed.
///
/// Mutation: drop a retired or a replaced unit inside applying instead of
/// keeping it in the box → the unit's `Drop` check fires under the marker →
/// panics → fails (each path mutated separately).
#[test]
fn applying_frees_nothing_on_the_audio_thread() {
    let (mut ed, mut exec) = Editor::new(prepare(8));
    ed.insert(SRC, "c", const_node(1.0));
    ed.insert(FX, "f", const_node(0.0));
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort { node: SRC, port: 0 })];
    ed.commit().unwrap();
    let mut back = Vec::new();
    for v in 2..6 {
        render(&mut exec, 8, 1);
        back.extend(ed.collect());
        // Every commit replaces SRC; the first also removes FX outright, so
        // both the replace path and the retire path run under the marker.
        ed.insert(SRC, "c", const_node(v as f32));
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
///
/// Mutation: in `Editor::insert`, reuse generation 0 every time → the second
/// insert compiles to no delta, the old unit keeps running → fails.
#[test]
fn reinserting_a_key_replaces_its_unit() {
    let (mut ed, mut exec) = Editor::new(prepare(8));
    ed.insert(
        SRC,
        "c",
        TestNode::new(Kind::Const {
            value: 1.0,
            width: 1,
        }),
    );
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort { node: SRC, port: 0 })];
    ed.commit().unwrap();
    exec.apply_pending();
    assert!(ed.collect().is_empty());
    assert_eq!(render(&mut exec, 8, 1)[0][0], 1.0);

    ed.insert(
        SRC,
        "c",
        TestNode::new(Kind::Const {
            value: 3.0,
            width: 1,
        }),
    );
    ed.commit().unwrap();
    exec.apply_pending();
    assert_eq!(ed.collect(), vec![SRC], "the old unit came back");
    assert_eq!(render(&mut exec, 8, 1)[0][0], 3.0);

    ed.remove(SRC);
    ed.insert(
        SRC,
        "c",
        TestNode::new(Kind::Const {
            value: 5.0,
            width: 1,
        }),
    );
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort { node: SRC, port: 0 })];
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
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO)
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
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY).with_events(0, 1)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        if let Some(at) = cx.env.offset_of(tutti_types::Frame(self.at)) {
            let _ = io
                .event_out(0)
                .push(tutti_graph::Event::midi(at, self.words));
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

/// A held note on a finite-tail synth keeps sounding — its event input is
/// quiet for as long as the note is held, and that is not an idle node — and
/// once released and `Idle` it is skipped.
///
/// Mutation: for nodes with event inputs, skip on `last_quiet` + tail (the
/// audio-node rule) instead of `last_idle` → the held note's synth is skipped
/// once its tail has run under quiet inputs → silent held note → fails.
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

/// "An honest `Silent` means parked forever" (review): a synth whose attack
/// starts 200 frames after its note-on is silent, with quiet inputs, well past
/// its 40-frame tail — and must still be called, because it is not idle.
///
/// Mutation: treat `Status::Silent` like `Status::Idle` for the skip (set
/// `last_idle` for either) → the synth is parked during the delay and the
/// note never sounds → fails.
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
        OneShot {
            at: on_at,
            words: [0x2090_3c64, 0, 0, 0],
        },
    );
    ed.insert(
        off,
        "off",
        OneShot {
            at: off_at,
            words: [0x2080_3c00, 0, 0, 0],
        },
    );
    ed.insert(key, "synth", synth);
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
    spec.topology.outputs = vec![Source::Node(OutPort { node: key, port: 0 })];
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
///
/// Mutation: delete the `assert_eq!(plan.prepare, self.prepare)` in
/// `Executor::apply` → the foreign plan installs → fails.
#[test]
#[should_panic(expected = "another Prepare")]
fn applying_refuses_a_plan_for_another_prepare() {
    let (mut ed, mut exec) = Editor::new(prepare(64));
    ed.insert(SRC, "c", const_node(1.0));
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

/// Review: a merge used to drop past one slot's capacity with no note-off
/// exception. The probe: capacity 32, two sources each sending 30 note-offs
/// on one frame into one port — 60 must arrive, none dropped.
///
/// Mutation: size merged slots at one capacity (all `event_slot_weight` 1)
/// → 28 note-offs are lost → fails.
#[test]
fn a_merge_holds_all_its_inputs_so_no_note_off_is_lost() {
    struct Offs;
    impl Node for Offs {
        fn shape(&self) -> Shape {
            Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY).with_events(0, 1)
        }
        fn prepare(&mut self, _: &Prepare) {}
        fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
            if cx.env.frame == tutti_types::Frame::ZERO {
                for n in 0..30 {
                    io.event_out(0)
                        .push(tutti_graph::Event::midi(
                            tutti_graph::Offset::ZERO,
                            [0x2080_0000 | (n << 8), 0, 0, 0],
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
            Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY).with_events(1, 0)
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
    ed.insert(NodeKey(1), "a", Offs);
    ed.insert(NodeKey(2), "b", Offs);
    ed.insert(NodeKey(3), "count", Count(Arc::clone(&seen)));
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
    assert_eq!(exec.dropped_events(), 0);
}
