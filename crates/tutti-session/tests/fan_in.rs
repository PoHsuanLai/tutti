//! A port holds a set of sources and the graph sums it (doc 013, "Audio
//! fan-in sums", point 7): `sources`, `set_sources`, `add_source` and
//! `remove_source` on a node's input, the same four on a global output
//! channel, and the output tap that hears a summed channel whole.
//!
//! Every figure is analytic: constants of exactly representable values, whose
//! sum is exact in `f32`.

use tutti_nodes::testing::{Const, Through};
use tutti_session::{GraphSource, Session};
use tutti_types::{ChannelLayout, InterleavedMut, SampleRate, Samples};

const A: f32 = 0.25;
const B: f32 = 0.5;
const C: f32 = 0.125;

fn offline() -> Session {
    Session::builder()
        .build_offline(SampleRate(48_000.0), ChannelLayout::STEREO, Samples(1_024))
        .expect("builds")
}

/// Commits and renders one 64-frame block; returns output channel 0.
fn left(s: &mut Session) -> Vec<f32> {
    s.update();
    let mut block = vec![f32::NAN; 64 * 2];
    s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO));
    block.iter().step_by(2).copied().collect()
}

/// A session with constants `a`, `b`, `c` (`A`, `B`, `C`) and a pass-through
/// `sink` feeding output channel 0, committed once so `is_edited` is false.
struct Rig {
    s: Session,
    a: tutti_graph::NodeKey,
    b: tutti_graph::NodeKey,
    c: tutti_graph::NodeKey,
    sink: tutti_graph::NodeKey,
}

fn rig() -> Rig {
    let mut s = offline();
    let g = s.graph();
    let (a, _) = g.insert(Const::mono(A));
    let (b, _) = g.insert(Const::mono(B));
    let (c, _) = g.insert(Const::mono(C));
    let (sink, _) = g.insert(Through::mono());
    g.set_output_source(0, GraphSource::Node(sink, 0));
    s.update();
    assert!(!s.graph_ref().is_edited());
    Rig { s, a, b, c, sink }
}

fn node(k: tutti_graph::NodeKey) -> GraphSource {
    GraphSource::Node(k, 0)
}

/// A second source on a port is a second addend, in source order.
///
/// Mutation (run, recorded here): `Graph::add_source` writes
/// `edges.insert(at, vec![edge])` (replace) instead of `Topology::connect`
/// -> the render is `B` and `sources` holds one.
#[test]
fn add_source_adds_an_addend() {
    let mut r = rig();
    r.s.graph().add_source(r.sink, 0, node(r.b));
    r.s.graph().add_source(r.sink, 0, node(r.a));
    assert_eq!(r.s.graph_ref().sources(r.sink, 0), [node(r.a), node(r.b)]);
    assert!(left(&mut r.s).iter().all(|&x| x == A + B));
}

/// Removing a source leaves the others, down to one (aliased) and none
/// (silence).
///
/// Mutation (run, recorded here): `remove_source` returns before the
/// `disconnect` -> the render stays `A + B`.
#[test]
fn remove_source_goes_back_to_one() {
    let mut r = rig();
    r.s.graph().add_source(r.sink, 0, node(r.a));
    r.s.graph().add_source(r.sink, 0, node(r.b));
    assert!(left(&mut r.s).iter().all(|&x| x == A + B));
    r.s.graph().remove_source(r.sink, 0, node(r.b));
    assert_eq!(r.s.graph_ref().sources(r.sink, 0), [node(r.a)]);
    assert!(left(&mut r.s).iter().all(|&x| x == A));
    r.s.graph().remove_source(r.sink, 0, node(r.a));
    assert!(r.s.graph_ref().sources(r.sink, 0).is_empty());
    assert!(left(&mut r.s).iter().all(|&x| x == 0.0));
}

