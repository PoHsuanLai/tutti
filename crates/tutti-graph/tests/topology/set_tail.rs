//! `Editor::set_tail`: a runtime tail-ceiling change (a plugin whose ring-out
//! moved) recompiles at the next commit, moves the silence skip's park time,
//! and touches neither the unit nor a running crossfade.

use tutti_graph::testing as common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{Kind, TestNode, prepare};
use tutti_graph::NodeKey;
use tutti_graph::{
    CommitError, CrossfadeCurve, Cx, Editor, Executor, Fade, Io, MAX_NODE_TAIL, Node, Prepare,
    Shape, Status, TransportPosition, Unforkable,
};
use tutti_graph::{Edge, InPort, OutPort, Source};
use tutti_types::{ChannelLayout, Samples, Tail};

const NODE: NodeKey = NodeKey(1);
const SRC: NodeKey = NodeKey(3);
const BLOCK: usize = 64;

/// A mono pass-through that declares a tail ceiling and reports nothing, so
/// the executor parks it on the ceiling once its input has been quiet that
/// long. Counts its calls.
struct Ringer {
    tail: Tail,
    calls: Arc<AtomicUsize>,
}

impl Node for Ringer {
    fn shape(&self) -> Shape {
        Shape::through_unassigned(ChannelLayout::MONO).with_tail(self.tail)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        self.calls.fetch_add(1, Ordering::Relaxed);
        // Honest about silence: it is what lets the skip trust the node.
        if io.silent().get(0) {
            return Status::Silent;
        }
        let (ins, mut outs) = io.split();
        for (o, &x) in outs.get(0).iter_mut().zip(ins.get(0)) {
            *o = x;
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

fn graph(tail: Tail) -> (Editor, Executor, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let (mut ed, mut exec) = Editor::new(prepare(BLOCK));
    // A source flagged silent from its first block (`Status::Constant`, +0.0),
    // which is what the skip reads.
    ed.insert(
        SRC,
        "silence",
        Unforkable(TestNode::new(Kind::Const {
            value: 0.0,
            width: 1,
        })),
    );
    ed.insert(
        NODE,
        "ringer",
        Unforkable(Ringer {
            tail,
            calls: Arc::clone(&calls),
        }),
    );
    ed.spec_mut().topology.edges.insert(
        InPort {
            node: NODE,
            port: 0,
        },
        vec![Edge::Direct(Source::Node(OutPort { node: SRC, port: 0 }))],
    );
    ed.spec_mut()
        .topology
        .set_outputs(vec![Source::Node(OutPort {
            node: NODE,
            port: 0,
        })]);
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    (ed, exec, calls)
}

fn block(exec: &mut Executor, input: f32) -> Vec<f32> {
    let inp = vec![input; BLOCK];
    let mut out = vec![0.0f32; BLOCK];
    exec.process(
        BLOCK,
        &TransportPosition::default(),
        &[&inp[..]],
        &mut [&mut out[..]],
    );
    out
}

/// Calls the node takes over `blocks` blocks of silence.
fn calls_over(exec: &mut Executor, calls: &AtomicUsize, blocks: usize) -> usize {
    let before = calls.load(Ordering::Relaxed);
    for _ in 0..blocks {
        block(exec, 0.0);
    }
    calls.load(Ordering::Relaxed) - before
}

/// Applies the commit the executor is waiting on.
fn land(ed: &mut Editor, exec: &mut Executor) {
    ed.commit().expect("recompiles");
    exec.apply_pending();
    ed.collect();
}

/// **The ceiling moves at the next commit, and the park time follows it.** A
/// node with a ceiling of one block is called once on silence and parked;
/// declared seven blocks, it is called for seven. Until the commit the
/// executor holds the old ceiling.
#[test]
fn a_tail_change_moves_the_park_time_at_the_next_commit() {
    let long = Tail::Finite(Samples(7 * BLOCK));

    let (_ed, mut exec, calls) = graph(Tail::Finite(Samples(BLOCK)));
    assert_eq!(calls_over(&mut exec, &calls, 20), 1, "one block of ceiling");

    let (mut ed, mut exec, calls) = graph(Tail::Finite(Samples(BLOCK)));
    ed.set_tail(NODE, long).expect("sets");
    assert_eq!(ed.spec().topology.nodes[&NODE].tail, long);
    assert_eq!(
        calls_over(&mut exec, &calls, 20),
        1,
        "before the commit the executor reads the old ceiling"
    );

    let (mut ed, mut exec, calls) = graph(Tail::Finite(Samples(BLOCK)));
    ed.set_tail(NODE, long).expect("sets");
    land(&mut ed, &mut exec);
    assert_eq!(
        calls_over(&mut exec, &calls, 20),
        7,
        "seven blocks of ceiling"
    );
}

/// A shorter ceiling parks sooner, the same way round.
#[test]
fn a_shorter_tail_parks_sooner() {
    let (mut ed, mut exec, calls) = graph(Tail::Finite(Samples(7 * BLOCK)));
    ed.set_tail(NODE, Tail::Finite(Samples(BLOCK)))
        .expect("sets");
    land(&mut ed, &mut exec);
    assert_eq!(calls_over(&mut exec, &calls, 20), 1);
}

/// **A refusal changes nothing, and nothing is clamped.** An unknown key is
/// `NoSuchNode`; a finite tail past `MAX_NODE_TAIL` is `TailTooLong` and the
/// spec keeps the old figure; `Unbounded` and `Unknown` are declarable.
#[test]
fn set_tail_refuses_and_never_clamps() {
    let (mut ed, _exec, _calls) = graph(Tail::Finite(Samples(BLOCK)));
    assert!(matches!(
        ed.set_tail(NodeKey(99), Tail::None),
        Err(CommitError::NoSuchNode { node }) if node == NodeKey(99)
    ));
    let over = Samples(MAX_NODE_TAIL.get() + 1);
    assert!(matches!(
        ed.set_tail(NODE, Tail::Finite(over)),
        Err(CommitError::TailTooLong { node, tail, limit })
            if node == NODE && tail == over && limit == MAX_NODE_TAIL
    ));
    assert_eq!(
        ed.spec().topology.nodes[&NODE].tail,
        Tail::Finite(Samples(BLOCK)),
        "a refusal changes nothing"
    );
    ed.set_tail(NODE, Tail::Finite(MAX_NODE_TAIL))
        .expect("the limit itself fits");
    for tail in [Tail::Unbounded, Tail::Unknown, Tail::None] {
        ed.set_tail(NODE, tail).expect("declarable");
        assert_eq!(ed.spec().topology.nodes[&NODE].tail, tail);
    }
}

/// **A tail change does not cut a crossfade**: a ceiling feeds no PDC, so
/// the fade's two units keep fading across it (`set_latency`, which does,
/// is `replace.rs`'s).
#[test]
fn set_tail_does_not_cut_a_fade() {
    let fade = Fade::new(Samples(1000), CrossfadeCurve::EqualAmplitude);
    let gain = |g: f32| TestNode::new(Kind::Gain { gain: g, width: 1 });
    let (mut ed, mut exec, _) = graph(Tail::None);
    // A gain node at its own key: the fade needs a unit whose shape matches.
    ed.spec_mut().topology.inputs = ChannelLayout::MONO;
    ed.insert(NodeKey(2), "gain", Unforkable(gain(1.0)));
    ed.spec_mut().topology.edges.insert(
        InPort {
            node: NodeKey(2),
            port: 0,
        },
        vec![Edge::Direct(Source::Global(0))],
    );
    ed.spec_mut()
        .topology
        .set_outputs(vec![Source::Node(OutPort {
            node: NodeKey(2),
            port: 0,
        })]);
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    ed.replace(NodeKey(2), Unforkable(gain(2.0)), fade)
        .expect("fits");
    ed.commit().expect("commits");
    block(&mut exec, 1.0);
    assert_eq!(ed.fades_in_flight(), 1);

    ed.set_tail(NodeKey(2), Tail::Finite(Samples(BLOCK)))
        .expect("sets");
    ed.commit().expect("commits");
    let out = block(&mut exec, 1.0);
    assert_eq!(ed.fades_in_flight(), 1, "the fade runs on");
    assert!(ed.collect().is_empty(), "nothing was retired");
    assert!(
        out.iter().any(|&y| y != 1.0 && y != 2.0),
        "still mid-fade: {:?}",
        &out[..4]
    );
}
