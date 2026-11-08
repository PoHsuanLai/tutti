//! Indexed param addresses through the session (doc 013, "Param addresses"):
//! `Graph::set_param` and a modulation route reach a node's second param of
//! a kind (`Threshold` at index 1) without touching its first.
//!
//! The render stands on `tests/modulation.rs`'s analytic sine figure.

mod common;

use tutti_modulation::Lfo;
use tutti_session::{GraphSource, ParamRef, Session};
use tutti_transport::MotionEvent;
use tutti_types::{BeatDuration, Depth, ParamAddr, UnitAddr, UnitParam};
use tutti_types::{ChannelLayout, InterleavedMut, SampleRate, Samples};

use common::bands::{BAND0, BAND1, Bands};

const RATE: f64 = 48_000.0;
/// A lane's control step (`tutti_nodes::automation::CONTROL_STEP`).
const STEP: usize = 32;
/// A port's connect crossfade (`PARAM_DECLICK`).
const DECLICK: usize = 256;

fn offline() -> Session {
    Session::builder()
        .build_offline(SampleRate(RATE), ChannelLayout::STEREO, Samples(1_024))
        .expect("builds")
}

/// Renders `frames` frames from a rolling transport, in blocks of 128;
/// returns the two output channels.
fn render(s: &mut Session, frames: usize) -> [Vec<f32>; 2] {
    s.transport()
        .motion
        .try_send(MotionEvent::Play)
        .expect("room");
    let mut all = [Vec::new(), Vec::new()];
    let mut buf = vec![0.0f32; 128 * 2];
    for _ in 0..frames / 128 {
        s.render(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO));
        for frame in buf.as_chunks::<2>().0 {
            all[0].push(frame[0]);
            all[1].push(frame[1]);
        }
    }
    all
}

/// `Graph::set_param` writes the cell of the index it names and no other,
/// and the node reads it: the render shows each band's own value.
///
/// Mutation (run): `ParamSet::slot` matching the kind alone → the write to
/// band 1 lands on band 0 → fails.
#[test]
fn set_param_writes_the_index_it_names() {
    let mut s = offline();
    let (node, _) = s.graph().add(Bands::new(100.0, 500.0));
    s.graph().set_output_source(0, GraphSource::Node(node, 0));
    s.graph().set_output_source(1, GraphSource::Node(node, 1));
    s.update();

    let [a, b] = render(&mut s, 256);
    assert!(a.iter().all(|&x| x == 100.0) && b.iter().all(|&x| x == 500.0));

    s.graph().set_param(node, BAND1, -6.0);
    let [a, b] = render(&mut s, 256);
    assert!(a.iter().all(|&x| x == 100.0), "band 0 did not move");
    assert!(b.iter().all(|&x| x == -6.0), "band 1 took the write");

    // A bare `UnitParam` is index 0.
    s.graph().set_param(node, UnitParam::Threshold, -12.0);
    let [a, b] = render(&mut s, 256);
    assert!(a.iter().all(|&x| x == -12.0));
    assert!(b.iter().all(|&x| x == -6.0));

    // An index the node lacks takes nothing.
    s.graph()
        .set_param(node, UnitAddr::new(UnitParam::Threshold, 2), 99.0);
    let [a, b] = render(&mut s, 256);
    assert!(a.iter().all(|&x| x == -12.0) && b.iter().all(|&x| x == -6.0));
}

/// The graph declares each index modulatable on its own.
///
/// Mutation (run): `ParamPorts::index_of` comparing the kind alone → index 2
/// is declared → fails.
#[test]
fn the_graph_declares_each_index() {
    let mut s = offline();
    let (node, _) = s.graph().add(Bands::new(0.0, 0.0));
    s.update();
    assert!(s.graph().declares_param(node, BAND0));
    assert!(s.graph().declares_param(node, BAND1));
    assert!(s.graph().declares_param(node, UnitParam::Threshold));
    assert!(
        !s.graph()
            .declares_param(node, UnitAddr::new(UnitParam::Threshold, 2))
    );
    assert!(!s.graph().declares_param(node, UnitParam::Ratio));
}

/// **A route to `Threshold` at index 1 modulates index 1 and leaves index 0
/// alone**: an LFO on a `0..=1000` target with knob 500 (the cell of
/// index 1, not index 0's 100) renders `500 + 300·sin` on output 1, past the
/// connect's declick, while output 0 reads its own base throughout. The
/// route's target is `ParamAddr::Indexed`, resolved through the session's
/// reconciler to the lane's knob and port.
///
/// Mutations (run), each fails:
/// - `ModHostOver::target` taking the knob from `p.cell(addr.param)` (the
///   index dropped: index 0's cell, 100) → output 1 swings about 100;
/// - `Bind::Native` building `ControlLaneNode::for_unit(at.param.param, ..)`
///   (index dropped) → the lane's events are for index 0: output 1 stays at
///   its base, output 0 swings;
/// - `ParamRef::unit_at` ignoring the index → the route targets index 0 →
///   output 0 swings.
#[test]
fn a_route_to_index_1_does_not_touch_index_0() {
    let mut s = offline();
    let (node, _) = s.graph().add(Bands::new(100.0, 500.0));
    s.graph().set_output_source(0, GraphSource::Node(node, 0));
    s.graph().set_output_source(1, GraphSource::Node(node, 1));

    let band1 = ParamRef::unit_at(node, UnitParam::Threshold, 1);
    assert_eq!(
        band1.param,
        ParamAddr::unit(BAND1),
        "the route names the indexed address"
    );
    assert_eq!(ParamRef::new(node, BAND1), band1, "and so does `new`");
    assert_eq!(
        ParamRef::unit_at(node, UnitParam::Threshold, 0),
        ParamRef::unit(node, UnitParam::Threshold),
        "index 0 is the bare param"
    );
    let mut m = s.mods();
    m.range(band1, 0.0, 1_000.0);
    let lfo = m.lfo(Lfo::sine().rate(BeatDuration(0.5)));
    m.route(lfo, band1, Depth(0.3));
    for _ in 0..4 {
        s.update();
    }
    assert_eq!(s.mod_state().refusal(), None, "the declarations apply");
    assert_eq!(s.mod_state().unresolved_targets(), &[], "and resolve");

    // 120 BPM: one cycle per half beat is 4 Hz.
    let [a, b] = render(&mut s, 24_000);
    assert!(a.iter().all(|&x| x == 100.0), "band 0 keeps its own base");
    let h = std::f64::consts::TAU * 4.0 * STEP as f64 / RATE;
    let tol = h * h / 8.0 * 1000.0 * 0.3 + 1e-3;
    for (f, &v) in b.iter().enumerate().skip(DECLICK) {
        let want = 500.0 + 300.0 * (std::f64::consts::TAU * 4.0 * f as f64 / RATE).sin();
        assert!(
            (f64::from(v) - want).abs() < tol,
            "band 1, frame {f}: {v} vs {want}"
        );
    }
    let (lo, hi) = b
        .iter()
        .fold((f32::MAX, f32::MIN), |(l, h), &x| (l.min(x), h.max(x)));
    assert!(lo < 250.0 && hi > 750.0, "it swings about 500: {lo}..{hi}");
}
