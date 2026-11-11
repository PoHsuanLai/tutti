//! `DesignNode` over Freeverb, as the graph runs it (design 022 step 1b): its
//! ports, its tail (a ceiling it narrows and honours), its controls (a cell, or
//! a port the graph modulates), and its fork. Every figure is from the
//! definition (the network's own analytic tail, an impulse's first echo, a
//! decay measured on a real lane), not from another reverb's render.

use std::sync::Arc;

use tutti_dsp::{
    Ceiling, Design, Effect, FdnControls, FdnDesign, Freeverb, FreeverbControls, FreeverbDesign,
    Realization, Setup, System,
};
use tutti_fixtures::Noise;
use tutti_graph::contract::{assert_honest_tail, assert_param_fork, drive_input, prepared};
use tutti_graph::{
    Env, GraphBuilder, IntoNode, Node, ParamFork, ParamIn, ParamNode, Prepare, Status, Transport,
    TransportChanges,
};
use tutti_modulation::ControlProgram;
use tutti_nodes::automation::ControlLaneNode;
use tutti_nodes::{DesignNode, FdnNode, FreeverbNode};
use tutti_types::{
    AtomicF32, ChannelLayout, Db, Frame, MaxBlock, Mix, SampleRate, Samples, Tail, UnitAddr,
    UnitParam,
};

const SR: SampleRate = SampleRate(48_000.0);
const BLOCK: usize = 64;

fn gm() -> FreeverbControls {
    FreeverbControls::GENERAL_MIDI
}

fn controls(room: f32, damp: f32) -> FreeverbControls {
    FreeverbControls {
        room: Mix(room),
        damp: Mix(damp),
    }
}

/// A live (modulatable) Freeverb node at `c`, prepared at 48 kHz.
fn live(c: FreeverbControls) -> FreeverbNode {
    prepared(DesignNode::live(FreeverbDesign, c), SR, BLOCK)
}

/// A node driven block by block, at its own running frame.
struct Rig<N> {
    node: N,
    frame: u64,
}

impl<N: Node> Rig<N> {
    fn new(node: N) -> Self {
        Self { node, frame: 0 }
    }

    fn block(&mut self, input: &[f32], params: &[Option<&[f32]>]) -> tutti_graph::contract::Driven {
        let env = Env {
            frame: Frame(self.frame),
            sample_rate: SR,
            block_len: Samples(input.len()),
            transport: Transport::default(),
            changes: TransportChanges::NONE,
            play: tutti_graph::PlayClock::ZERO,
        };
        self.frame += input.len() as u64;
        drive_input(&mut self.node, &env, &[input], params, &[])
    }

    fn silent(&mut self) -> tutti_graph::contract::Driven {
        self.block(&[0.0; BLOCK], &[])
    }
}

fn noise(seed: u32, frames: usize) -> Vec<f32> {
    Noise::seeded(seed).take(frames).map(|x| 0.5 * x).collect()
}

fn impulse() -> Vec<f32> {
    let mut x = vec![0.0; BLOCK];
    x[0] = 1.0;
    x
}

fn reported(d: &tutti_graph::contract::Driven) -> usize {
    match d.reported_tail {
        Some(Tail::Finite(n)) => n.get(),
        other => panic!("a finite reported tail, not {other:?}"),
    }
}

/// The network's own tail at -60 dB, in frames at 48 kHz: the figure the node
/// reports for `c`, from the network's analytic query.
fn tail_of(c: FreeverbControls) -> usize {
    let ring = FreeverbDesign.design(&c, SR).tail(Db(-60.0));
    let tutti_dsp::Ring::Finite(s) = ring else {
        panic!("finite")
    };
    (s.get() * SR.get()).ceil() as usize
}

