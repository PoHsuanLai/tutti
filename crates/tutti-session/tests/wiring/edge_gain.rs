//! The session's edge gains (doc 013, "Edge gain", G3): `set_edge_gain`,
//! `edge_gain` and `clear_edge_gain`, the weighted fan-in forms
//! `add_source_weighted` and `add_output_source_weighted`, and what removing a
//! source does to its weight.
//!
//! Every figure is analytic: constants of exactly representable values and
//! weights, so the weighted fold `(0.0 + w * a) + b` is exact in `f32`.

use std::sync::Arc;
use tutti_types::MaxBlock;

use tutti_graph::NodeKey;
use tutti_nodes::testing::{Const, Through};
use tutti_session::{EdgeGainError, EdgeRef, GraphSource, Session};
use tutti_types::{Amplitude, ChannelLayout, InterleavedMut, SampleRate, Samples};

const A: f32 = 0.25;
const B: f32 = 0.5;
const BLOCK: usize = 64;

fn offline() -> Session {
    Session::builder()
        .build_offline(
            SampleRate(48_000.0),
            ChannelLayout::STEREO,
            MaxBlock::of(Samples(1_024)),
        )
        .expect("builds")
}

fn amp(x: f32) -> Amplitude {
    Amplitude::new(x)
}

/// Commits and renders two blocks, so a weight that moved has finished its
/// one-block ramp; returns the last block's channel `ch`.
fn settled(s: &mut Session, ch: usize) -> Vec<f32> {
    s.update();
    let mut block = vec![f32::NAN; BLOCK * 2];
    for _ in 0..2 {
        s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO));
    }
    block.iter().skip(ch).step_by(2).copied().collect()
}

fn all_are(block: &[f32], want: f32) -> bool {
    block.iter().all(|x| x.to_bits() == want.to_bits())
}

/// Constants `a` (`A`) and `b` (`B`) and a pass-through `sink` feeding output
/// channel 0, committed once so `is_edited` is false. Nothing feeds the sink.
struct Rig {
    s: Session,
    a: NodeKey,
    b: NodeKey,
    sink: NodeKey,
}

fn rig() -> Rig {
    let mut s = offline();
    let g = s.graph();
    let (a, _) = g.insert(Const::mono(A));
    let (b, _) = g.insert(Const::mono(B));
    let (sink, _) = g.insert(Through::mono());
    g.set_output_source(0, GraphSource::Node(sink, 0));
    s.update();
    assert!(!s.graph_ref().is_edited());
    Rig { s, a, b, sink }
}

fn node(k: NodeKey) -> GraphSource {
    GraphSource::Node(k, 0)
}

impl Rig {
    fn edge_from(&self, from: NodeKey) -> EdgeRef {
        EdgeRef::port(self.sink, 0, node(from))
    }
}

/// Set, read, change and clear: an edge with no entry reads `None` (it is
/// aliased, not weighted at unity); giving it one is structural and moves the
/// sum; clearing returns the plain sum and is structural too, and a second
/// clear finds nothing.
#[test]
fn an_edge_gain_sets_reads_and_clears() {
    let mut r = rig();
    r.s.graph().add_source(r.sink, 0, node(r.a));
    r.s.graph().add_source(r.sink, 0, node(r.b));
    assert!(all_are(&settled(&mut r.s, 0), A + B));
    let edge = r.edge_from(r.a);
    assert_eq!(r.s.graph_ref().edge_gain(edge), None, "plain: no entry");

    r.s.graph().set_edge_gain(edge, amp(0.5)).expect("an edge");
    assert!(r.s.graph_ref().is_edited(), "a first gain is structural");
    assert_eq!(r.s.graph_ref().edge_gain(edge), Some(amp(0.5)));
    assert!(all_are(&settled(&mut r.s, 0), (0.0 + 0.5 * A) + B));

    r.s.graph().set_edge_gain(edge, amp(-2.0)).expect("an edge");
    assert_eq!(r.s.graph_ref().edge_gain(edge), Some(amp(-2.0)));
    assert!(all_are(&settled(&mut r.s, 0), (0.0 + -2.0 * A) + B));

    assert!(r.s.graph().clear_edge_gain(edge));
    assert!(r.s.graph_ref().is_edited(), "clearing is structural");
    assert_eq!(r.s.graph_ref().edge_gain(edge), None);
    assert!(all_are(&settled(&mut r.s, 0), A + B), "the plain sum again");
    assert!(!r.s.graph().clear_edge_gain(edge), "nothing left to clear");
    assert!(!r.s.graph_ref().is_edited());
}

