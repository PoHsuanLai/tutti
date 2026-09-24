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
use std::sync::Arc;

use tutti_core::{AtomicF32, AudioUnit, BufferVec, ChannelLayout, SampleRate};
use tutti_nodes::{
    CompressorNode, DelayLineNode, GateNode, LadderFilterNode, LadderType, ModDelayNode,
    PhaserNode, SvfFilterNode, SvfType,
};

const BAD: [f32; 3] = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY];

fn noise_block(channels: usize, seed: u32) -> BufferVec {
    let mut buf = BufferVec::new(channels);
    let mut state = seed.wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
    for c in 0..channels {
        for i in 0..64 {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            buf.set_f32(c, i, (state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0);
        }
    }
    buf
}

/// Render `blocks` blocks and return whether every output sample was finite.
fn render_finite(node: &mut dyn AudioUnit, blocks: usize, seed: u32) -> bool {
    let mut out = BufferVec::new(node.outputs());
    let mut all = true;
    for b in 0..blocks {
        let input = noise_block(node.inputs(), seed + b as u32);
        node.process(64, &input.buffer_ref(), &mut out.buffer_mut());
        for c in 0..node.outputs() {
            all &= (0..64).all(|i| out.at_f32(c, i).is_finite());
        }
    }
    all
}

/// A control cell, and a finite value it can be moved to.
type Cell = (&'static str, Arc<AtomicF32>, f32);

/// For every cell and every non-finite value: write it while every other cell
/// moves to its alternate value, render, change the sample rate and render, then restore a finite value and
/// render again. Both renders must be finite. A fresh node per case, so one
/// case's damage cannot hide another's.
fn survives<N: AudioUnit>(name: &str, make: impl Fn() -> N, cells: impl Fn(&N) -> Vec<Cell>) {
    for bad in BAD {
        let count = cells(&make()).len();
        for k in 0..count {
            let mut node = make();
            node.set_sample_rate(SampleRate(48_000.0));
            assert!(render_finite(&mut node, 4, 1), "{name}: finite before");
            let cs = cells(&node);
            let (cell, atomic, alt) = &cs[k];
            let before = atomic.load(Ordering::Acquire);
            atomic.store(bad, Ordering::Release);
            // Every *other* control moves in the same block, so whichever of
            // them triggers a re-solve (a filter's cutoff, an envelope's
            // other time constant) does.
            for (j, (_, other, other_alt)) in cs.iter().enumerate() {
                if j != k {
                    other.store(*other_alt, Ordering::Release);
                }
            }
            assert!(
                render_finite(&mut node, 8, 10),
                "{name}: {cell} = {bad} (with every other control moving) reached the output"
            );
            // A rate change re-derives every time constant from the cells in
            // one go — the path that skips the per-control change guards.
            node.set_sample_rate(SampleRate(44_100.0));
            assert!(
                render_finite(&mut node, 8, 30),
                "{name}: {cell} = {bad} reached the output through a rate change"
            );
            atomic.store(
                if before.is_finite() { *alt } else { before },
                Ordering::Release,
            );
            assert!(
                render_finite(&mut node, 8, 20),
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
    survives(
        "svf",
        || SvfFilterNode::<f64>::with_channels(ChannelLayout::STEREO, SvfType::Bell, 900.0, 1.0),
        |n| {
            vec![
                ("cutoff", n.frequency(), 2_000.0),
                ("q", n.q(), 3.0),
                ("gain", n.gain_db(), 6.0),
            ]
        },
    );
    survives(
        "ladder",
        || {
            LadderFilterNode::<f64>::with_channels(
                ChannelLayout::STEREO,
                LadderType::LP24,
                900.0,
                0.5,
            )
        },
        |n| {
            vec![
                ("cutoff", n.frequency(), 3_000.0),
                ("resonance", n.resonance(), 0.8),
                ("drive", n.drive(), 3.0),
            ]
        },
    );
    survives(
        "delay",
        || {
            let n = DelayLineNode::stereo(0.1, 0.01, 0.013, 0.5);
            n.set_cross_feedback(0.2);
            n.set_mix(0.5);
            n
        },
        |n| {
            vec![
                ("delay time", n.delay_time(), 0.02),
                ("feedback", n.feedback(), 0.7),
                ("cross feedback", n.cross_feedback(), 0.1),
                ("mix", n.mix(), 0.3),
            ]
        },
    );
    survives(
        "chorus",
        || ModDelayNode::chorus(ChannelLayout::STEREO),
        |n| {
            vec![
                ("rate", n.rate(), 3.0),
                ("depth", n.depth(), 0.008),
                ("feedback", n.feedback(), 0.6),
                ("mix", n.mix(), 0.2),
            ]
        },
    );
    survives(
        "phaser",
        || PhaserNode::with_channels(ChannelLayout::STEREO, 6),
        |n| {
            vec![
                ("rate", n.rate(), 2.0),
                ("depth", n.depth(), 0.9),
                ("feedback", n.feedback(), 0.8),
                ("mix", n.mix(), 0.2),
            ]
        },
    );
    survives(
        "compressor",
        || CompressorNode::stereo(-20.0, 4.0, 0.001, 0.05).with_soft_knee(6.0),
        |n| {
            vec![
                ("threshold", n.threshold(), -30.0),
                ("ratio", n.ratio(), 8.0),
                ("attack", n.attack_time(), 0.01),
                ("release", n.release_time(), 0.2),
                ("makeup", n.makeup_gain(), 3.0),
                ("knee", n.knee_width(), 2.0),
            ]
        },
    );
    survives(
        "gate",
        || GateNode::stereo(-30.0, 0.001, 0.01, 0.05),
        |n| {
            vec![
                ("threshold", n.threshold(), -20.0),
                ("attack", n.attack_time(), 0.005),
                ("hold", n.hold_time(), 0.02),
                ("release", n.release_time(), 0.1),
                ("range", n.range(), -20.0),
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
    use tutti_nodes::{generate_test_ir, ConvolverNode};
    let ir = generate_test_ir(128, 0.1, 48_000.0);
    survives(
        "convolver",
        || ConvolverNode::shared_ir(ChannelLayout::STEREO, &ir, 64),
        |n| vec![("mix", n.mix(), 0.8), ("gain", n.gain(), 2.0)],
    );
}