/// The General MIDI send takes a mono input and gives a stereo one, with no
/// latency, and offers no cell to the graph (its dry/wet is the graph's mix); a
/// live node offers the room and the damping. Its tail is a ceiling.
#[test]
fn the_ports_and_the_ceiling() {
    let fixed = prepared(FreeverbNode::general_midi(), SR, BLOCK);
    let shape = fixed.shape();
    assert_eq!(shape.audio_in(), ChannelLayout::MONO);
    assert_eq!(shape.audio_out(), ChannelLayout::STEREO);
    assert_eq!(shape.latency.samples(), Samples::ZERO);
    assert_eq!(shape.params.len(), 0, "no cells");
    assert_eq!(shape.tail, Tail::Finite(Samples(tail_of(gm()))));

    let live = live(gm());
    let shape = live.shape();
    let ports = shape.params.len();
    assert_eq!(ports, 2, "the room and the damping");
    let Tail::Finite(ceiling) = shape.tail else {
        panic!("finite")
    };
    assert!(
        ceiling.get() > 2 * tail_of(gm()),
        "the live ceiling is the worst case (room 1), far above the General MIDI room's"
    );
    // The fixed node's ceiling is exact, the live node's is not.
    assert_eq!(live.param_set().params().count(), 2);
    assert_eq!(fixed.param_set().params().count(), 0);
    assert_eq!(
        live.param_set().get(UnitParam::Wet),
        None,
        "wet only: the blend is the graph's mix, not a port"
    );
}

/// The ports are in a fixed order, which the graph's wires and a saved project
/// name by index: the design's cells in the order its params list them. Freeverb
/// offers `[RoomSize, Damping]`; the FDN offers the three decays, the size, the diffusion, the pre-delay, the
/// modulation depth and rate, the width and the wet gain.
#[test]
fn the_port_order_is_the_cells_in_their_order() {
    let freeverb = live(gm());
    assert_eq!(
        freeverb.shape().params.as_slice(),
        [
            UnitAddr::first(UnitParam::RoomSize),
            UnitAddr::first(UnitParam::Damping),
        ]
    );
    let fdn: FdnNode = prepared(
        DesignNode::live(FdnDesign::default(), FdnControls::DEFAULT),
        SR,
        BLOCK,
    );
    assert_eq!(
        fdn.shape().params.as_slice(),
        [
            UnitAddr::new(UnitParam::Decay, 0),
            UnitAddr::new(UnitParam::Decay, 1),
            UnitAddr::new(UnitParam::Decay, 2),
            UnitAddr::first(UnitParam::RoomSize),
            UnitAddr::first(UnitParam::Diffusion),
            UnitAddr::first(UnitParam::DelayTime),
            UnitAddr::first(UnitParam::Depth),
            UnitAddr::first(UnitParam::Rate),
            UnitAddr::first(UnitParam::StereoSpread),
            UnitAddr::new(UnitParam::GainDb, 0),
        ]
    );
}

/// A live node narrows its ceiling each block to the tail of the network it is
/// running, from the network's own `tail` query at -60 dB: a smaller room reports
/// less than a bigger one, and both less than the ceiling.
#[test]
fn a_live_node_reports_the_tail_of_its_current_network() {
    let mut last = 0;
    let ceiling = match live(gm()).shape().tail {
        Tail::Finite(n) => n.get(),
        t => panic!("{t:?}"),
    };
    for room in [0.1, 0.5, 0.9] {
        let mut rig = Rig::new(live(controls(room, 0.5)));
        let first = rig.block(&impulse(), &[]);
        let got = reported(&first);
        assert_eq!(got, tail_of(controls(room, 0.5)), "room {room}");
        assert!(got > last && got < ceiling, "room {room}: {got}");
        last = got;
    }
}

/// The reported tail holds: past it the output is exactly silent, for the fixed
/// send and a live node at a short and a long room (the harness's check, which
/// also looks past the ceiling).
#[test]
fn the_reported_tail_is_honest() {
    assert_honest_tail(&mut prepared(FreeverbNode::general_midi(), SR, BLOCK), SR);
    assert_honest_tail(&mut live(controls(0.1, 0.5)), SR);
    assert_honest_tail(&mut live(controls(0.9, 0.2)), SR);
}

