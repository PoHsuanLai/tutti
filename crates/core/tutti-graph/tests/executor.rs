//! The executor's own behaviour: silence skipping, status handling, the
//! audio-thread mark, the block bound, and the editor's control-side
//! protocol (typed controls, generations, back-pressure, reclaim).

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use common::{prepare, Kind, TestNode};
use tutti_graph::{
    CommitError, Cx, Editor, Executor, IntoNode, Io, Node, Prepare, Shape, SilenceMask, Status,
    Transport, MAX_IN_FLIGHT,
};
use tutti_types::graph::{Edge, InPort, OutPort, Source};
use tutti_types::{Amplitude, ChannelLayout, NodeKey, Param, Retire, Samples, Tail};

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
    let done = exec.apply(ed.commit().expect("commits"));
    ed.reclaim(done).expect("our own applied box");
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
    let done = exec.apply(ed.commit().unwrap());
    ed.reclaim(done).expect("our own applied box");
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
    let done = exec.apply(ed.commit().unwrap());
    ed.reclaim(done).expect("our own applied box");
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
    struct Freer(Option<Retire<Vec<f32>>>);
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
    ed.insert(SRC, "freer", Freer(Some(Retire::new(vec![0.0; 8]))));
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort { node: SRC, port: 0 })];
    let done = exec.apply(ed.commit().unwrap());
    ed.reclaim(done).expect("our own applied box");
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
    let done = exec.apply(ed.commit().unwrap());
    ed.reclaim(done).expect("our own applied box");
    assert_eq!(render(&mut exec, 8, 1)[0][0], 2.0);
    gain.store(Amplitude(0.25));
    assert_eq!(render(&mut exec, 8, 1)[0][0], 0.5);
}

/// At most `MAX_IN_FLIGHT` commits may be out; reclaiming one frees a credit.
/// This is what makes the phase-2 return push unable to fail.
///
/// Mutation: never increment `in_flight` in `commit` → no back-pressure →
/// fails.
#[test]
fn commits_are_back_pressured_until_reclaimed() {
    let (mut ed, mut exec) = Editor::new(prepare(8));
    let mut out = Vec::new();
    for i in 0..MAX_IN_FLIGHT {
        ed.insert(
            NodeKey(i as u64),
            "c",
            TestNode::new(Kind::Const {
                value: 1.0,
                width: 1,
            }),
        );
        out.push(exec.apply(ed.commit().expect("a credit is free")));
    }
    ed.insert(
        NodeKey(99),
        "c",
        TestNode::new(Kind::Const {
            value: 1.0,
            width: 1,
        }),
    );
    assert_eq!(ed.commit().err(), Some(CommitError::Backpressure));
    ed.reclaim(out.pop().unwrap()).expect("our own applied box");
    assert!(ed.commit().is_ok());
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
    let done = exec.apply(ed.commit().unwrap());
    assert!(ed.reclaim(done).expect("reclaims").is_empty());
    assert_eq!(render(&mut exec, 8, 1)[0][0], 1.0);

    ed.insert(
        SRC,
        "c",
        TestNode::new(Kind::Const {
            value: 3.0,
            width: 1,
        }),
    );
    let done = exec.apply(ed.commit().unwrap());
    assert_eq!(
        ed.reclaim(done).expect("reclaims"),
        vec![SRC],
        "the old unit came back"
    );
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
    let done = exec.apply(ed.commit().unwrap());
    assert_eq!(ed.reclaim(done).expect("reclaims"), vec![SRC]);
    assert_eq!(render(&mut exec, 8, 1)[0][0], 5.0);
}

/// A synth: a note-on starts a constant output, a note-off starts a linear
/// release of `release` frames, then `Status::Silent`. Its tail is the
/// release — true after a note-off, and not a promise about a held note.
struct Synth {
    release: usize,
    level: f32,
    releasing: Option<usize>,
    calls: Arc<AtomicUsize>,
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
            while next < evs.len() && evs[next].offset as usize == i {
                if evs[next].is_note_off() {
                    self.releasing = Some(self.release);
                } else {
                    self.level = 1.0;
                    self.releasing = None;
                }
                next += 1;
            }
            if let Some(left) = self.releasing.as_mut() {
                self.level = *left as f32 / self.release as f32;
                *left = left.saturating_sub(1);
            }
            io.output(0)[i] = self.level;
        }
        if self.level == 0.0 {
            Status::Silent
        } else {
            Status::Modified
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
        let start = cx.env.frame;
        if (start..start + io.frames() as u64).contains(&self.at) {
            let _ = io.event_out(0).push(tutti_graph::Event::midi(
                (self.at - start) as u32,
                self.words,
            ));
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

/// R1: a held note on a finite-tail synth keeps sounding — its event input is
/// quiet for as long as the note is held, and that is not an idle node — and
/// once released and silent it is skipped.
///
/// Mutation: drop `u.last_quiet &&` from the skip condition in
/// `Executor::process` → the synth is skipped `release` frames after the
/// note-on and the held note goes silent → fails.
#[test]
fn a_held_note_keeps_sounding_and_a_released_one_is_skipped() {
    let (on, off, synth) = (NodeKey(1), NodeKey(2), NodeKey(3));
    let calls = Arc::new(AtomicUsize::new(0));
    let (mut ed, mut exec) = Editor::new(prepare(32));
    ed.insert(
        on,
        "on",
        OneShot {
            at: 5,
            words: [0x2090_3c64, 0, 0, 0],
        },
    );
    ed.insert(
        off,
        "off",
        OneShot {
            at: 1000,
            words: [0x2080_3c00, 0, 0, 0],
        },
    );
    ed.insert(
        synth,
        "synth",
        Synth {
            release: 40,
            level: 0.0,
            releasing: None,
            calls: Arc::clone(&calls),
        },
    );
    let spec = ed.spec_mut();
    for from in [on, off] {
        spec.connect_events(
            tutti_graph::EventIn {
                node: synth,
                port: 0,
            },
            tutti_graph::EventEdge::Direct(tutti_graph::EventOut {
                node: from,
                port: 0,
            }),
        );
    }
    spec.topology.outputs = vec![Source::Node(OutPort {
        node: synth,
        port: 0,
    })];
    let done = exec.apply(ed.commit().unwrap());
    ed.reclaim(done).expect("our own applied box");

    let mut out = Vec::new();
    for _ in 0..60 {
        out.extend(render(&mut exec, 32, 1).remove(0));
    }
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
        "and skipped once released and silent ({total} calls of 60 blocks)"
    );
}

fn one_node_commit(ed: &mut Editor) -> Retire<tutti_graph::Commit> {
    ed.insert(
        SRC,
        "c",
        TestNode::new(Kind::Const {
            value: 1.0,
            width: 1,
        }),
    );
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort { node: SRC, port: 0 })];
    ed.commit().expect("commits")
}

