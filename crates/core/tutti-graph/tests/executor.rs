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
    let mut ed = Editor::new(prepare(64));
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
    let mut exec = Executor::new(prepare(64));
    let done = exec.apply(ed.commit().expect("commits"));
    ed.reclaim(done);
    (ed, exec, calls)
}

/// Passes its input through and counts calls.
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
        io.channel(0).map(|x| x);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// A node with no tail is not called while its input is exact silence; one
/// with a finite tail is called until the tail has elapsed; one that has not
/// said is always called.
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
    assert_eq!(calls.load(Ordering::Relaxed), 0, "Tail::None: never called");

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
    let mut ed = Editor::new(prepare(32));
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
    let mut exec = Executor::new(prepare(32));
    let done = exec.apply(ed.commit().unwrap());
    ed.reclaim(done);
    let out = render(&mut exec, 32, 2);
    assert!(out[0].iter().all(|&x| x == 0.0));
    assert_eq!(
        calls.load(Ordering::Relaxed),
        0,
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
    let mut ed = Editor::new(prepare(16));
    ed.insert(SRC, "half", HalfSilent);
    ed.insert(FX, "observer", Observer(Arc::clone(&seen)));
    for port in 0..2 {
        ed.spec_mut().topology.edges.insert(
            InPort { node: FX, port },
            Edge::Direct(Source::Node(OutPort { node: SRC, port })),
        );
    }
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort { node: FX, port: 0 })];
    let mut exec = Executor::new(prepare(16));
    let done = exec.apply(ed.commit().unwrap());
    ed.reclaim(done);
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
    let mut ed = Editor::new(prepare(16));
    ed.insert(SRC, "freer", Freer(Some(Retire::new(vec![0.0; 8]))));
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort { node: SRC, port: 0 })];
    let mut exec = Executor::new(prepare(16));
    let done = exec.apply(ed.commit().unwrap());
    ed.reclaim(done);
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
    let mut ed = Editor::new(prepare(8));
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
    let mut exec = Executor::new(prepare(8));
    let done = exec.apply(ed.commit().unwrap());
    ed.reclaim(done);
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
    let mut ed = Editor::new(prepare(8));
    let mut exec = Executor::new(prepare(8));
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
    ed.reclaim(out.pop().unwrap());
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
    let mut ed = Editor::new(prepare(8));
    let mut exec = Executor::new(prepare(8));
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
    assert!(ed.reclaim(done).is_empty());
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
    assert_eq!(ed.reclaim(done), vec![SRC], "the old unit came back");
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
    assert_eq!(ed.reclaim(done), vec![SRC]);
    assert_eq!(render(&mut exec, 8, 1)[0][0], 5.0);
}