/// `set_sources` means exactly these, whatever order they are listed in, and
/// `set_source` means exactly one.
///
/// Mutation (run, recorded here): `set_sources` adds each source without
/// clearing the port -> the replaced `a` and `b` still sound (`A + B + C`).
#[test]
fn set_sources_replaces_the_set() {
    let mut r = rig();
    r.s.graph().add_source(r.sink, 0, node(r.a));
    r.s.graph().add_source(r.sink, 0, node(r.b));
    r.s.graph().set_sources(r.sink, 0, &[node(r.c)]);
    assert_eq!(r.s.graph_ref().sources(r.sink, 0), [node(r.c)]);
    assert!(left(&mut r.s).iter().all(|&x| x == C));

    r.s.graph().set_sources(r.sink, 0, &[node(r.b), node(r.a)]);
    assert_eq!(
        r.s.graph_ref().sources(r.sink, 0),
        [node(r.a), node(r.b)],
        "listed in any order, held in the graph's"
    );
    assert!(left(&mut r.s).iter().all(|&x| x == A + B));

    r.s.graph().set_source(r.sink, 0, node(r.c));
    assert_eq!(r.s.graph_ref().sources(r.sink, 0), [node(r.c)]);
    assert!(left(&mut r.s).iter().all(|&x| x == C));
}

/// A source listed twice is one source (not a weight of 2), and adding one
/// the port holds changes nothing.
///
/// Mutation (run, recorded here): drop the `dedup` in `Graph::edge_set` ->
/// the topology holds the edge twice and the update refuses it
/// (`DuplicateEdge`), so the render is not `A`.
#[test]
fn a_source_twice_is_one_source() {
    let mut r = rig();
    r.s.graph().set_sources(r.sink, 0, &[node(r.a), node(r.a)]);
    assert_eq!(r.s.graph_ref().sources(r.sink, 0), [node(r.a)]);
    r.s.graph().add_source(r.sink, 0, node(r.a));
    assert_eq!(r.s.graph_ref().sources(r.sink, 0), [node(r.a)]);
    assert!(left(&mut r.s).iter().all(|&x| x == A));
}

/// A change to nothing marks nothing edited, so nothing recompiles; a real
/// change does.
///
/// Mutations (each run, each fails this test): `set_sources` sets `edited`
/// on its unchanged path; `add_source` always sets it; `remove_source` always
/// sets it.
#[test]
fn an_unchanged_set_marks_nothing_edited() {
    let mut r = rig();
    r.s.graph().set_sources(r.sink, 0, &[node(r.a), node(r.b)]);
    assert!(r.s.graph_ref().is_edited(), "a change");
    r.s.update();
    r.s.graph().set_sources(r.sink, 0, &[node(r.b), node(r.a)]);
    assert!(!r.s.graph_ref().is_edited(), "the same set, reordered");
    r.s.graph().add_source(r.sink, 0, node(r.a));
    assert!(!r.s.graph_ref().is_edited(), "a source it holds");
    r.s.graph().remove_source(r.sink, 0, node(r.c));
    assert!(!r.s.graph_ref().is_edited(), "a source it lacks");
    r.s.graph().add_source(r.sink, 0, GraphSource::Silence);
    assert!(!r.s.graph_ref().is_edited(), "silence is no source");
    r.s.graph().add_source(r.sink, 0, node(r.c));
    assert!(r.s.graph_ref().is_edited(), "a new source");
}

/// Silence is the empty set: a lone `Silence` clears the port, and
/// `add_source` of it adds nothing.
///
/// Mutation (run): `edge_set` stops filtering `Source::Zero` -> the port
/// stores it and `planned_compensation` is `None` (the spec no longer
/// validates).
#[test]
fn silence_is_the_empty_set() {
    let mut r = rig();
    r.s.graph().set_sources(r.sink, 0, &[node(r.a), node(r.b)]);
    r.s.graph().set_sources(r.sink, 0, &[GraphSource::Silence]);
    assert!(r.s.graph_ref().sources(r.sink, 0).is_empty());
    r.s.graph().add_source(r.sink, 0, GraphSource::Silence);
    assert!(r.s.graph_ref().sources(r.sink, 0).is_empty());
    assert!(
        r.s.graph_ref().planned_compensation().is_some(),
        "no `Source::Zero` is stored: the spec validates"
    );
    assert!(left(&mut r.s).iter().all(|&x| x == 0.0));
}