/// R2: a plan compiled for another `Prepare` is refused at `apply` — the
/// units in it were prepared for a `MaxBlock` this executor does not keep.
///
/// Mutation: delete the `assert_eq!(plan.prepare, self.prepare)` in
/// `Executor::apply` → the foreign plan installs → fails.
#[test]
#[should_panic(expected = "another Prepare")]
fn apply_refuses_a_plan_for_another_prepare() {
    let (mut ed, _) = Editor::new(prepare(128));
    let (_, mut exec) = Editor::new(prepare(64));
    let commit = one_node_commit(&mut ed);
    drop(exec.apply(commit));
}

/// R4: a box goes back only to the editor that sent it, and only once
/// applied; an unapplied one is handed back to be applied.
///
/// Mutation: drop the token comparison in `Editor::reclaim` → the foreign
/// box is accepted and the other editor's credit restored → fails. Drop the
/// `applied` check → the unapplied box is accepted → fails.
#[test]
fn reclaim_checks_the_editor_and_that_the_box_was_applied() {
    let (mut ed, mut exec) = Editor::new(prepare(16));
    let (mut other, _) = Editor::new(prepare(16));

    let commit = one_node_commit(&mut ed);
    let commit = match ed.reclaim(commit) {
        Err(tutti_graph::ReclaimError::NotApplied(c)) => c,
        other => panic!("expected NotApplied, got {other:?}"),
    };
    assert_eq!(ed.in_flight(), 1, "the credit stays spent until applied");
    let done = exec.apply(commit);
    let done = match other.reclaim(done) {
        Err(tutti_graph::ReclaimError::ForeignEditor(c)) => c,
        other => panic!("expected ForeignEditor, got {other:?}"),
    };
    assert!(ed.reclaim(done).is_ok());
    assert_eq!(ed.in_flight(), 0);
}

/// R3: the box `apply` returns holds the previous plan (and with it the
/// previous arena and delay state), so the executor frees none of it; and
/// `apply` runs marked as the audio thread, so a commit dropped inside it
/// would panic in debug builds.
///
/// Mutation: in `apply`, leave `c.plan` empty instead of storing the old plan
/// → the returned box has no plan → fails.
#[test]
fn apply_returns_what_it_replaced() {
    let (mut ed, mut exec) = Editor::new(prepare(16));
    let first = one_node_commit(&mut ed);
    let first_plan = std::sync::Arc::clone(first.plan().unwrap());
    let done = exec.apply(first);
    assert!(done.plan().is_none(), "nothing was running before");
    ed.reclaim(done).unwrap();

    ed.insert(
        SRC,
        "c",
        TestNode::new(Kind::Const {
            value: 2.0,
            width: 1,
        }),
    );
    let done = exec.apply(ed.commit().unwrap());
    assert!(
        std::sync::Arc::ptr_eq(done.plan().unwrap(), &first_plan),
        "the previous plan comes back in the box"
    );
    assert!(done.is_applied());
    ed.reclaim(done).unwrap();
}

/// A `Commit` replaced in place on the audio thread — the one way to free it
/// there once `Retire` has no `DerefMut` — panics in a debug build: its own
/// `Drop` checks the marker.
///
/// Mutation: delete `AudioThread::check_not_current("Commit")` from
/// `Commit::drop` → the replacement is silent → fails.
#[test]
#[cfg(debug_assertions)]
fn a_commit_dropped_on_the_audio_thread_panics() {
    let (mut ed, _exec) = Editor::new(prepare(16));
    let mut a = one_node_commit(&mut ed);
    ed.insert(
        FX,
        "c",
        TestNode::new(Kind::Const {
            value: 1.0,
            width: 1,
        }),
    );
    let b = ed.commit().expect("a second credit is free");
    let b = b.reclaim(); // on the control thread: fine
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _rt = tutti_types::AudioThread::enter();
        *a.get_mut() = *b; // drops the old Commit here
        drop(a.reclaim());
    }));
    let msg = caught.expect_err("the replacement panics");
    let msg = msg.downcast_ref::<String>().cloned().unwrap_or_default();
    assert!(msg.contains("Commit dropped on the audio thread"), "{msg}");
}