/// What the node cuts off at the tail is under the floor: the realization alone,
/// left to ring, is below -60 dB of its first echo (the loudest frame of an
/// impulse's answer) at every frame past the tail the node reports, so parking
/// the room there takes nothing audible. (The realization's own render, held to the
/// network's figure; no other reverb's.)
#[test]
fn the_cut_discards_only_what_is_below_the_floor() {
    for c in [gm(), controls(0.9, 0.2), controls(0.1, 0.9)] {
        let tail = tail_of(c);
        let mut tuned = FreeverbDesign.realization();
        tuned.prepare(
            &FreeverbDesign.bounds(&Ceiling::at(SR).unwrap()),
            &Setup {
                rate: SR,
                max_block: MaxBlock::of(Samples(BLOCK)),
            },
        );
        tuned.retarget(&FreeverbDesign.design(&c, SR));
        let frames = tail + 20_000;
        let mut x = vec![0.0f32; frames];
        x[0] = 1.0;
        let (mut l, mut r) = (vec![0.0f32; frames], vec![0.0f32; frames]);
        tuned.process(&[&x], &mut [&mut l, &mut r]);
        let peak = l.iter().chain(&r).fold(0.0f32, |m, s| m.max(s.abs()));
        let after = l[tail + 1..]
            .iter()
            .chain(&r[tail + 1..])
            .fold(0.0f32, |m, s| m.max(s.abs()));
        let floor = peak * 10f32.powf(-60.0 / 20.0);
        assert!(after <= floor, "{c:?}: {after} above {floor} past {tail}");
    }
}

/// The room is cut at its tail, to the frame: after input that ends at frame
/// `n - 1`, the last frame to sound is `n - 1 + tail` (the tail counted from the
/// last frame of input) and every frame after it is exactly zero. The input is
/// loud noise, so the answer is dense at the cut and a frame early or late shows.
#[test]
fn the_room_is_cut_at_its_tail_to_the_frame() {
    let n = 4_000;
    let tail = tail_of(gm());
    let mut x: Vec<f32> = noise(9, n).iter().map(|s| 1_000.0 * s).collect();
    x.resize(n + tail + 4 * BLOCK, 0.0);
    // Block-aligned input ends at a block edge: 4000 = 62 * 64 + 32, so the last
    // input frame is mid-block, as it may be.
    let mut rig = Rig::new(prepared(FreeverbNode::general_midi(), SR, BLOCK));
    let mut out = [Vec::new(), Vec::new()];
    for chunk in x.chunks(BLOCK) {
        for (all, more) in out.iter_mut().zip(rig.block(chunk, &[]).audio) {
            all.extend(more);
        }
    }
    for (c, ch) in out.iter().enumerate() {
        let last = ch.iter().rposition(|&s| s != 0.0).expect("it sounds");
        assert_eq!(last, n - 1 + tail, "channel {c}");
    }
}

/// A shorter decay does not shorten the tail owed: the figure is held at the
/// longer one until the room has been heard out, measured from the last frame of
/// input. Room 0.9 rung by an impulse, then the cell set to 0.1: every later
/// block still reports the long room's tail, and a block of new input takes
/// off the frames since the old input and no more.
#[test]
fn a_shorter_decay_keeps_the_longer_tail_until_heard_out() {
    let mut rig = Rig::new(live(controls(0.9, 0.5)));
    let long = tail_of(controls(0.9, 0.5));
    let short = tail_of(controls(0.1, 0.5));
    assert!(short < long);
    assert_eq!(reported(&rig.block(&impulse(), &[])), long);
    let cell = rig.node.param_set();
    assert!(cell.set(UnitParam::RoomSize, 0.1));
    for _ in 0..8 {
        assert_eq!(
            reported(&rig.silent()),
            long,
            "held while it may still ring"
        );
    }
    // New input at frame 0 of a block: the old ring ends `long` after the
    // impulse, so `long - (8 blocks + 1 block's frames)` are left, and the new
    // input's own short tail is less than that.
    let owed = long - 9 * BLOCK;
    assert!(short < owed);
    assert_eq!(reported(&rig.block(&impulse(), &[])), owed);
}

