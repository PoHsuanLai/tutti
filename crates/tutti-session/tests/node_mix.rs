//! The session's node mixes (doc 013, "Bypass and mute are graph state", M4's
//! session half): `Graph::set_mix` and `Graph::mix` over the executor's blend
//! of a node's output with its aligned input (`tutti-graph/tests/mix.rs` pins
//! the formula, the ramp, the ends and the composition with modes; this suite
//! pins what the session adds: the call reaches the running unit, is a
//! control and not an edit, and refuses a node that is not there or a mix off
//! the scale).
//!
//! Every figure is analytic: a constant `A` through a node that halves it, so
//! the WET output is `A / 2`, the DRY output is `A` (the node has no latency,
//! so its aligned input is its input) and a mix of one half is
//! `A / 2 * 0.5 + A * 0.5`, each exact in `f32`.

use tutti_graph::{CrossfadeCurve, Cx, ForkByClone, Io, Node, NodeKey, Prepare, Shape, Status};
use tutti_nodes::testing::Const;
use tutti_session::{GraphSource, MixError, NodeMode, Session};
use tutti_types::MaxBlock;
use tutti_types::{ChannelLayout, InterleavedMut, Mix, SampleRate, Samples, Seconds};

const A: f32 = 0.25;
const BLOCK: usize = 64;
/// Blocks after a mix set: its ramp (one block) is over by the last.
const SETTLE_BLOCKS: usize = 3;

/// `out = in * 0.5`: a node whose output differs from its input, so the
/// blend is audible.
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
        .build_offline(
            SampleRate(48_000.0),
            ChannelLayout::STEREO,
            MaxBlock::of(Samples(1_024)),
        )
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
    /// Runs `update` and enough blocks for a ramp to finish; returns the last
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

/// Each mix's steady output, through the session: WET `A / 2`, DRY `A`, one
/// half `A / 2 * 0.5 + A * 0.5`, and WET again.
///
/// Mutation (run): `Graph::set_mix` returning `Ok(())` without calling the
/// editor -> the DRY block reads `A / 2` and the test fails.
#[test]
fn each_mix_reaches_the_running_node() {
    let mut r = rig();
    assert!(all_are(&r.settled(), A * 0.5), "WET is the node's output");

    r.s.graph().set_mix(r.halve, Mix::DRY).unwrap();
    assert!(all_are(&r.settled(), A), "DRY is the aligned input");

    r.s.graph().set_mix(r.halve, Mix::new(0.5)).unwrap();
    assert!(
        all_are(&r.settled(), A * 0.5 * 0.5 + A * (1.0 - 0.5)),
        "one half is the blend"
    );

    r.s.graph().set_mix(r.halve, Mix::WET).unwrap();
    assert!(all_are(&r.settled(), A * 0.5), "WET again");
}

/// The mode wins over the mix, through the session: a bypassed node at a mix
/// of one half is its aligned input, and muted is `+0.0`.
///
/// Mutation (run): the executor blending a Muted node (`NodeMode::Muted` arm
/// of `Segments::blend` calling `blend_active`) -> the muted block is not
/// silent.
#[test]
fn the_mode_wins_over_the_mix() {
    let mut r = rig();
    r.s.graph().set_mix(r.halve, Mix::new(0.5)).unwrap();
    r.s.graph().set_mode(r.halve, NodeMode::Bypassed).unwrap();
    // The mode's fade is 256 frames: more blocks than a mix needs.
    for _ in 0..6 {
        r.settled();
    }
    assert!(all_are(&r.settled(), A), "bypassed passes the input");
    r.s.graph().set_mode(r.halve, NodeMode::Muted).unwrap();
    for _ in 0..6 {
        r.settled();
    }
    assert!(all_are(&r.settled(), 0.0), "muted is +0.0");
}

/// `mix` reads back what `set_mix` stored, and is `WET` for a node never given
/// one.
///
/// Mutation (run): `Graph::mix` returning `Mix::WET` always -> the read-back
/// after the set fails.
#[test]
fn mix_reads_back_what_was_set() {
    let mut r = rig();
    assert_eq!(r.s.graph_ref().mix(r.halve), Mix::WET);
    r.s.graph().set_mix(r.halve, Mix::new(0.25)).unwrap();
    assert_eq!(r.s.graph_ref().mix(r.halve), Mix::new(0.25));
    r.s.graph().set_mix(r.halve, Mix::DRY).unwrap();
    assert_eq!(r.s.graph_ref().mix(r.halve), Mix::DRY);
}

/// A mix is a control: setting one marks nothing edited, and the next update
/// sends no commit (the plan last sent is the same `Arc`).
///
/// Mutation (run): `set_mix` also setting `self.edited = true` -> the
/// `is_edited` assertion fails.
#[test]
fn a_mix_is_not_an_edit() {
    let mut r = rig();
    let plan = r.s.graph_ref().sent_plan().expect("a plan");
    let sent = r.s.graph_ref().commits_sent();

    r.s.graph().set_mix(r.halve, Mix::DRY).unwrap();
    assert!(!r.s.graph_ref().is_edited(), "a mix marks nothing edited");
    let _ = r.settled();

    assert_eq!(r.s.graph_ref().commits_sent(), sent, "no commit was sent");
    let now = r.s.graph_ref().sent_plan().expect("a plan");
    assert!(std::sync::Arc::ptr_eq(&plan, &now), "the same plan");
}

