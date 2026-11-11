//! A modulation route onto the **gain of an edge** (doc 013, "Edge gain",
//! G2): `Mods::route(source, EdgeRef, depth)` gives the edge's sink sum a
//! driven lane, as a route onto a param gives a port one.
//!
//! The figures are analytic. Two constant sources sum at global output 0;
//! a macro (a constant source) is routed onto the edge of the first at a
//! depth over a range, and the render is the weighted fold
//! `(0.0 + w * a) + b` with `w = depth * macro * span` (a bipolar linear
//! route on a lane with no knob: its base is nothing, and the lane's value is
//! the route's whole figure; the range clamps it).

use crate::common;

use tutti_graph::ForkByClone;
use tutti_session::{EdgeRef, GraphSource, ModTarget, ParamRef, Session};
use tutti_types::MaxBlock;
use tutti_types::{
    Amplitude, ChannelLayout, Depth, InterleavedMut, SampleRate, Samples, UnitParam,
};

use common::probe::Pulse;

const RATE: f64 = 48_000.0;
const BLOCK: usize = 128;
/// The sources' levels.
const A: f32 = 0.5;
const B: f32 = 0.25;

fn offline() -> Session {
    Session::builder()
        .build_offline(
            SampleRate(RATE),
            ChannelLayout::STEREO,
            MaxBlock::of(Samples(1_024)),
        )
        .expect("builds")
}

fn settle(s: &mut Session) {
    for _ in 0..6 {
        let _ = s.update();
    }
    assert_eq!(s.mod_state().refusal(), None, "the declarations apply");
    assert_eq!(
        s.mod_state().unresolved_targets(),
        &[],
        "every target resolved"
    );
}

/// Output channel 0 (the left of the stereo root) for `blocks` blocks.
fn render(s: &mut Session, blocks: usize) -> Vec<f32> {
    let mut all = Vec::new();
    let mut buf = vec![0.0f32; BLOCK * 2];
    for _ in 0..blocks {
        s.render(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO));
        all.extend(buf.as_chunks::<2>().0.iter().map(|f| f[0]));
    }
    all
}

/// Two always-on sources summed at output 0; the edge of the first.
fn mixer(s: &mut Session) -> EdgeRef {
    let level = |level| {
        ForkByClone(Pulse {
            from: 0,
            to: u64::MAX,
            level,
        })
    };
    let (a, _) = s.graph().add(level(A));
    let (b, _) = s.graph().add(level(B));
    s.graph().add_output_source(0, GraphSource::Node(a, 0));
    s.graph().add_output_source(0, GraphSource::Node(b, 0));
    EdgeRef::output(0, GraphSource::Node(a, 0))
}