/// Silence is the sum's zero: listed beside a source it is dropped, so the
/// set is that source and the render is its figure.
///
/// Mutation (run): `edge_set` keeps `Source::Zero` in a set of several ->
/// the port holds two entries and the spec no longer validates.
#[test]
fn silence_beside_a_source_adds_nothing() {
    let mut r = rig();
    r.s.graph()
        .set_sources(r.sink, 0, &[node(r.a), GraphSource::Silence]);
    assert_eq!(r.s.graph_ref().sources(r.sink, 0), [node(r.a)]);
    assert!(r.s.graph_ref().planned_compensation().is_some());
    assert!(left(&mut r.s).iter().all(|&x| x == A));
}

#[test]
#[should_panic(expected = "cannot feed its own input")]
fn a_node_cannot_be_its_own_source() {
    let mut r = rig();
    r.s.graph().add_source(r.sink, 0, node(r.sink));
}

/// A global output channel is a sink too: its sources sum.
///
/// Mutation (run, recorded here): `add_output_source` replaces the channel's
/// set (`vec![source]`) -> the render is `B`; `remove_output_source` removes
/// nothing -> the figure after it stays `A + B`.
#[test]
fn an_output_channel_sums_its_sources() {
    let mut r = rig();
    r.s.graph().set_output_sources(0, &[node(r.b)]);
    r.s.graph().add_output_source(0, node(r.a));
    assert_eq!(r.s.graph_ref().output_sources(0), [node(r.a), node(r.b)]);
    assert!(left(&mut r.s).iter().all(|&x| x == A + B));

    r.s.graph().remove_output_source(0, node(r.b));
    assert_eq!(r.s.graph_ref().output_sources(0), [node(r.a)]);
    assert!(left(&mut r.s).iter().all(|&x| x == A));

    r.s.graph().set_output_sources(0, &[node(r.c), node(r.a)]);
    assert!(left(&mut r.s).iter().all(|&x| x == A + C));
    r.s.graph().set_output_source(0, node(r.c));
    assert_eq!(r.s.graph_ref().output_sources(0), [node(r.c)]);
    assert!(left(&mut r.s).iter().all(|&x| x == C));
}

/// The set-setters of an output mark nothing edited when nothing changes.
///
/// Mutation (run, recorded here): `set_output_sources` sets `edited`
/// unconditionally -> the first assert fails.
#[test]
fn an_unchanged_output_set_marks_nothing_edited() {
    let mut r = rig();
    r.s.graph().set_output_sources(0, &[node(r.sink)]);
    assert!(!r.s.graph_ref().is_edited());
    r.s.graph().add_output_source(0, node(r.sink));
    assert!(!r.s.graph_ref().is_edited());
    r.s.graph().remove_output_source(0, node(r.a));
    assert!(!r.s.graph_ref().is_edited());
}

/// The output tap hears a fan-in channel whole, before and after the fan-in
/// is wired, and follows it as it changes.
///
/// Mutation (run, recorded here): `Graph::open_output_tap` opens a ports tap
/// on the first source of each channel (the old snapshot) -> lane 0 reads `A`,
/// not `A + B`, from the first block on.
#[test]
fn the_output_tap_hears_the_sum() {
    let mut r = rig();
    r.s.graph().set_output_sources(0, &[node(r.a), node(r.b)]);
    let mut tap =
        r.s.graph()
            .open_output_tap(Samples(1_024))
            .expect("the sources exist");
    assert!(left(&mut r.s).iter().all(|&x| x == A + B));
    r.s.graph().add_output_source(0, node(r.c));
    assert!(left(&mut r.s).iter().all(|&x| x == A + B + C));

    let mut got = vec![f32::NAN; 128 * 2];
    assert_eq!(tap.pop_into(&mut got), Samples(128));
    let lane: Vec<f32> = got.iter().step_by(2).copied().collect();
    assert!(lane[..64].iter().all(|&x| x == A + B), "{lane:?}");
    assert!(lane[64..].iter().all(|&x| x == A + B + C), "{lane:?}");
    assert!(got.iter().skip(1).step_by(2).all(|&x| x == 0.0));
}
