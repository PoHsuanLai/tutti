//! A node's params are addressed as the node goes in, so a modulation lane's
//! knob is the node's own cell.
//!
//! The session half of `bevy-tutti`'s `params_at_insert` suite, which pinned
//! the three ECS insertion paths (`spawn_audio_node`, `insert_audio_node`,
//! `crossfade_audio_node`); all three go through `Graph::add` and
//! `Graph::crossfade`, so these pin those: a lane finds a native param's knob
//! through the node's `ParamSet`, which the insertion keeps by key
//! (`SessionNode::params`, `set_node_params`). They stand at the insertion
//! paths rather than at a fixture that addresses by hand, because a path that
//! forgot to would leave every hand-built suite green. The nodes are the
//! suites' own `ProbeUnit` of `Drive` (a host's `ParamNode`) and an engine
//! node, `DistortionNode`.

use crate::common;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use tutti_types::MaxBlock;

use tutti_graph::CrossfadeCurve;
use tutti_graph::NodeKey;
use tutti_nodes::{DistortionNode, ShapeKind};
use tutti_session::{GraphSource, ParamRef, Session};
use tutti_types::AtomicF32;
use tutti_types::{ChannelLayout, Depth, Drive, SampleRate, Samples, Seconds, UnitParam};

use common::probe::ProbeUnit;

const BASE: f32 = 5.0;

fn session() -> Session {
    Session::builder()
        .build_offline(
            SampleRate(48_000.0),
            ChannelLayout::MONO,
            MaxBlock::of(Samples(1_024)),
        )
        .expect("builds")
}

/// A macro on `node`'s drive, `0..=10`, and updates until it applies.
fn modulate_drive(s: &mut Session, node: NodeKey) {
    let drive = ParamRef::unit(node, UnitParam::Drive);
    let mut m = s.mods();
    m.range(drive, 0.0, 10.0);
    let offset = m.macro_(1.0);
    m.route(offset, drive, Depth(0.1)).unwrap();
    settle(s);
}

fn settle(s: &mut Session) {
    for _ in 0..4 {
        s.update();
    }
    assert_eq!(s.mod_state().refusal(), None, "the declarations apply");
}

/// The knob of `node`'s drive lane, `None` with no lane or no knob.
fn knob(s: &Session, node: NodeKey) -> Option<f32> {
    s.mod_state()
        .lane(ParamRef::unit(node, UnitParam::Drive).key())
        .and_then(|(_, lane)| lane.knob())
}

/// The lane's knob reads `cell`: what it holds, and a write into it.
fn assert_knob_is(s: &Session, node: NodeKey, cell: &Arc<AtomicF32>) {
    assert_eq!(knob(s, node), Some(cell.load(Ordering::Acquire)));
    cell.store(7.5, Ordering::Release);
    assert_eq!(knob(s, node), Some(7.5), "the knob is the node's cell");
}

#[test]
fn add_addresses_a_hosts_param_node() {
    let mut s = session();
    let unit = ProbeUnit::of(UnitParam::Drive, BASE);
    let cell = unit.knob();
    let (node, _) = s.graph().add(unit);
    modulate_drive(&mut s, node);
    assert_knob_is(&s, node, &cell);
}

#[test]
fn add_addresses_an_engine_node() {
    let mut s = session();
    let node = DistortionNode::new(ShapeKind::Tanh, Drive::new(BASE));
    let cell = node.drive();
    let (key, _) = s.graph().add(node);
    modulate_drive(&mut s, key);
    assert_knob_is(&s, key, &cell);
}

/// After a crossfade the lane's knob is the **incoming** node's cell: a
/// write into it moves the knob, and one into the outgoing node's does not.
#[test]
fn a_crossfade_hands_the_lane_the_incoming_nodes_cell() {
    let mut s = session();
    let outgoing = ProbeUnit::of(UnitParam::Drive, BASE);
    let old = outgoing.knob();
    let (node, _) = s.graph().add(outgoing);
    s.graph().set_output_source(0, GraphSource::Node(node, 0));
    modulate_drive(&mut s, node);
    assert_eq!(knob(&s, node), Some(BASE), "precondition: the lane");

    let incoming = ProbeUnit::of(UnitParam::Drive, 3.0);
    let new = incoming.knob();
    s.graph()
        .crossfade(
            node,
            incoming,
            Seconds(0.005),
            CrossfadeCurve::EqualAmplitude,
        )
        .expect("the crossfade starts");
    settle(&mut s);
    old.store(9.0, Ordering::Release);
    assert_eq!(knob(&s, node), Some(3.0), "not the outgoing cell");
    assert_knob_is(&s, node, &new);
}
