//! `DesignNode<FdnDesign>` as the graph runs it (design 022 step 1c): its ports
//! (the freeze only where the design can freeze), its tail (the network's own,
//! unbounded while frozen), the freeze itself, a modulated decay, and
//! `ReverbKind`. Every figure is from the network's own queries or the
//! definition of a freeze, not from another reverb's render.

use std::sync::Arc;

use tutti_dsp::{Design, FdnControls, FdnDesign, System};
use tutti_graph::contract::{Driven, assert_honest_tail, assert_param_fork, drive_input, prepared};
use tutti_graph::{
    Env, GraphBuilder, Node, ParamIn, ParamNode, PlayClock, Prepare, Status, Transport,
    TransportChanges,
};
use tutti_modulation::ControlProgram;
use tutti_nodes::automation::ControlLaneNode;
use tutti_nodes::{DesignNode, FdnNode, ReverbKind, SpawnedReverb};
use tutti_types::{
    AtomicF32, ChannelLayout, Db, Frame, Mix, SampleRate, Samples, Seconds, Tail, UnitAddr,
    UnitParam,
};

const SR: SampleRate = SampleRate(48_000.0);
const BLOCK: usize = 64;

fn plain() -> FdnDesign {
    FdnDesign::default()
}

fn freezable() -> FdnDesign {
    FdnDesign::new(8, Seconds(8.0), true).unwrap()
}

fn node(design: FdnDesign, c: FdnControls) -> FdnNode {
    prepared(DesignNode::live(design, c), SR, BLOCK)
}

struct Rig {
    node: FdnNode,
    frame: u64,
}

impl Rig {
    fn new(node: FdnNode) -> Self {
        Self { node, frame: 0 }
    }

    fn block(&mut self, input: &[f32], params: &[Option<&[f32]>]) -> Driven {
        let env = Env {
            frame: Frame(self.frame),
            sample_rate: SR,
            block_len: Samples(input.len()),
            transport: Transport::default(),
            changes: TransportChanges::NONE,
            play: PlayClock::ZERO,
        };
        self.frame += input.len() as u64;
        drive_input(&mut self.node, &env, &[input], params, &[])
    }

    fn silent(&mut self) -> Driven {
        self.block(&[0.0; BLOCK], &[])
    }
}

fn noise(seed: u32, frames: usize) -> Vec<f32> {
    let mut state = seed.wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
    (0..frames)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            0.5 * ((state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0)
        })
        .collect()
}

fn impulse() -> Vec<f32> {
    let mut x = vec![0.0; BLOCK];
    x[0] = 1.0;
    x
}

fn finite(d: &Driven) -> usize {
    match d.reported_tail {
        Some(Tail::Finite(n)) => n.get(),
        other => panic!("a finite reported tail, not {other:?}"),
    }
}

/// The network's own tail at -60 dB in frames at 48 kHz.
fn tail_of(design: &FdnDesign, c: &FdnControls) -> usize {
    let tutti_dsp::Ring::Finite(s) = design.design(c, SR).tail(Db(-60.0)) else {
        panic!("finite")
    };
    (s.get() * SR.get()).ceil() as usize
}

fn energy(blocks: &[Driven]) -> f64 {
    blocks
        .iter()
        .flat_map(|d| d.audio.iter().flatten())
        .map(|&s| f64::from(s).powi(2))
        .sum()
}

/// A design that cannot freeze offers the mix and ten params and a finite tail
/// ceiling; one that can offers an eleventh, the freeze, and an unbounded
/// ceiling (so an export spends its cap: freeze is opt-in for that reason). The
/// addresses are the doc's: three decays at indices 0 (mid), 1 (low), 2 (high).
///
/// Mutation (run): offering the freeze from a design that cannot freeze fails
/// the first count; a finite ceiling for a freezable one fails the tail row.
#[test]
fn the_freeze_is_a_port_only_where_the_design_can_freeze() {
    let (p, f) = (
        node(plain(), FdnControls::DEFAULT),
        node(freezable(), FdnControls::DEFAULT),
    );
    assert_eq!(p.shape().params.len(), 11);
    assert_eq!(f.shape().params.len(), 12);
    assert!(matches!(p.shape().tail, Tail::Finite(_)));
    assert_eq!(f.shape().tail, Tail::Unbounded);
    let (set_p, set_f) = (p.param_set(), f.param_set());
    assert_eq!(set_p.get(UnitParam::Freeze), None);
    assert_eq!(set_f.get(UnitParam::Freeze), Some(0.0));
    for (index, secs) in [(0u8, 2.0), (1, 2.4), (2, 1.2)] {
        assert_eq!(
            set_p.get(UnitAddr::new(UnitParam::Decay, index)),
            Some(secs)
        );
    }
    assert_eq!(set_p.get(UnitParam::Wet), Some(1.0));
    let Tail::Finite(ceiling) = p.shape().tail else {
        unreachable!()
    };
    assert!(
        ceiling.get() as f64 / SR.get() > 8.0,
        "the room at its longest decay"
    );
}