/// A route onto an edge's gain modulates the sum by the analytic amount. The
/// lane's value is its knob (the gain's base, unity) plus the route's offset,
/// `depth * macro * span`, clamped to the range: with a range of `-2..=2`
/// (span 4), macro 0.5 at depth 0.25 gives `w = 1.0 + 0.5 = 1.5`, so the sum
/// is `(0.0 + 1.5 * A) + B`. Then the macro moves (`w = 1.0 - 0.5 = 0.5`),
/// the knob moves (`ModState::set_knob`: `w = -0.25 - 0.5`, polarity), a
/// narrower range changes the span, and one whose floor the value is below
/// clamps it; and removing the route returns the edge to its base, which
/// the knob left at `-0.25` (the knob is the edge's gain: one cell), once the
/// lane's source goes (the declick).
#[test]
fn a_route_onto_an_edge_modulates_the_sum_by_the_analytic_amount() {
    let mut s = offline();
    let edge = mixer(&mut s);
    settle(&mut s);
    let plain = render(&mut s, 2);
    assert!(plain.iter().all(|&x| x == A + B), "unity before any route");

    let mut m = s.mods();
    m.range(edge, -2.0, 2.0);
    let source = m.macro_(0.5);
    let route = m.route(source, edge, Depth(0.25)).unwrap();
    let target = m.key(edge);
    settle(&mut s);

    // Past the lane's first block and the connect's declick.
    let figure = |s: &mut Session, w: f32, what: &str| {
        let got = render(s, 12);
        let want = (0.0 + w * A) + B;
        for (f, &x) in got.iter().enumerate().skip(512) {
            assert_eq!(x.to_bits(), want.to_bits(), "{what}, frame {f}");
        }
    };
    figure(&mut s, 1.5, "knob + offset");

    s.mods().set_source(source, tutti_modulation::Macro(-0.5));
    settle(&mut s);
    figure(&mut s, 0.5, "the macro moved");

    assert!(s.mod_state().set_knob(target, -0.25), "the lane has a knob");
    settle(&mut s);
    figure(&mut s, -0.75, "the knob moved");

    // The range is the route's span too: `-0.25 + 0.25 * -0.5 * 2`.
    s.mods().range(edge, -2.0, 0.0);
    settle(&mut s);
    figure(&mut s, -0.5, "within the range");
    // Span 2.25: `-0.25 - 0.28125` is below the range's floor.
    s.mods().range(edge, -0.25, 2.0);
    settle(&mut s);
    figure(&mut s, -0.25, "clamped to the range");

    s.mods().remove_route(route);
    settle(&mut s);
    figure(&mut s, -0.25, "the route gone: the base the knob left");
}

/// The gain has one writer. `Graph::set_edge_gain` is the knob whether or not
/// the edge is routed: a base of 0.5 stays the base when a route arrives (the
/// lane modulates around it, it does not snap to unity), a base moved while
/// routed moves the modulated figure, and removing the route returns to the
/// base, not unity.
///
/// With a range of `-2..=2` (span 4) the macro at 0.5 and depth 0.5 offsets
/// by `0.5 * 0.5 * 4 = 1.0`: `w = base + 1.0`, so `1.5`, then `1.25` once the
/// base moves to 0.25; gone, the base `0.25`.
#[test]
fn the_base_is_the_lanes_knob_and_a_route_returns_to_it() {
    let mut s = offline();
    let edge = mixer(&mut s);
    settle(&mut s);
    s.graph()
        .set_edge_gain(edge, Amplitude::new(0.5))
        .expect("an edge");
    settle(&mut s);

    let figure = |s: &mut Session, w: f32, what: &str| {
        let got = render(s, 12);
        let want = (0.0 + w * A) + B;
        for (f, &x) in got.iter().enumerate().skip(512) {
            assert_eq!(x.to_bits(), want.to_bits(), "{what}, frame {f}");
        }
    };
    figure(&mut s, 0.5, "the base, before any route");

    let mut m = s.mods();
    m.range(edge, -2.0, 2.0);
    let source = m.macro_(0.5);
    let route = m.route(source, edge, Depth(0.5)).unwrap();
    settle(&mut s);
    figure(&mut s, 1.5, "the base plus the route's offset");

    s.graph()
        .set_edge_gain(edge, Amplitude::new(0.25))
        .expect("an edge");
    settle(&mut s);
    figure(&mut s, 1.25, "the base moved while routed");
    assert_eq!(s.graph_ref().edge_gain(edge), Some(Amplitude::new(0.25)));

    s.mods().remove_route(route);
    settle(&mut s);
    figure(&mut s, 0.25, "the route gone: the base, not unity");
}

/// An edge target keys once per edge, apart from a param's, and is no param.
#[test]
fn an_edge_target_keys_apart_and_goes_with_its_node() {
    let mut s = offline();
    let edge = mixer(&mut s);
    let key = s.mods().key(edge);
    assert_eq!(key, s.mods().key(ModTarget::Edge(edge)), "one key an edge");
    let param = ParamRef::unit(tutti_graph::NodeKey(1), UnitParam::Volume);
    assert_ne!(key, s.mods().key(param));
    assert_eq!(ParamRef::from_key(key), None);
}
