//! A non-finite control value never reaches a recursive node's state.
//!
//! Every control cell here is a raw `Arc<AtomicF32>` a host can write
//! anything into. A NaN or ±∞ that reached a filter integrator, a delay line,
//! an LFO phase or an envelope follower would stay there for good — every later
//! sample is computed from it — so the nodes read a non-finite value as
//! "unchanged" and keep the last good one.
//!
//! The hard case is the one tested: the bad value lands in the **same block as
//! another control moving**, which is what sends it into the coefficient solve
//! (a held filter never re-solves, so a NaN alone would be ignored by
//! accident). After it, and after a finite value is written back, the output
//! must be finite.

use std::sync::atomic::Ordering;

use tutti_graph::contract::drive;
use tutti_graph::{Node, ParamNode, Prepare};
use tutti_nodes::{
    CompressorNode, DelayLineNode, GateNode, LadderFilterNode, LadderType, ModDelayNode,
    PhaserNode, SvfFilterNode, SvfType,
};
use tutti_types::{
    ChannelLayout, CompressionRatio, Db, Drive, Feedback, Hz, Mix, ParamCell, Q, Resonance,
    SampleRate, Samples, Seconds, UnitParam,
};

const BAD: [f32; 3] = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY];

/// 64 frames of noise per channel, planar.
fn noise_block(channels: usize, seed: u32) -> Vec<Vec<f32>> {
    let mut buf = vec![vec![0.0f32; 64]; channels];
    let mut state = seed.wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
    for ch in &mut buf {
        for s in ch.iter_mut() {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *s = (state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0;
        }
    }
    buf
}

/// A control cell, and a finite value it can be moved to.
type Cell = (&'static str, ParamCell, f32);

/// [`render_finite`] for a graph node: `blocks` 64-frame blocks through
/// `tutti_graph::contract::drive` at `rate`.
fn render_node_finite(node: &mut dyn Node, rate: SampleRate, blocks: usize, seed: u32) -> bool {
    let width = usize::from(node.shape().audio_in.count());
    let mut all = true;
    for b in 0..blocks {
        let chans = noise_block(width, seed + b as u32);
        let refs: Vec<&[f32]> = chans.iter().map(|c| &c[..]).collect();
        let out = drive(node, rate, &refs, &[]);
        all &= out.iter().flatten().all(|s| s.is_finite());
    }
    all
}

/// [`survives`] for a graph node: the same cases, the rate change a
/// re-`prepare`.
fn survives_node<N: Node>(name: &str, make: impl Fn() -> N, cells: impl Fn(&N) -> Vec<Cell>) {
    let (a, b) = (SampleRate(48_000.0), SampleRate(44_100.0));
    for bad in BAD {
        let count = cells(&make()).len();
        for k in 0..count {
            let mut node = make();
            node.prepare(&Prepare::new(a, Samples(64)));
            assert!(
                render_node_finite(&mut node, a, 4, 1),
                "{name}: finite before"
            );
            let cs = cells(&node);
            let (cell, atomic, alt) = &cs[k];
            let before = atomic.load(Ordering::Acquire);
            atomic.store(bad, Ordering::Release);
            for (j, (_, other, other_alt)) in cs.iter().enumerate() {
                if j != k {
                    other.store(*other_alt, Ordering::Release);
                }
            }
            assert!(
                render_node_finite(&mut node, a, 8, 10),
                "{name}: {cell} = {bad} (with every other control moving) reached the output"
            );
            node.prepare(&Prepare::new(b, Samples(64)));
            assert!(
                render_node_finite(&mut node, b, 8, 30),
                "{name}: {cell} = {bad} reached the output through a rate change"
            );
            atomic.store(
                if before.is_finite() { *alt } else { before },
                Ordering::Release,
            );
            assert!(
                render_node_finite(&mut node, b, 8, 20),
                "{name}: {cell} = {bad} left the state non-finite after a finite write"
            );
        }
    }
}

/// Mutation (each run, each fails): dropping the `LastGood` read on the SVF's
/// cutoff (`read_controls`), on the ladder's resonance, on the delay's delay
/// time, on the LFO rate (`LfoDrive::fill_block`), on the mod-mix depth
/// (`TimeModMix::load`), on the compressor's attack (`times`, caught only
/// through the rate change: the follower's own change guard holds a NaN off
/// otherwise), on the gate's attack and range, and on the shared mod-mix
/// feedback. The gate's hold is sanitised too but cannot go non-finite in the
/// output (it becomes an integer frame count, where NaN saturates to 0), so
/// dropping that read is not caught here — it would only turn an infinite hold
/// into a gate held open for good.
#[test]
fn svf_ladder_delay_moddelay_phaser_compressor_gate_hold_off_non_finite_controls() {
    survives_node(
        "svf",
        || {
            SvfFilterNode::<f64>::with_channels(
                ChannelLayout::STEREO,
                SvfType::Bell,
                Hz::new(900.0),
                Q::new(1.0),
            )
        },
        |n| {
            vec![
                (
                    "cutoff",
                    n.param_set().cell(UnitParam::Cutoff).unwrap(),
                    2_000.0,
                ),
                ("q", n.param_set().cell(UnitParam::Q).unwrap(), 3.0),
                ("gain", n.param_set().cell(UnitParam::GainDb).unwrap(), 6.0),
            ]
        },
    );
    survives_node(
        "ladder",
        || {
            LadderFilterNode::<f64>::with_channels(
                ChannelLayout::STEREO,
                LadderType::LP24,
                Hz::new(900.0),
                Resonance::new(0.5),
            )
        },
        |n| {
            vec![
                (
                    "cutoff",
                    n.param_set().cell(UnitParam::Cutoff).unwrap(),
                    3_000.0,
                ),
                ("resonance", n.param_set().cell(UnitParam::Q).unwrap(), 0.8),
                ("drive", n.param_set().cell(UnitParam::Drive).unwrap(), 3.0),
            ]
        },
    );
    survives_node(
        "delay",
        || {
            let n = DelayLineNode::stereo(
                Seconds::new(0.1),
                Seconds::new(0.01),
                Seconds::new(0.013),
                Feedback::new(0.5),
            );
            n.set_cross_feedback(Feedback::new(0.2));
            n.set_mix(Mix::new(0.5));
            n
        },
        |n| {
            vec![
                ("delay time", n.delay_time().into(), 0.02),
                ("feedback", n.feedback().into(), 0.7),
                ("cross feedback", n.cross_feedback().into(), 0.1),
                ("mix", n.mix().into(), 0.3),
            ]
        },
    );
    survives_node(
        "chorus",
        || ModDelayNode::chorus(ChannelLayout::STEREO),
        |n| {
            vec![
                ("rate", n.rate().into(), 3.0),
                ("depth", n.depth().into(), 0.008),
                ("feedback", n.feedback().into(), 0.6),
                ("mix", n.mix().into(), 0.2),
            ]
        },
    );
    survives_node(
        "phaser",
        || PhaserNode::with_channels(ChannelLayout::STEREO, 6),
        |n| {
            vec![
                ("rate", n.rate().into(), 2.0),
                ("depth", n.depth().into(), 0.9),
                ("feedback", n.feedback().into(), 0.8),
                ("mix", n.mix().into(), 0.2),
            ]
        },
    );
    survives_node(
        "compressor",
        || {
            CompressorNode::stereo(
                Db::new(-20.0),
                CompressionRatio::new(4.0),
                Seconds::new(0.001),
                Seconds::new(0.05),
            )
            .with_soft_knee(Db::new(6.0))
        },
        |n| {
            vec![
                ("threshold", n.threshold().into(), -30.0),
                ("ratio", n.ratio().into(), 8.0),
                ("attack", n.attack_time().into(), 0.01),
                ("release", n.release_time().into(), 0.2),
                ("makeup", n.makeup_gain().into(), 3.0),
                ("knee", n.knee_width().into(), 2.0),
            ]
        },
    );
    survives_node(
        "gate",
        || {
            GateNode::stereo(
                Db::new(-30.0),
                Seconds::new(0.001),
                Seconds::new(0.01),
                Seconds::new(0.05),
            )
        },
        |n| {
            vec![
                ("threshold", n.threshold().into(), -20.0),
                ("attack", n.attack_time().into(), 0.005),
                ("hold", n.hold_time().into(), 0.02),
                ("release", n.release_time().into(), 0.1),
                ("range", n.range().into(), -20.0),
            ]
        },
    );
}

/// The convolver's mix and gain are its only cells; neither feeds recursive
/// state, but the block's end starts the next block's ramp.
///
/// Mutation: dropping the `LastGood` read on the gain makes the `inf` case
/// fail (∞ × a zero wet sample is NaN).
#[cfg(feature = "convolution")]
#[test]
fn the_convolver_holds_off_non_finite_controls() {
    use tutti_nodes::{ConvolverNode, generate_test_ir};
    let ir = generate_test_ir(128, Seconds::new(0.1), SampleRate::new(48_000.0));
    survives_node(
        "convolver",
        || ConvolverNode::shared_ir(ChannelLayout::STEREO, &ir, 64),
        |n| vec![("mix", n.mix().into(), 0.8), ("gain", n.gain().into(), 2.0)],
    );
}

/// The waveshaper's drive cell holds a non-finite write off: an infinite drive
/// built an infinitely hard shaper and NaN output (0 * inf) before.
///
/// Mutation (run): in `maybe_update`, `self.drive.load().get()` without the
/// `LastGood` read makes the `inf` case fail for every curve. (The NaN case
/// passes either way: a NaN never exceeds the change threshold.)
#[test]
fn the_distortion_holds_off_non_finite_controls() {
    use tutti_nodes::{DistortionNode, ShapeKind};
    for kind in [
        ShapeKind::Tanh,
        ShapeKind::Atan,
        ShapeKind::Softsign,
        ShapeKind::HardClip,
        ShapeKind::Crush,
        ShapeKind::SoftCrush,
    ] {
        survives_node(
            "distortion",
            || DistortionNode::new(kind, Drive::new(2.0)),
            |n| vec![("drive", n.drive().into(), 5.0)],
        );
    }
}

/// A waveshaper built with a drive that is NaN or infinite renders finite.
///
/// No mutation is claimed: the first block's `LastGood` read rebuilds the
/// shaper from 0 whatever the constructor built, so this pins the behaviour.
#[test]
fn a_distortion_built_with_a_non_finite_drive_renders_finite() {
    use tutti_nodes::{DistortionNode, ShapeKind};
    for bad in BAD {
        let mut node = DistortionNode::new(ShapeKind::Tanh, Drive::new(bad));
        node.prepare(&Prepare::new(SampleRate(48_000.0), Samples(64)));
        assert!(
            render_node_finite(&mut node, SampleRate(48_000.0), 4, 1),
            "drive {bad}"
        );
    }
}
