//! Waveshaping distortion node over six memoryless curves ([`ShapeKind`]).
//!
//! Each curve ([`ShapeKind::apply`]) is one line of arithmetic: `Tanh`,
//! `Atan`, `Softsign`, `Clip`, `Crush` and `SoftCrush`.
//!
//! The node owns an atomic `drive` (a [`Param`]) and rebuilds the cheap,
//! stateless shaper only when drive actually moves, so `UnitParam::Drive` is
//! written through the node's [`ParamSet`] live, with no node rebuild and no
//! zipper noise.
//!
//! The waveshape *kind* (Tanh / Atan / … ) is fixed at construction: switching
//! kind is a different effect kind, which a host handles as remove + add (a
//! respawn), exactly like switching filter type.
//!
//! 2 inputs / 2 outputs by default ([`DistortionNode::with_channels`] for
//! any width). The shaper is memoryless, so the two channels are
//! fully independent and stereo is just the same curve applied per channel.
//!
//! # Modulated drive
//!
//! Drive is modulatable by the graph: when the graph
//! feeds the node's param port ([`Io::param`](tutti_graph::Io::param)) a
//! per-frame drive, it **overrides** the `drive` atomic per sample (rebuilding the stateless
//! shaper when it moves). Unfed, the node reads its atomic once per block —
//! bit-identical output to a node nothing can modulate, at the cost of one
//! branch.

use crate::ramp::LastGood;
use std::sync::Arc;
use tutti_dsp::{ShapeKind, Shaper};
use tutti_graph::{Cx, IntoNode, Io, Node, NodeParts, ParamNode, ParamSet, Prepare, Shape, Status};
use tutti_types::AtomicF32;
use tutti_types::UnitParam;
use tutti_types::{ChannelLayout, Drive, Param, Tail};

/// Stereo waveshaping distortion. 2 inputs, 2 outputs.
///
/// `drive` is live-modulatable via the [`Param`] atomic (UI handle) and, in a
/// graph, the [`ParamSet`] it is inserted with (`UnitParam::Drive`); the
/// waveshape `kind` is set at construction.
#[derive(Debug)]
pub struct DistortionNode {
    kind: ShapeKind,
    drive: Param<Drive>,
    shaper: Shaper,
    last_drive: f32,
    /// The last finite drive read from the cell or the port: an infinite one
    /// would build an infinitely hard shaper (a NaN is already inert).
    good: LastGood,
    /// Audio channel width (as many inputs as outputs). The shaper is
    /// stateless and channel-shared, so widening is purely the port count.
    layout: ChannelLayout,
}

/// The params a [`DistortionNode`] lets the graph modulate, in port order.
pub const DISTORTION_PARAMS: [UnitParam; 1] = [UnitParam::Drive];

impl DistortionNode {
    /// Builds a stereo waveshaper of `kind` at `drive`.
    ///
    /// [`Drive`] is input gain into the shaping curve, floored at 0: higher
    /// pushes further into the nonlinearity and distorts harder. The curve
    /// itself is stateless, so drive is the only thing that changes its
    /// character.
    pub fn new(kind: ShapeKind, drive: impl Into<Drive>) -> Self {
        Self::with_channels(ChannelLayout::STEREO, kind, drive)
    }

    /// An `n`-channel waveshaper. The shaper carries no per-channel state, so
    /// every channel is shaped by the same (linked) drive/kind.
    /// `with_channels(ChannelLayout::STEREO, …)` is bit-identical to
    /// [`Self::new`]. An empty layout is taken as mono, as the other nodes do.
    pub fn with_channels(
        channels: impl Into<ChannelLayout>,
        kind: ShapeKind,
        drive: impl Into<Drive>,
    ) -> Self {
        let drive = drive.into();
        let d = drive.get().max(0.0);
        Self {
            kind,
            drive: Param::new(drive),
            shaper: Shaper::build(kind, d),
            last_drive: d,
            // An infinite initial drive reads as 0 here, so the first block rebuilds.
            good: LastGood::new(d, 0.0),
            layout: ChannelLayout::from_count(channels.into().count().max(1)),
        }
    }

    /// Atomic handle for the UI / automation to share the drive cell.
    pub fn drive(&self) -> Arc<AtomicF32> {
        self.drive.as_atomic()
    }

    /// Sets the [`Drive`] into the shaping curve, floored at 0.
    ///
    /// Read once per block; the shaper is rebuilt only when drive moves
    /// meaningfully, which is cheap because it carries no state.
    pub fn set_drive(&self, drive: impl Into<Drive>) {
        self.drive.store(Drive(drive.into().get().max(0.0)));
    }