/// The reported tail is the network's own `tail(-60 dB)` at the controls in
/// force, and past it the output is exactly silent, at a short and a long decay
/// (the harness's check, which also looks past the ceiling).
///
/// Mutation (run): not calling `report_tail` leaves `reported_tail` empty and
/// fails; a node that reports half the tail fails `assert_honest_tail`.
#[test]
fn the_reported_tail_is_the_networks_and_honest() {
    for decay in [0.3, 1.0, 2.5] {
        let c = FdnControls {
            decay: [Seconds(decay); 3],
            ..FdnControls::DEFAULT
        };
        let mut rig = Rig::new(node(plain(), c));
        assert_eq!(finite(&rig.block(&impulse(), &[])), tail_of(&plain(), &c));
        assert_honest_tail(&mut node(plain(), c), SR);
    }
}

/// A freeze reports an unbounded tail and holds: with the input silent the level
/// a second after the freeze and four seconds later differ by under half a dB,
/// and the node is never parked. Thawed, it reports a finite tail again, decays
/// and is eventually idle.
///
/// Mutation (run): not handling the thaw in `process` (the unbounded ring kept
/// for good) fails the last loop with a node that never parks.
#[test]
fn a_freeze_holds_and_a_thaw_lets_go() {
    let mut rig = Rig::new(node(freezable(), FdnControls::DEFAULT));
    let cells = rig.node.param_set();
    for chunk in noise(1, 20_000).chunks(BLOCK) {
        rig.block(chunk, &[]);
    }
    assert!(cells.set(UnitParam::Freeze, 1.0));
    let run = |rig: &mut Rig, seconds: f64| -> Vec<Driven> {
        (0..(seconds * SR.get()) as usize / BLOCK)
            .map(|_| rig.silent())
            .collect()
    };
    let _ramp = run(&mut rig, 0.3);
    let early = run(&mut rig, 0.2);
    assert!(
        early
            .iter()
            .all(|d| d.reported_tail == Some(Tail::Unbounded))
    );
    let _between = run(&mut rig, 4.0);
    let late = run(&mut rig, 0.2);
    let db = |a: f64, b: f64| 10.0 * (b / a).log10();
    assert!(db(energy(&early), energy(&late)).abs() < 0.5, "held");
    assert!(
        late.iter().all(|d| d.status == Some(Status::Modified)),
        "never parked"
    );

    assert!(cells.set(UnitParam::Freeze, 0.0));
    let thawed = run(&mut rig, 0.1);
    assert!(thawed.last().unwrap().reported_tail != Some(Tail::Unbounded));
    let fall = run(&mut rig, 12.0);
    assert!(
        fall.last().unwrap().status == Some(Status::Idle),
        "parked after its tail"
    );
    assert!(
        energy(&fall[..BLOCK]) > 1e3 * energy(&fall[fall.len() - 10..]),
        "it decayed"
    );
}

/// A modulated decay port is read at the block's last frame, as the cell is, and
/// the tail is reported from the longest decay the port takes in the block, so a
/// decay that falls through the block is not reported short.
///
/// Mutation (run): dropping `longest` for the FDN reports the last frame's short
/// decay and fails the second assertion; reading the port's first frame fails
/// the first.
#[test]
fn a_modulated_decay_is_read_at_the_end_and_its_peak_sets_the_tail() {
    let x = noise(3, 4_096);
    let rising: Vec<f32> = (0..BLOCK)
        .map(|i| 0.5 + 2.0 * i as f32 / (BLOCK - 1) as f32)
        .collect();
    let mut by_port = Rig::new(node(plain(), FdnControls::DEFAULT));
    let mut by_cell = Rig::new(node(
        plain(),
        FdnControls {
            decay: [
                Seconds(2.5),
                FdnControls::DEFAULT.decay[1],
                FdnControls::DEFAULT.decay[2],
            ],
            ..FdnControls::DEFAULT
        },
    ));
    let none: Option<&[f32]> = None;
    // Port 1 is the mid decay (after the mix at port 0).
    for chunk in x.chunks(BLOCK) {
        let a = by_port.block(chunk, &[none, Some(&rising)]).audio;
        let b = by_cell.block(chunk, &[]).audio;
        assert_eq!(a, b);
    }
    let falling: Vec<f32> = rising.iter().rev().copied().collect();
    let mut rig = Rig::new(node(plain(), FdnControls::DEFAULT));
    let d = rig.block(
        &impulse(),
        &[none, Some(&falling), Some(&falling), Some(&falling)],
    );
    let at_peak = FdnControls {
        decay: [Seconds(2.5); 3],
        ..FdnControls::DEFAULT
    };
    assert!(
        finite(&d) >= tail_of(&plain(), &at_peak),
        "{} against the peak's",
        finite(&d)
    );
    // The size and the pre-delay lengthen the tail the same way: a room that is
    // large early in the block and small at its end, a pre-delay likewise.
    let big = FdnControls {
        size: Mix(1.0),
        pre_delay: Seconds(0.2),
        ..FdnControls::DEFAULT
    };
    let (high, low): (Vec<f32>, Vec<f32>) = (
        (0..BLOCK).map(|i| if i < 8 { 1.0 } else { 0.1 }).collect(),
        (0..BLOCK).map(|i| if i < 8 { 0.2 } else { 0.0 }).collect(),
    );
    let mut rig = Rig::new(node(plain(), FdnControls::DEFAULT));
    let d = rig.block(
        &impulse(),
        &[none, none, none, none, Some(&high), none, Some(&low)],
    );
    assert!(
        finite(&d) >= tail_of(&plain(), &big),
        "{} against the large room's {}",
        finite(&d),
        tail_of(&plain(), &big)
    );
}