/// A node that is not in the graph, or no longer is, is refused and reads
/// `WET`; a mix off the scale (NaN, negative, above one) is refused and
/// changes nothing.
///
/// Mutation (run): `Graph::set_mix` mapping the editor's refusal to `Ok(())`
/// -> the refusal assertions fail.
#[test]
fn a_missing_node_and_a_bad_mix_are_refused() {
    let mut r = rig();
    let stranger = NodeKey::fresh();
    assert_eq!(
        r.s.graph().set_mix(stranger, Mix::DRY),
        Err(MixError::NoSuchNode { node: stranger })
    );
    assert_eq!(r.s.graph_ref().mix(stranger), Mix::WET);

    r.s.graph().set_mix(r.halve, Mix::new(0.5)).unwrap();
    for bad in [f32::NAN, -0.5, 1.5] {
        assert_eq!(
            r.s.graph().set_mix(r.halve, Mix::new(bad)),
            Err(MixError::OutOfRange)
        );
    }
    assert_eq!(r.s.graph_ref().mix(r.halve), Mix::new(0.5));

    assert!(r.s.graph().remove(r.halve));
    assert_eq!(
        r.s.graph().set_mix(r.halve, Mix::DRY),
        Err(MixError::NoSuchNode { node: r.halve })
    );
    assert_eq!(
        r.s.graph_ref().mix(r.halve),
        Mix::WET,
        "a removed node's mix goes with it"
    );
}

/// `out = (in * 0.25, in * 0.25)`: one input, two outputs that name no
/// placement, so it has no pass (`PassError::Unplaced`) and nothing may bypass
/// or mix it: by index its input would be heard on the left alone.
#[derive(Clone)]
struct Quarter;

impl Node for Quarter {
    fn shape(&self) -> Shape {
        Shape::buses(
            tutti_graph::Buses::unassigned(ChannelLayout::MONO),
            tutti_graph::Buses::unassigned(ChannelLayout::STEREO),
        )
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let (ins, mut outs) = io.split();
        for c in 0..2 {
            for (o, &x) in outs.get(c).iter_mut().zip(ins.get(0)) {
                *o = x * 0.25;
            }
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// **A replace the graph refuses leaves the last plan sounding, and setting the
/// node's mix or mode back lets the next commit through** (doc 013, "As built
/// (B3)", reversed by #570). A node at a mix of one half, or bypassed, replaced
/// by one with no pass: the commit is refused (`CommitError::Unplaced`), so the
/// old node's figure goes on (`A/2 * 0.5 + A * 0.5`, or `A`), the graph stays
/// edited, and once the host sets the mix to WET or the mode to Active the next
/// update commits and the new node is heard alone (`A / 4`).
///
/// Mutation (each run, each failed): `check_passes` not called in `commit` →
/// the replace commits and the first figure is the new node's blend; the
/// session clearing the edit flag on a refused commit → the recovery never
/// commits and the last figure is the old node's.
#[test]
fn a_refused_replace_keeps_the_last_plan_until_the_mix_or_mode_is_set_back() {
    for bypass in [false, true] {
        let mut r = rig();
        let before = if bypass {
            r.s.graph().set_mode(r.halve, NodeMode::Bypassed).unwrap();
            A
        } else {
            r.s.graph().set_mix(r.halve, Mix::new(0.5)).unwrap();
            A * 0.5 * 0.5 + A * 0.5
        };
        for _ in 0..6 {
            r.settled();
        }
        assert!(all_are(&r.settled(), before), "bypass {bypass}: before");

        assert!(
            r.s.graph()
                .replace(
                    r.halve,
                    ForkByClone(Quarter),
                    Seconds(0.0),
                    CrossfadeCurve::EqualAmplitude
                )
                .is_ok()
        );
        for _ in 0..6 {
            r.settled();
        }
        let got = r.settled();
        assert!(
            all_are(&got, before),
            "bypass {bypass}: the refused commit leaves the last plan: {} want {before}",
            got[0]
        );
        assert!(
            r.s.graph_ref().is_edited(),
            "bypass {bypass}: still to commit"
        );

        if bypass {
            r.s.graph().set_mode(r.halve, NodeMode::Active).unwrap();
        } else {
            r.s.graph().set_mix(r.halve, Mix::WET).unwrap();
        }
        for _ in 0..6 {
            r.settled();
        }
        assert!(
            all_are(&r.settled(), A * 0.25),
            "bypass {bypass}: set back, the next commit places the new node"
        );
        assert!(!r.s.graph_ref().is_edited(), "bypass {bypass}: committed");
    }
}