/// Past its tail the node cuts the room off, resets and parks (`Status::Idle`),
/// and an idle node answers new input as a fresh one does, bit for bit: the
/// state it holds is exactly zero. Before the tail has elapsed it is not idle.
#[test]
fn an_idle_reverb_wakes_as_a_fresh_one() {
    let mut rig = Rig::new(live(gm()));
    // Loud, so what the cut leaves behind would be well above the flush floor.
    let burst: Vec<f32> = noise(1, BLOCK).iter().map(|s| 1_000.0 * s).collect();
    rig.block(&burst, &[]);
    let tail = tail_of(gm());
    let mut idle_at = None;
    for b in 0..(tail / BLOCK + 4) {
        let d = rig.silent();
        if d.status == Some(Status::Idle) && idle_at.is_none() {
            idle_at = Some(b);
        }
        assert!(
            idle_at.is_some() || d.status == Some(Status::Modified),
            "block {b}: {:?}",
            d.status
        );
        if idle_at.is_some() {
            assert_eq!(d.status, Some(Status::Idle), "stays idle");
        }
    }
    let idle_at = idle_at.expect("it parks after its tail");
    assert!(
        (idle_at + 1) * BLOCK >= tail - BLOCK,
        "not before the tail: block {idle_at} of a {tail}-frame tail"
    );
    // Long enough for the first echoes to come round, where any old state shows.
    let probe = noise(2, 4_096);
    let mut fresh = Rig::new(live(gm()));
    for chunk in probe.chunks(BLOCK) {
        assert_eq!(rig.block(chunk, &[]).audio, fresh.block(chunk, &[]).audio);
    }
}

/// A port the graph modulates is read at the block's last frame, as the cell is
/// read: a block whose room port ends at 0.9 renders as one whose cell is 0.9.
/// And the tail is reported from the *longest* room the port takes in the
/// block, so a decay that falls through the block is not reported short.
#[test]
fn a_modulated_port_is_read_at_the_blocks_end_and_its_peak_sets_the_tail() {
    // Long enough for the combs to come round, where the room is heard.
    let x = noise(3, 4_096);
    let rising: Vec<f32> = (0..BLOCK)
        .map(|i| 0.1 + 0.8 * i as f32 / (BLOCK - 1) as f32)
        .collect();
    let mut by_port = Rig::new(live(controls(0.5, 0.5)));
    let mut by_cell = Rig::new(live(controls(0.9, 0.5)));
    let none: Option<&[f32]> = None;
    for chunk in x.chunks(BLOCK) {
        assert_eq!(
            by_port.block(chunk, &[Some(&rising), none]).audio,
            by_cell.block(chunk, &[]).audio
        );
    }

    // Falling through the block: 0.9 at the start, 0.1 at the end.
    let falling: Vec<f32> = rising.iter().rev().copied().collect();
    let mut rig = Rig::new(live(controls(0.5, 0.5)));
    let d = rig.block(&impulse(), &[Some(&falling), none]);
    assert!(
        reported(&d) >= tail_of(controls(0.9, 0.5)),
        "{} against the long room's {}",
        reported(&d),
        tail_of(controls(0.9, 0.5))
    );
}

/// A non-finite write to a cell reads as unchanged: the node keeps the last
/// finite room, so the render equals one that never saw the NaN.
#[test]
fn a_non_finite_cell_write_is_ignored() {
    let x = noise(4, 4_096);
    let mut hostile = Rig::new(live(controls(0.9, 0.5)));
    let mut calm = Rig::new(live(controls(0.9, 0.5)));
    let set = hostile.node.param_set();
    for (i, chunk) in x.chunks(BLOCK).enumerate() {
        if i == 1 {
            set.set(UnitParam::RoomSize, f32::NAN);
            set.set(UnitParam::Damping, f32::INFINITY);
        }
        assert_eq!(
            hostile.block(chunk, &[]).audio,
            calm.block(chunk, &[]).audio
        );
    }
}

/// The node is the room alone: an impulse leaves the first frame silent on both
/// channels (no dry input in the output), and the first left echo is the input at
/// the fixed gain, 1215 frames in at 48 kHz. Its dry/wet is the graph's mix
/// (`node_mix.rs`), which blends this signal with the aligned input.
#[test]
fn the_node_is_the_room_alone() {
    let mut x = vec![0.0f32; 1_400];
    x[0] = 1.0;
    let mut rig = Rig::new(live(gm()));
    let mut out = [Vec::new(), Vec::new()];
    for chunk in x.chunks(BLOCK) {
        for (all, more) in out.iter_mut().zip(rig.block(chunk, &[]).audio) {
            all.extend(more);
        }
    }
    assert!(out.iter().all(|c| c[0] == 0.0), "nothing at once");
    assert!((out[0][1_215] - Freeverb::FIXED_GAIN).abs() < 1e-6);
}