    /// Rebuild the (stateless) shaper if drive has moved meaningfully. Cheap:
    /// the shapers carry no z-state, so reconstruction is just a field swap.
    #[inline]
    fn maybe_update(&mut self) {
        let d = self.good.read(self.drive.load().get());
        if (d - self.last_drive).abs() > 1e-6 {
            self.shaper = Shaper::build(self.kind, d);
            self.last_drive = d;
        }
    }

    /// Rebuild the shaper from a per-sample effective drive (audio-rate
    /// modulation path). Same change guard as [`Self::maybe_update`] so a held
    /// modulation value doesn't rebuild every sample needlessly.
    #[inline]
    fn maybe_update_modulated(&mut self, drive: f32) {
        if (drive - self.last_drive).abs() > 1e-6 {
            self.shaper = Shaper::build(self.kind, drive);
            self.last_drive = drive;
        }
    }
}

/// Shares the drive cell (the template [`tutti_graph::param_parts`] forks
/// from).
impl Clone for DistortionNode {
    fn clone(&self) -> Self {
        Self {
            kind: self.kind,
            drive: self.drive.handle(),
            shaper: self.shaper,
            last_drive: self.last_drive,
            good: self.good,
            layout: self.layout,
        }
    }
}

impl Node for DistortionNode {
    /// `channels` in and out, drive modulatable ([`DISTORTION_PARAMS`]).
    /// The shapers carry no z-state, so the output stops with the input.
    ///
    /// The cost is the curve's, measured by doc 013's fixed-plus-per-frame
    /// fit against its two reference nodes (its cost table has the row):
    /// `Softsign` and `HardClip` are arithmetic, `Light`; `Crush` and
    /// `SoftCrush` (a rounding call) and `Atan` and `Tanh` (a libm call)
    /// are `Moderate`.
    fn shape(&self) -> Shape {
        let width = self.layout;
        let cost = match self.kind {
            ShapeKind::Softsign | ShapeKind::HardClip => tutti_graph::Cost::Light,
            _ => tutti_graph::Cost::Moderate,
        };
        Shape::through_discrete(width)
            .with_cost(cost)
            .with_tail(Tail::None)
            .with_params(&DISTORTION_PARAMS)
    }

    /// Nothing is rate-dependent: the curves are memoryless.
    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let size = io.frames();
        let drive = io.param(0).frames();
        let (inputs, mut outputs) = io.split();
        // Both paths are channel-major, each loop compiled for its curve
        // (`ShapeKind::shape_block` and friends). The curve is memoryless, so a sample's value depends
        // only on its input and its frame's hardness, not on the loop order.
        match drive {
            // Fast path: drive not modulated — block-rate shaper update,
            // bit-identical to a node nothing modulates.
            None => {
                self.maybe_update();
                let h = self.shaper.hardness();
                for (c, out) in outputs.iter_mut().enumerate() {
                    self.kind
                        .shape_block(h, &inputs.get(c)[..size], &mut out[..size]);
                }
            }
            // Modulated path: the per-frame drive rebuilds the shaper when it
            // moves (a sequential guard, so it runs once, in frame order),
            // and each frame's hardness lands in channel 0's output, which
            // is then the hardness every channel reads. Channel 0 is shaped
            // last, over its own hardness.
            Some(drive) => {
                let mut outs = outputs.iter_mut();
                let Some(first) = outs.next() else {
                    return Status::Modified;
                };
                let first = &mut first[..size];
                for (h, &d) in first.iter_mut().zip(drive) {
                    let d = self.good.read(d);
                    self.maybe_update_modulated(d.max(0.0));
                    *h = self.shaper.hardness();
                }
                for (c, out) in outs.enumerate() {
                    self.kind.shape_block_per_frame(
                        &*first,
                        &inputs.get(c + 1)[..size],
                        &mut out[..size],
                    );
                }
                self.kind
                    .shape_block_in_place(&inputs.get(0)[..size], first);
            }
        }
        Status::Modified
    }

    fn reset(&mut self) {}

    fn param_base(&self, k: usize) -> Option<f32> {
        (k == 0).then(|| self.drive.load().get())
    }
}

impl ParamNode for DistortionNode {
    /// The drive.
    fn param_set(&self) -> ParamSet {
        ParamSet::builder()
            .param(UnitParam::Drive, self.drive())
            .build()
    }

