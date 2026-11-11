//! The session's node modes (doc 013, "Bypass and mute are graph state", M2's
//! session half): `Graph::set_mode` and `Graph::mode` over the executor's
//! `NodeMode` (M1, `tutti-graph/tests/mode.rs`, pins the fades, the bypass
//! line and the parking; this suite pins what the session adds: the call
//! reaches the running unit, is a control and not an edit, and refuses a
//! node that is not there).
//!
//! Every figure is analytic: a constant `A` through a node that halves it, so
//! the active output is `A / 2`, the bypassed output is `A` (the node has no
//! latency, so its aligned input is its input) and the muted output is `+0.0`,
//! each exact in `f32`.

use tutti_graph::{Cx, ForkByClone, Io, Node, NodeKey, Prepare, Shape, Status};
use tutti_nodes::testing::Const;
use tutti_session::{GraphSource, ModeError, NodeMode, Session};
use tutti_types::{ChannelLayout, InterleavedMut, SampleRate, Samples};

const A: f32 = 0.25;
const BLOCK: usize = 64;
/// More than `MODE_DECLICK` (256): a mode set before these blocks has
/// finished its fade by the last one.
const SETTLE_BLOCKS: usize = 6;

/// `out = in * 0.5`: a node whose output differs from its input, so a bypass
/// is audible.
#[derive(Clone)]
struct Halve;

impl Node for Halve {
    fn shape(&self) -> Shape {
        Shape::through_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let (ins, mut outs) = io.split();
        for (o, &x) in outs.get(0).iter_mut().zip(ins.get(0)) {
            *o = x * 0.5;
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

struct Rig {
    s: Session,
    halve: NodeKey,
}

/// `Const(A) -> Halve -> output 0`, committed once.
fn rig() -> Rig {
    let mut s = Session::builder()
        .build_offline(SampleRate(48_000.0), ChannelLayout::STEREO, Samples(1_024))
        .expect("builds");
    let g = s.graph();
    let (a, _) = g.insert(Const::mono(A));
    let (halve, _) = g.insert(ForkByClone(Halve));
    g.set_source(halve, 0, GraphSource::Node(a, 0));
    g.set_output_source(0, GraphSource::Node(halve, 0));
    s.update();
    assert!(!s.graph_ref().is_edited());
    Rig { s, halve }
}

impl Rig {
    /// Runs `update` and enough blocks for a fade to finish; returns the last
    /// block's channel 0.
    fn settled(&mut self) -> Vec<f32> {
        self.s.update();
        let mut block = vec![f32::NAN; BLOCK * 2];
        for _ in 0..SETTLE_BLOCKS {
            self.s
                .render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO));
        }
        block.iter().step_by(2).copied().collect()
    }
}

fn all_are(block: &[f32], want: f32) -> bool {
    block.iter().all(|x| x.to_bits() == want.to_bits())
}

/// Each mode's steady output, through the session: active `A / 2`, bypassed
/// `A`, muted `+0.0`, and active again `A / 2`.
#[test]
fn each_mode_reaches_the_running_node() {
    let mut r = rig();
    assert!(all_are(&r.settled(), A * 0.5), "active halves");

    r.s.graph().set_mode(r.halve, NodeMode::Bypassed).unwrap();
    assert!(all_are(&r.settled(), A), "bypassed passes the input");

    r.s.graph().set_mode(r.halve, NodeMode::Muted).unwrap();
    assert!(all_are(&r.settled(), 0.0), "muted is +0.0");

    r.s.graph().set_mode(r.halve, NodeMode::Active).unwrap();
    assert!(all_are(&r.settled(), A * 0.5), "active again");
}

/// `mode` reads back what `set_mode` stored, and is `Active` for a node never
/// given one.
#[test]
fn mode_reads_back_what_was_set() {
    let mut r = rig();
    assert_eq!(r.s.graph_ref().mode(r.halve), NodeMode::Active);
    r.s.graph().set_mode(r.halve, NodeMode::Bypassed).unwrap();
    assert_eq!(r.s.graph_ref().mode(r.halve), NodeMode::Bypassed);
    r.s.graph().set_mode(r.halve, NodeMode::Muted).unwrap();
    assert_eq!(r.s.graph_ref().mode(r.halve), NodeMode::Muted);
}

/// A mode is a control: setting one marks nothing edited, and the next
/// update sends no commit (the plan last sent is the same `Arc`).
#[test]
fn a_mode_is_not_an_edit() {
    let mut r = rig();
    let plan = r.s.graph_ref().sent_plan().expect("a plan");
    let sent = r.s.graph_ref().commits_sent();

    r.s.graph().set_mode(r.halve, NodeMode::Bypassed).unwrap();
    assert!(!r.s.graph_ref().is_edited(), "a mode marks nothing edited");
    let _ = r.settled();

    assert_eq!(r.s.graph_ref().commits_sent(), sent, "no commit was sent");
    let now = r.s.graph_ref().sent_plan().expect("a plan");
    assert!(std::sync::Arc::ptr_eq(&plan, &now), "the same plan");
}

/// A node that is not in the graph, or no longer is, is refused, and reads
/// `Active`.
#[test]
fn a_missing_node_is_refused() {
    let mut r = rig();
    let stranger = NodeKey::fresh();
    assert_eq!(
        r.s.graph().set_mode(stranger, NodeMode::Muted),
        Err(ModeError::NoSuchNode { node: stranger })
    );
    assert_eq!(r.s.graph_ref().mode(stranger), NodeMode::Active);

    r.s.graph().set_mode(r.halve, NodeMode::Muted).unwrap();
    assert!(r.s.graph().remove(r.halve));
    assert_eq!(
        r.s.graph().set_mode(r.halve, NodeMode::Bypassed),
        Err(ModeError::NoSuchNode { node: r.halve })
    );
    assert_eq!(
        r.s.graph_ref().mode(r.halve),
        NodeMode::Active,
        "a removed node's mode goes with it"
    );
}