/// A gain on an edge that is not there is refused, and so is one that is not
/// a number; the graph is unchanged (nothing edited, no entry). Silence is
/// never an edge.
#[test]
fn a_gain_on_a_missing_edge_or_a_bad_number_is_refused() {
    let mut r = rig();
    r.s.graph().add_source(r.sink, 0, node(r.a));
    r.s.update();
    let missing = r.edge_from(r.b);
    assert_eq!(
        r.s.graph().set_edge_gain(missing, amp(0.5)),
        Err(EdgeGainError::NoSuchEdge(missing))
    );
    let silence = EdgeRef::port(r.sink, 0, GraphSource::Silence);
    assert_eq!(
        r.s.graph().set_edge_gain(silence, amp(0.5)),
        Err(EdgeGainError::NoSuchEdge(silence))
    );
    let real = r.edge_from(r.a);
    assert_eq!(
        r.s.graph().set_edge_gain(real, amp(f32::NAN)),
        Err(EdgeGainError::NotFinite)
    );
    assert_eq!(
        r.s.graph().set_edge_gain(real, amp(f32::INFINITY)),
        Err(EdgeGainError::NotFinite)
    );
    assert_eq!(r.s.graph_ref().edge_gain(real), None);
    assert_eq!(r.s.graph_ref().edge_gain(missing), None);
    assert!(!r.s.graph_ref().is_edited(), "a refusal edits nothing");
}

/// `add_source_weighted` adds the edge and its weight in one edit, and the
/// render is the weighted fold; a source the port already holds only has its
/// weight set; silence adds nothing (it is no edge); a gain that is not a
/// number adds nothing.
#[test]
fn add_source_weighted_adds_the_edge_and_its_weight() {
    let mut r = rig();
    let (a, b, sink) = (r.a, r.b, r.sink);
    r.s.graph()
        .add_source_weighted(sink, 0, node(a), amp(0.5))
        .expect("finite");
    r.s.graph()
        .add_source_weighted(sink, 0, node(b), amp(-0.25))
        .expect("finite");
    assert_eq!(r.s.graph_ref().sources(sink, 0), [node(a), node(b)]);
    assert_eq!(r.s.graph_ref().edge_gain(r.edge_from(a)), Some(amp(0.5)));
    assert_eq!(r.s.graph_ref().edge_gain(r.edge_from(b)), Some(amp(-0.25)));
    assert!(all_are(&settled(&mut r.s, 0), (0.0 + 0.5 * A) + -0.25 * B));

    // Held already: the weight is set, nothing else changes.
    r.s.graph()
        .add_source_weighted(sink, 0, node(a), amp(2.0))
        .expect("finite");
    assert_eq!(r.s.graph_ref().sources(sink, 0), [node(a), node(b)]);
    assert!(all_are(&settled(&mut r.s, 0), (0.0 + 2.0 * A) + -0.25 * B));

    // Silence is no edge; a bad weight adds nothing.
    r.s.graph()
        .add_source_weighted(sink, 0, GraphSource::Silence, amp(3.0))
        .expect("a no-op");
    assert_eq!(r.s.graph_ref().sources(sink, 0), [node(a), node(b)]);
    let c = r.s.graph().insert(Const::mono(0.125)).0;
    assert_eq!(
        r.s.graph()
            .add_source_weighted(sink, 0, node(c), amp(f32::NAN)),
        Err(EdgeGainError::NotFinite)
    );
    assert_eq!(r.s.graph_ref().sources(sink, 0), [node(a), node(b)]);
}