    /// A clone with its drive cell detached.
    fn fork_fresh(&self) -> Self {
        let mut fork = self.clone();
        fork.drive.detach();
        fork
    }
}

/// Inserted with its [`ParamSet`] as its controls and a fork from the value
/// last set through it ([`tutti_graph::param_parts`]).
impl IntoNode for DistortionNode {
    type Controls = ParamSet;

    fn into_parts(self) -> NodeParts<ParamSet> {
        tutti_graph::param_parts(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{RATE, tick};
    use tutti_graph::contract::{assert_param_fork, drive};

    fn process_mono_through(node: &mut DistortionNode, input: &[f32]) -> Vec<f32> {
        let mut out = vec![0.0f32; input.len()];
        for (i, &x) in input.iter().enumerate() {
            let mut o = [0.0f32; 2];
            tick(node, &[x, x], &mut o);
            out[i] = o[0];
        }
        out
    }

    /// A fork starts from the drive last set through the node's `ParamSet`
    /// and shares no cell with it.
    ///
    /// Mutation (run): drop `fork.drive.detach()` in `fork_fresh` → "a live
    /// write reached the fork" for `Drive` → fails.
    #[test]
    fn a_fork_shares_no_cell() {
        assert_param_fork(DistortionNode::new(ShapeKind::Tanh, Drive::new(2.0)));
    }

    #[test]
    fn distortion_is_two_in_two_out() {
        let n = DistortionNode::new(ShapeKind::Tanh, Drive::new(1.0));
        assert_eq!(n.shape().audio_in().count(), 2);
        assert_eq!(n.shape().audio_out().count(), 2);
    }

    #[test]
    fn with_channels_reports_arity_and_shapes_every_channel() {
        let mut n = DistortionNode::with_channels(6u16, ShapeKind::HardClip, Drive::new(1.0));
        assert_eq!(n.shape().audio_in().count(), 6);
        assert_eq!(n.shape().audio_out().count(), 6);

        // Each channel gets the same (linked) shaper — hardclip clamps all 6.
        let mut out = [0.0f32; 6];
        tick(&mut n, &[4.0, -4.0, 2.0, -2.0, 0.5, -0.5], &mut out);
        for (c, &y) in out.iter().enumerate() {
            assert!(
                (-1.0 - 1e-6..=1.0 + 1e-6).contains(&y),
                "ch{c} not hardclipped: {y}"
            );
        }
        // The two saturating channels actually hit the rails (not silent).
        assert!((out[0] - 1.0).abs() < 1e-6);
        assert!((out[1] + 1.0).abs() < 1e-6);
    }

    #[test]
    fn with_channels_2_is_bit_identical_to_new() {
        let mut a = DistortionNode::new(ShapeKind::Tanh, Drive::new(1.7));
        let mut b = DistortionNode::with_channels(2u16, ShapeKind::Tanh, Drive::new(1.7));
        for i in 0..256 {
            let x = (i as f32 / 128.0) - 1.0;
            let mut oa = [0.0f32; 2];
            let mut ob = [0.0f32; 2];
            tick(&mut a, &[x, -x], &mut oa);
            tick(&mut b, &[x, -x], &mut ob);
            assert_eq!(oa[0].to_bits(), ob[0].to_bits());
            assert_eq!(oa[1].to_bits(), ob[1].to_bits());
        }
    }

    #[test]
    fn hardclip_clamps_to_unity() {
        let mut n = DistortionNode::new(ShapeKind::HardClip, Drive::new(1.0));
        let input = vec![-4.0, -1.0, 0.0, 1.0, 4.0];
        let out = process_mono_through(&mut n, &input);
        for y in out {
            assert!(
                (-1.0 - 1e-6..=1.0 + 1e-6).contains(&y),
                "hardclip exceeded ±1: {y}"
            );
        }
    }

    #[test]
    fn tanh_is_monotonic_and_bounded() {
        let mut n = DistortionNode::new(ShapeKind::Tanh, Drive::new(2.0));
        let input: Vec<f32> = (0..200).map(|i| (i as f32 / 100.0) - 1.0).collect();
        let out = process_mono_through(&mut n, &input);
        // tanh saturates within (-1, 1) and is strictly increasing for increasing input.
        for w in out.windows(2) {
            assert!(
                w[1] >= w[0] - 1e-6,
                "tanh not monotonic: {} -> {}",
                w[0],
                w[1]
            );
        }
        for &y in &out {
            assert!(y.abs() <= 1.0, "tanh out of bounds: {y}");
        }
    }

    #[test]
    fn higher_drive_saturates_more() {
        // Same input, more drive → the saturating curve pushes mid-level signal
        // closer to the rails, so RMS rises.
        let signal: Vec<f32> = (0..512).map(|i| 0.3 * (i as f32 * 0.05).sin()).collect();
        let rms = |v: &[f32]| (v.iter().map(|x| x * x).sum::<f32>() / v.len() as f32).sqrt();

        let mut low = DistortionNode::new(ShapeKind::Tanh, Drive::new(1.0));
        let mut high = DistortionNode::new(ShapeKind::Tanh, Drive::new(8.0));
        let out_low = process_mono_through(&mut low, &signal);
        let out_high = process_mono_through(&mut high, &signal);
        assert!(
            rms(&out_high) > rms(&out_low),
            "more drive should raise RMS: low={}, high={}",
            rms(&out_low),
            rms(&out_high)
        );
    }

    /// Drive is reachable by address through the node's `ParamSet`, and an
    /// address it does not own is refused.
    ///
    /// Mutation (run): leave `Drive` out of `param_set` → the set refuses it
    /// → fails.
    #[test]
    fn drive_settable_via_unit_param() {
        use std::sync::atomic::Ordering;
        let n = DistortionNode::new(ShapeKind::Tanh, Drive::new(1.0));
        let set = n.param_set();
        assert!(set.set(UnitParam::Drive, 5.0));
        assert!((n.drive().load(Ordering::Acquire) - 5.0).abs() < 1e-3);
        // A param this unit doesn't own is refused.
        assert!(!set.set(UnitParam::Cutoff, 1000.0));
    }

    #[test]
    fn channels_are_independent() {
        let mut n = DistortionNode::new(ShapeKind::HardClip, Drive::new(1.0));
        let mut o = [0.0f32; 2];
        tick(&mut n, &[4.0, 0.5], &mut o);
        assert!(
            (o[0] - 1.0).abs() < 1e-6,
            "L should clip to 1.0, got {}",
            o[0]
        );
        assert!((o[1] - 0.5).abs() < 1e-6, "R should pass 0.5, got {}", o[1]);
    }

    // ── Modulated drive (the graph's param feed) ────────────────────────────

    /// The shape declares drive, and never changes the arity.
    ///
    /// Mutation (run): declare no params in `shape` → the first assertion
    /// fails.
    #[test]
    fn distortion_declares_its_drive_param() {
        let d = DistortionNode::new(ShapeKind::Tanh, Drive::new(1.0));
        assert_eq!(d.shape().params.as_slice(), &[UnitParam::Drive][..]);
        assert_eq!(
            (d.shape().audio_in().count(), d.shape().audio_out().count()),
            (2, 2)
        );
        assert_eq!(d.param_base(0), Some(1.0), "the base is the drive control");
    }

    #[test]
    fn distortion_unmodulated_matches_held_constant() {
        // A node whose fed drive is held at the same value as a plain node's
        // atomic must produce bit-identical output — the modulated path is a
        // faithful superset. Tanh at drive 5.0 saturates hard enough that any
        // divergence would show.
        let signal: Vec<f32> = (0..512).map(|i| 0.6 * (i as f32 * 0.05).sin()).collect();

        let mut plain = DistortionNode::new(ShapeKind::Tanh, Drive::new(5.0));
        let plain_out = process_mono_through(&mut plain, &signal);

        let mut modn = DistortionNode::new(ShapeKind::Tanh, Drive::new(5.0));
        let held = vec![5.0f32; signal.len()];
        let mod_out = drive(&mut modn, RATE, &[&signal, &signal], &[Some(&held)]);
        for i in 0..signal.len() {
            assert!(
                (plain_out[i] - mod_out[0][i]).abs() < 1e-6,
                "modulated-held output diverges from plain at sample {i}: {} vs {}",
                plain_out[i],
                mod_out[0][i]
            );
        }
    }

    /// A fed drive shapes every channel of a wide node, per frame: with the
    /// drive fed a step, each of six channels saturates harder from the
    /// step's frame on, and a block whose param reads its base reads the
    /// control again.
    ///
    /// Mutation (run): shape only channel 0 on the modulated path → the
    /// other channels do not move at the step → fails. Keep the last
    /// modulated shaper on a base block (skip `maybe_update`) → the last
    /// block is still driven hard → fails.
    #[test]
    fn a_fed_drive_shapes_every_channel_of_a_wide_node() {
        let mut n = DistortionNode::with_channels(6u16, ShapeKind::HardClip, Drive::new(1.0));
        assert_eq!(
            (n.shape().audio_in().count(), n.shape().audio_out().count()),
            (6, 6)
        );
        let x = vec![0.25f32; 64];
        let ins: Vec<&[f32]> = (0..6).map(|_| &x[..]).collect();
        let drive_step: Vec<f32> = (0..64).map(|i| if i < 32 { 1.0 } else { 3.0 }).collect();
        let out = drive(&mut n, RATE, &ins, &[Some(&drive_step)]);
        for (c, o) in out.iter().enumerate() {
            assert_eq!(o[31], 0.25, "channel {c} before the step");
            assert_eq!(o[32], 0.75, "channel {c} on the step's frame");
        }
        let out = drive(&mut n, RATE, &ins, &[None]);
        assert!(
            out.iter().all(|o| o.iter().all(|&y| y == 0.25)),
            "the control again"
        );
    }

    const KINDS: [ShapeKind; 6] = [
        ShapeKind::Tanh,
        ShapeKind::Atan,
        ShapeKind::Softsign,
        ShapeKind::HardClip,
        ShapeKind::Crush,
        ShapeKind::SoftCrush,
    ];

    /// Every output sample of a block is the node's curve applied to that
    /// channel's own input at the node's drive: the per-kind loops pick the
    /// curve they name and read their own channel at their own frame.
    ///
    /// Mutation (run): map `Atan`'s arm in `ShapeKind::shape_block` to `Tanh` → the Atan
    /// row fails; read `inputs.get(0)` for every channel → channel 1 fails.
    #[test]
    fn a_block_is_the_curve_of_each_sample() {
        use crate::test_support::noise;
        for kind in KINDS {
            let mut n = DistortionNode::with_channels(3u16, kind, Drive::new(2.5));
            let ins: Vec<Vec<f32>> = (0..3).map(|c| noise(c + 7, 64)).collect();
            let refs: Vec<&[f32]> = ins.iter().map(|c| &c[..]).collect();
            let out = drive(&mut n, RATE, &refs, &[None]);
            for (c, (o, x)) in out.iter().zip(&ins).enumerate() {
                for (i, (&y, &x)) in o.iter().zip(x).enumerate() {
                    let want = kind.apply(x, Drive(2.5));
                    assert_eq!(y.to_bits(), want.to_bits(), "{kind:?} ch{c} frame {i}");
                }
            }
        }
    }

    /// With a drive fed that moves every frame, every channel's sample is
    /// the curve at that frame's drive, channel 0 included (whose output
    /// holds the hardness until it is shaped last).
    ///
    /// Mutation (run): skip channel 0's final pass → channel 0 outputs the
    /// hardness → fails; read frame `i - 1`'s hardness for the other
    /// channels → fails.
    #[test]
    fn a_fed_drive_is_read_per_frame_on_every_channel() {
        use crate::test_support::noise;
        let fed: Vec<f32> = (0..64).map(|i| 1.0 + 0.125 * i as f32).collect();
        for kind in KINDS {
            let mut n = DistortionNode::with_channels(3u16, kind, Drive::new(1.0));
            let ins: Vec<Vec<f32>> = (0..3).map(|c| noise(c + 11, 64)).collect();
            let refs: Vec<&[f32]> = ins.iter().map(|c| &c[..]).collect();
            let out = drive(&mut n, RATE, &refs, &[Some(&fed)]);
            for (c, (o, x)) in out.iter().zip(&ins).enumerate() {
                for (i, (&y, &x)) in o.iter().zip(x).enumerate() {
                    let want = kind.apply(x, Drive(fed[i]));
                    assert_eq!(y.to_bits(), want.to_bits(), "{kind:?} ch{c} frame {i}");
                }
            }
        }
    }

    /// The two arithmetic-only curves declare themselves `Light`, the four
    /// that call a math routine (`tanh`, `atan`, `round`, `floor`)
    /// `Moderate`.
    ///
    /// Mutation (run): declare `Moderate` for every kind → the `HardClip`
    /// row fails; add `Crush` to the light arm → its row fails.
    #[test]
    fn the_declared_cost_follows_the_curve() {
        use tutti_graph::Cost;
        for kind in KINDS {
            let want = match kind {
                ShapeKind::Softsign | ShapeKind::HardClip => Cost::Light,
                _ => Cost::Moderate,
            };
            assert_eq!(
                DistortionNode::new(kind, Drive::new(1.0)).shape().cost,
                want,
                "{kind:?}"
            );
        }
    }
}