/// A real control lane in a real graph reaches the decay: with the cell at its
/// default, a lane holding the mid decay at 6 s makes a noise burst ring far
/// longer than one holding it at 0.3 s.
///
/// Mutation (run): the node reading its cell instead of the port makes the two
/// renders identical and fails.
#[test]
fn a_lane_driving_the_decay_changes_how_long_it_rings() {
    let ring = |decay: f32| -> f64 {
        let node = DesignNode::live(plain(), FdnControls::DEFAULT);
        let mut g = GraphBuilder::new(ChannelLayout::MONO, ChannelLayout::STEREO);
        let (n, _) = g.add_with_controls(node);
        g.connect_input(0, n, 0).connect_output(n, 0, 0);
        g.connect_output(n, 1, 1);
        let mut r = g
            .renderer(Prepare::new(SR, Samples(100)))
            .expect("the graph builds");
        let at = ParamIn {
            node: n,
            param: UnitAddr::new(UnitParam::Decay, 0),
        };
        let program = ControlProgram::new(None, vec![], vec![]).expect("a program");
        let lane = ControlLaneNode::for_unit(
            UnitParam::Decay,
            program,
            Some(Arc::new(AtomicF32::new(decay)).into()),
            None,
        );
        let key = tutti_graph::NodeKey(9_000);
        let editor = r.editor_mut();
        editor.insert(key, "lane", lane);
        editor
            .spec_mut()
            .connect_param(at, tutti_graph::EventOut { node: key, port: 0 });
        editor.commit().expect("the lane commits");
        let frames = 96_000;
        let mut x = noise(8, 4_800);
        x.resize(frames, 0.0);
        let out = r.render_input(&[&x]);
        let energy = |from: usize, to: usize| -> f64 {
            out.iter()
                .flat_map(|c| &c[from..to])
                .map(|&s| f64::from(s).powi(2))
                .sum()
        };
        energy(72_000, 90_000) / energy(12_000, 30_000)
    };
    let (short, long) = (ring(0.3), ring(6.0));
    assert!(
        long > 100.0 * short,
        "6 s keeps {long} of its energy, 0.3 s {short}"
    );
}

/// A fork starts from the values set, shares no cell and rings nothing: the
/// harness's fork checks over every cell of both designs.
///
/// Mutation (run): not detaching the freeze or the width in
/// `Params::detach` fails `assert_param_fork`.
#[test]
fn a_fork_shares_no_cell() {
    assert_param_fork(DesignNode::live(plain(), FdnControls::DEFAULT));
    assert_param_fork(DesignNode::live(freezable(), FdnControls::DEFAULT));
}

/// A non-finite write to any cell reads as unchanged: the render equals one that
/// never saw it.
///
/// Mutation (run): reading a cell without `LastGood` designs a default room for
/// the NaN and differs from 0.9 s.
#[test]
fn a_non_finite_write_is_ignored() {
    let c = FdnControls {
        decay: [Seconds(0.9); 3],
        ..FdnControls::DEFAULT
    };
    let (mut hostile, mut calm) = (Rig::new(node(plain(), c)), Rig::new(node(plain(), c)));
    let set = hostile.node.param_set();
    for (i, chunk) in noise(4, 8_192).chunks(BLOCK).enumerate() {
        if i == 3 {
            for addr in set.params().collect::<Vec<_>>() {
                if addr.param != UnitParam::Wet {
                    set.set(addr, f32::NAN);
                }
            }
        }
        assert_eq!(
            hostile.block(chunk, &[]).audio,
            calm.block(chunk, &[]).audio
        );
    }
}

/// `ReverbKind` spawns the matching node and nothing else.
///
/// Mutation (run): swapping the two arms fails both.
#[test]
fn a_kind_spawns_its_node() {
    let SpawnedReverb::Freeverb(f) = ReverbKind::Freeverb.spawn() else {
        panic!("a Freeverb")
    };
    assert_eq!(
        f.param_set().params().count(),
        3,
        "the mix, the room, the damping"
    );
    let SpawnedReverb::Fdn(n) = ReverbKind::Fdn(freezable()).spawn() else {
        panic!("an FDN")
    };
    assert_eq!(n.param_set().params().count(), 12);
    assert_eq!(n.shape().audio_out, ChannelLayout::STEREO);
}