/// The output-channel twin: weights on what feeds global output 1, read on
/// the right channel.
#[test]
fn add_output_source_weighted_weights_the_channel() {
    let mut r = rig();
    let (a, b) = (r.a, r.b);
    r.s.graph()
        .add_output_source_weighted(1, node(a), amp(0.5))
        .expect("finite");
    r.s.graph()
        .add_output_source_weighted(1, node(b), amp(2.0))
        .expect("finite");
    assert_eq!(r.s.graph_ref().output_sources(1), [node(a), node(b)]);
    assert_eq!(
        r.s.graph_ref().edge_gain(EdgeRef::output(1, node(a))),
        Some(amp(0.5))
    );
    assert!(all_are(&settled(&mut r.s, 1), (0.0 + 0.5 * A) + 2.0 * B));
    r.s.graph()
        .set_edge_gain(EdgeRef::output(1, node(b)), amp(1.0))
        .expect("an edge");
    assert!(all_are(&settled(&mut r.s, 1), (0.0 + 0.5 * A) + 1.0 * B));
    assert!(r.s.graph().clear_edge_gain(EdgeRef::output(1, node(a))));
    assert!(r.s.graph().clear_edge_gain(EdgeRef::output(1, node(b))));
    assert!(all_are(&settled(&mut r.s, 1), A + B));
}

/// A source that goes takes its weight with it, by whichever call removes it
/// (`remove_source`, `set_sources`, `remove_output_source`,
/// `set_output_sources`): the next commit is valid (a stale entry would fail
/// it), and the source added back is plain, not weighted again.
#[test]
fn removing_a_source_drops_its_weight() {
    let mut r = rig();
    let (a, b, sink) = (r.a, r.b, r.sink);
    let g = |r: &mut Rig| {
        r.s.graph()
            .add_source_weighted(sink, 0, node(a), amp(0.5))
            .unwrap();
        r.s.graph().add_source(sink, 0, node(b));
    };
    g(&mut r);
    assert!(all_are(&settled(&mut r.s, 0), (0.0 + 0.5 * A) + B));

    r.s.graph().remove_source(sink, 0, node(a));
    assert_eq!(r.s.graph_ref().edge_gain(r.edge_from(a)), None);
    assert!(all_are(&settled(&mut r.s, 0), B), "valid, and just b");
    r.s.graph().add_source(sink, 0, node(a));
    assert!(
        all_are(&settled(&mut r.s, 0), A + B),
        "added back plain, no stale weight"
    );

    let edge = r.edge_from(a);
    r.s.graph().set_edge_gain(edge, amp(0.5)).unwrap();
    r.s.graph().set_sources(sink, 0, &[node(b)]);
    assert_eq!(r.s.graph_ref().edge_gain(r.edge_from(a)), None);
    assert!(all_are(&settled(&mut r.s, 0), B));

    r.s.graph()
        .add_output_source_weighted(1, node(a), amp(0.5))
        .unwrap();
    r.s.graph().add_output_source(1, node(b));
    assert!(all_are(&settled(&mut r.s, 1), (0.0 + 0.5 * A) + B));
    r.s.graph().remove_output_source(1, node(a));
    assert_eq!(r.s.graph_ref().edge_gain(EdgeRef::output(1, node(a))), None);
    assert!(all_are(&settled(&mut r.s, 1), B));
    r.s.graph()
        .add_output_source_weighted(1, node(a), amp(0.5))
        .unwrap();
    r.s.graph().set_output_sources(1, &[node(b)]);
    assert_eq!(r.s.graph_ref().edge_gain(EdgeRef::output(1, node(a))), None);
    assert!(all_are(&settled(&mut r.s, 1), B));
}

/// Moving the weight of an edge that already has one is a control: nothing is
/// edited, no plan is sent (the sent plan is the same `Arc`, the commit count
/// the same), and the render still follows.
#[test]
fn moving_a_weight_marks_no_recompile() {
    let mut r = rig();
    let (a, sink) = (r.a, r.sink);
    r.s.graph()
        .add_source_weighted(sink, 0, node(a), amp(0.5))
        .unwrap();
    r.s.graph().add_source(sink, 0, node(r.b));
    assert!(all_are(&settled(&mut r.s, 0), (0.0 + 0.5 * A) + B));
    let plan = r.s.graph_ref().sent_plan().expect("a plan");
    let commits = r.s.graph_ref().commits_sent();

    let edge = r.edge_from(a);
    r.s.graph().set_edge_gain(edge, amp(1.5)).unwrap();
    assert!(!r.s.graph_ref().is_edited(), "a control, not a rewire");
    assert!(all_are(&settled(&mut r.s, 0), (0.0 + 1.5 * A) + B));
    assert!(Arc::ptr_eq(
        &plan,
        &r.s.graph_ref().sent_plan().expect("a plan")
    ));
    assert_eq!(r.s.graph_ref().commits_sent(), commits);
}