/// A fork of the node starts from silence with the values set, and shares nothing
/// with it: the harness's fork checks, and a fork taken while the room rings
/// does not carry the ring (doc 022's export-fork rule: `fresh()` plus preroll).
#[test]
fn a_fork_starts_silent_with_the_values_set() {
    assert_param_fork(DesignNode::live(FreeverbDesign, gm()));

    let mut rig = Rig::new(live(controls(0.9, 0.5)));
    rig.block(&noise(5, BLOCK), &[]);
    rig.node.param_set().set(UnitParam::RoomSize, 0.3);
    let template = ParamFork::new(&rig.node);
    let mut fork = Rig::new(prepared(template.fork_node(), SR, BLOCK));
    // Silent input: the fork has nothing to ring, though the live room does.
    let forked = fork.silent().audio;
    assert!(
        forked.iter().flatten().all(|&s| s == 0.0),
        "a fork is silent"
    );
    // It runs the room set (0.3), not the room it was built with (0.9).
    let mut fresh = Rig::new(live(controls(0.3, 0.5)));
    let x = noise(6, 4 * BLOCK);
    for chunk in x.chunks(BLOCK) {
        assert_eq!(fork.block(chunk, &[]).audio, fresh.block(chunk, &[]).audio);
    }
}

/// A real control lane in a real graph reaches the reverb: with the cell at the
/// General MIDI room, a lane holding the room port at 0.9 makes the answer to a
/// noise burst ring far longer than one holding it at 0.1. The decay is measured
/// as the energy between two windows well after the burst: a bigger room keeps
/// more of it.
///
/// This is the modulation path as an engine runs it: the lane sends a segment to
/// the node's param port, the executor fuses it with the node's base, the node
/// reads the port at the block's last frame, designs, and retargets its realization.
#[test]
fn a_lane_driving_the_room_changes_how_long_it_rings() {
    let ring = |room: f32| -> f64 {
        let node = DesignNode::live(FreeverbDesign, gm());
        let mut g = GraphBuilder::new(ChannelLayout::MONO, ChannelLayout::STEREO);
        let (n, _) = g.add_with_controls(node);
        g.connect_input(0, n, 0).connect_output(n, 0, 0);
        g.connect_output(n, 1, 1);
        let mut r = g
            .renderer(Prepare::new(SR, Samples(100)))
            .expect("the graph builds");
        let at = ParamIn {
            node: n,
            param: UnitAddr::first(UnitParam::RoomSize),
        };
        let program = ControlProgram::new(None, vec![], vec![]).expect("a program");
        let lane = ControlLaneNode::for_unit(
            UnitParam::RoomSize,
            program,
            Some(Arc::new(AtomicF32::new(room)).into()),
            None,
        );
        let key = tutti_graph::NodeKey(9_000);
        let editor = r.editor_mut();
        editor.insert(key, "lane", lane);
        editor
            .spec_mut()
            .connect_param(at, tutti_graph::EventOut { node: key, port: 0 });
        editor.commit().expect("the lane commits");
        let frames = 60_000;
        let mut x = noise(8, 4_800);
        x.resize(frames, 0.0);
        let out = r.render_input(&[&x]);
        let energy = |from: usize, to: usize| -> f64 {
            out.iter()
                .flat_map(|c| &c[from..to])
                .map(|&s| f64::from(s).powi(2))
                .sum()
        };
        energy(40_000, 50_000) / energy(10_000, 20_000)
    };
    let (small, big) = (ring(0.1), ring(0.9));
    assert!(
        big > 20.0 * small,
        "a room of 0.9 keeps {big} of its energy, a room of 0.1 keeps {small}"
    );
}

/// A `DesignNode` is an `IntoNode` whose controls are its
/// `ParamSet`, so the session can address the room by `UnitParam`.
#[test]
fn it_is_inserted_with_a_param_set() {
    let (_, set) = DesignNode::live(FreeverbDesign, gm()).into_node();
    assert!(set.set(UnitParam::RoomSize, 0.7));
    assert_eq!(set.get(UnitParam::RoomSize), Some(0.7));
    assert!(
        !set.set(UnitParam::Wet, 0.25),
        "no mix port: the graph blends"
    );
    let (_, fixed) = FreeverbNode::general_midi().into_node();
    assert!(!fixed.set(UnitParam::RoomSize, 0.7), "the send has no room");
}
