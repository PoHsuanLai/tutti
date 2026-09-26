//! Waveshaping distortion node over six memoryless curves ([`ShapeKind`]).
//!
//! The curves are this crate's own ([`ShapeKind::apply`]). They were fundsp's
//! `Shape` impls (`Tanh`, `Atan`, `Softsign`, `Clip`, `Crush`, `SoftCrush`),
//! reached through `tutti_core::dsp`; each is one line of arithmetic, so owning
//! them cost less than the re-export surface did. The formulas are fundsp's,
//! in the same operation order, and matched fundsp's output bit for bit when
//! they were moved (design doc 013, Phase 0b).
//!
//! fundsp's `shape(..)` opcode bakes its drive (the shaper's hardness field) in
//! at construction and exposes no `set()`, so driving it live would force a
//! crossfade node-rebuild every parameter change. Instead this node owns an
//! atomic `drive` (the standard [`Param`] UI-handle pattern) and reconstructs
//! the cheap, stateless shaper struct only when drive actually moves — so
//! `UnitParam::Drive` flows through the ordinary lock-free `Net::set` path and
//! the generic `reconcile_unit_params` reconciler, with no rebuild and no
//! zipper noise.
//!
//! The waveshape *kind* (Tanh / Atan / … ) is fixed at construction: switching
//! kind is a different effect kind, which a host handles as remove + add (a
//! respawn), exactly like switching filter type.
//!
//! 2 inputs / 2 outputs. The shaper is memoryless, so the two channels are
//! fully independent and stereo is just the same curve applied per channel.
//!
//! # Modulated drive
//!
//! Drive is modulatable by the graph (design doc 013 item 6): when the graph
//! feeds the node's [`ParamFeed`](tutti_core::ParamFeed) a per-frame drive,
//! it **overrides** the `drive` atomic per sample (rebuilding the stateless
//! shaper when it moves). Unfed, the node reads its atomic once per block —
//! bit-identical output to a node nothing can modulate, at the cost of one
//! branch.

use tutti_core::Arc;
use tutti_core::AtomicF32;
use tutti_core::{AudioUnit, BufferMut, BufferRef, SignalFrame};
use tutti_core::{Drive, Param, ParamFeed, Tail};
use tutti_types::UnitParam;

/// Selects which waveshaping curve a [`DistortionNode`] applies.
///
/// Discriminants are stable — they persist in saved projects via
/// `EffectKind::Distortion`. Append new kinds at the end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ShapeKind {
    /// `tanh` saturation — smooth, tube-like.
    #[default]
    Tanh,
    /// `atan` saturation — slightly harder knee than tanh.
    Atan,
    /// `softsign` (`x / (1 + |x|)`) — gentle.
    Softsign,
    /// Hard clip to ±1.
    HardClip,
    /// Bitcrush-style staircase quantization.
    Crush,
    /// Smoothed staircase quantization.
    SoftCrush,
}

impl ShapeKind {
    /// Shape one sample: `x` through this curve at `drive`.
    ///
    /// `drive` is input gain into the curve, floored at 0 — the same
    /// normalization [`DistortionNode`] applies to its param. For the two
    /// staircases it is instead the number of **levels per unit**, floored at 1
    /// (fewer than one level per unit is not a staircase).
    ///
    /// Memoryless and total: every curve maps every finite `x` to a finite
    /// output, and none keeps state between calls.
    #[inline]
    pub fn apply(self, x: f32, drive: Drive) -> f32 {
        Shaper::build(self, drive.get().max(0.0)).shape(x)
    }

    /// The curve at a hardness already normalized by [`Shaper::build`].
    ///
    /// Each arm is the formula fundsp's corresponding `Shape` impl used, in the
    /// same operation order — which is what kept the move bit-identical.
    #[inline]
    fn curve(self, hardness: f32, x: f32) -> f32 {
        use core::f32::consts::PI;
        match self {
            Self::Tanh => (x * hardness).tanh(),
            // Rescaled to saturate at ±1 while keeping unit slope at the origin.
            Self::Atan => (x * (hardness * PI * 0.5)).atan() * (2.0 / PI),
            Self::Softsign => {
                let y = x * hardness;
                y / (1.0 + y.abs())
            }
            Self::HardClip => (x * hardness).clamp(-1.0, 1.0),
            Self::Crush => (x * hardness).round() / hardness,
            Self::SoftCrush => {
                let y = x * hardness;
                let step = y.floor();
                (step + smooth9(y - step)) / hardness
            }
        }
    }
}

/// The ninth-order smoothstep: `0 → 0`, `1 → 1`, with the first four
/// derivatives zero at both ends, so each [`ShapeKind::SoftCrush`] step is a
/// smooth S rather than a jump.
#[inline]
fn smooth9(x: f32) -> f32 {
    let x2 = x * x;
    ((((70.0 * x - 315.0) * x + 540.0) * x - 420.0) * x + 126.0) * x2 * x2 * x
}

/// A curve at a fixed hardness: the kind plus its drive, already floored the
/// way the kind needs. Rebuilt only when drive moves, which is a field swap —
/// the curves carry no state.
#[derive(Clone, Copy)]
struct Shaper {
    kind: ShapeKind,
    hardness: f32,
}

impl Shaper {
    fn build(kind: ShapeKind, drive: f32) -> Self {
        let hardness = match kind {
            ShapeKind::Crush | ShapeKind::SoftCrush => drive.max(1.0),
            _ => drive,
        };
        Self { kind, hardness }
    }

    #[inline]
    fn shape(&self, x: f32) -> f32 {
        self.kind.curve(self.hardness, x)
    }
}

/// Stereo waveshaping distortion. 2 inputs, 2 outputs.
///
/// `drive` is live-modulatable via the [`Param`] atomic (UI handle) and the
/// `UnitParam::Drive` setting path; the waveshape `kind` is set at construction.
pub struct DistortionNode {
    kind: ShapeKind,
    drive: Param<Drive>,
    shaper: Shaper,
    last_drive: f32,
    /// Audio channel width (`inputs()` audio ports == `outputs()`). The shaper
    /// is stateless and channel-shared, so widening is purely the port count.
    channels: usize,
    /// A per-frame drive from the graph, when it modulates it: overrides
    /// [`Self::drive`] per sample.
    feed: ParamFeed,
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
        Self::with_channels(2, kind, drive)
    }

    /// An `n`-channel waveshaper. The shaper carries no per-channel state, so
    /// every channel is shaped by the same (linked) drive/kind.
    /// `with_channels(2, …)` is bit-identical to [`Self::new`].
    pub fn with_channels(channels: usize, kind: ShapeKind, drive: impl Into<Drive>) -> Self {
        let drive = drive.into();
        let d = drive.get().max(0.0);
        Self {
            kind,
            drive: Param::new(drive),
            shaper: Shaper::build(kind, d),
            last_drive: d,
            channels: channels.max(1),
            feed: ParamFeed::new(&DISTORTION_PARAMS),
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
        let d = self.drive.load().get();
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

impl Clone for DistortionNode {
    fn clone(&self) -> Self {
        Self {
            kind: self.kind,
            drive: self.drive.handle(),
            shaper: self.shaper,
            last_drive: self.last_drive,
            channels: self.channels,
            feed: self.feed.clone(),
        }
    }
}

impl AudioUnit for DistortionNode {
    fn inputs(&self) -> usize {
        self.channels
    }

    fn outputs(&self) -> usize {
        self.channels
    }

    /// Detach every control cell this node reads (see `Param::detach`), so
    /// a fork renders the controls as they were when it was taken, not the
    /// live knob moves made while it runs. Values are kept.
    fn isolate(&mut self) {
        self.drive.detach();
    }

    fn reset(&mut self) {}

    #[inline]
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        // Effective drive: a fed drive overrides the atomic (the atomic is
        // the base the graph's modulation rides on).
        match self.feed.get(0, 1) {
            None => self.maybe_update(),
            Some(d) => self.maybe_update_modulated(d[0].max(0.0)),
        }
        for c in 0..self.channels {
            output[c] = self.shaper.shape(input[c]);
        }
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        // Fast path: drive not fed — block-rate shaper update, bit-identical
        // to a node nothing modulates.
        if !self.feed.any_live() {
            self.maybe_update();
            for i in 0..size {
                for c in 0..self.channels {
                    output.set_f32(c, i, self.shaper.shape(input.at_f32(c, i)));
                }
            }
            return;
        }
        // Modulated path: read the fed drive per sample and rebuild the shaper
        // when it moves before shaping every channel.
        let feed = ParamFeed::take(&mut self.feed);
        let drive = feed.get(0, size).expect("the one param is live");
        for (i, &d) in drive.iter().enumerate() {
            self.maybe_update_modulated(d.max(0.0));
            for c in 0..self.channels {
                output.set_f32(c, i, self.shaper.shape(input.at_f32(c, i)));
            }
        }
        self.feed = feed;
    }

    fn param_feed(&mut self) -> Option<&mut ParamFeed> {
        Some(&mut self.feed)
    }

    fn param_base(&self, k: usize) -> Option<f32> {
        (k == 0).then(|| self.drive.load().get())
    }

    fn set(&mut self, setting: tutti_core::Setting) {
        if let Some((param, value)) = tutti_core::unit_param::from_setting(&setting) {
            if matches!(param, tutti_core::UnitParam::Drive) {
                self.set_drive(value);
            }
        }
    }

    fn get_id(&self) -> u64 {
        crate::node_id::DISTORTION_ID
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        // Nonlinear: the output is no longer a pure scaling of the input, so
        // mark every channel as unknown-value (latency 0, no constant prop).
        let mut out = SignalFrame::new(self.channels);
        for c in 0..self.channels {
            out.set(c, input.at(c).distort(0.0));
        }
        out
    }

    /// The shapers carry no z-state, so the output stops with the input.
    fn tail(&mut self) -> Tail {
        Tail::None
    }

    fn footprint(&self) -> usize {
        core::mem::size_of::<Self>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process_mono_through(node: &mut DistortionNode, input: &[f32]) -> Vec<f32> {
        let mut out = vec![0.0f32; input.len()];
        for (i, &x) in input.iter().enumerate() {
            let mut o = [0.0f32; 2];
            node.tick(&[x, x], &mut o);
            out[i] = o[0];
        }
        out
    }

    #[test]
    fn distortion_is_two_in_two_out() {
        let n = DistortionNode::new(ShapeKind::Tanh, 1.0);
        assert_eq!(n.inputs(), 2);
        assert_eq!(n.outputs(), 2);
    }

    #[test]
    fn with_channels_reports_arity_and_shapes_every_channel() {
        let mut n = DistortionNode::with_channels(6, ShapeKind::HardClip, 1.0);
        assert_eq!(n.inputs(), 6);
        assert_eq!(n.outputs(), 6);

        // Each channel gets the same (linked) shaper — hardclip clamps all 6.
        let mut out = [0.0f32; 6];
        n.tick(&[4.0, -4.0, 2.0, -2.0, 0.5, -0.5], &mut out);
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
        let mut a = DistortionNode::new(ShapeKind::Tanh, 1.7);
        let mut b = DistortionNode::with_channels(2, ShapeKind::Tanh, 1.7);
        for i in 0..256 {
            let x = (i as f32 / 128.0) - 1.0;
            let mut oa = [0.0f32; 2];
            let mut ob = [0.0f32; 2];
            a.tick(&[x, -x], &mut oa);
            b.tick(&[x, -x], &mut ob);
            assert_eq!(oa[0].to_bits(), ob[0].to_bits());
            assert_eq!(oa[1].to_bits(), ob[1].to_bits());
        }
    }

    #[test]
    fn hardclip_clamps_to_unity() {
        let mut n = DistortionNode::new(ShapeKind::HardClip, 1.0);
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
        let mut n = DistortionNode::new(ShapeKind::Tanh, 2.0);
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

        let mut low = DistortionNode::new(ShapeKind::Tanh, 1.0);
        let mut high = DistortionNode::new(ShapeKind::Tanh, 8.0);
        let out_low = process_mono_through(&mut low, &signal);
        let out_high = process_mono_through(&mut high, &signal);
        assert!(
            rms(&out_high) > rms(&out_low),
            "more drive should raise RMS: low={}, high={}",
            rms(&out_low),
            rms(&out_high)
        );
    }

    #[test]
    fn drive_settable_via_unit_param() {
        use std::sync::atomic::Ordering;
        use tutti_core::unit_param;
        use tutti_core::{AudioUnit, UnitParam};
        let mut n = DistortionNode::new(ShapeKind::Tanh, 1.0);
        n.set(unit_param::setting(UnitParam::Drive, 5.0));
        assert!((n.drive().load(Ordering::Acquire) - 5.0).abs() < 1e-3);
        // A param this unit doesn't own is a silent no-op.
        n.set(unit_param::setting(UnitParam::Cutoff, 1000.0));
    }

    #[test]
    fn channels_are_independent() {
        let mut n = DistortionNode::new(ShapeKind::HardClip, 1.0);
        let mut o = [0.0f32; 2];
        n.tick(&[4.0, 0.5], &mut o);
        assert!(
            (o[0] - 1.0).abs() < 1e-6,
            "L should clip to 1.0, got {}",
            o[0]
        );
        assert!((o[1] - 0.5).abs() < 1e-6, "R should pass 0.5, got {}", o[1]);
    }

    // ── Modulated drive (the graph's param feed) ────────────────────────────

    /// The feed declares drive, and never changes the arity.
    ///
    /// Mutation (run): declare the feed empty (`ParamFeed::new(&[])`) → the
    /// first assertion fails, and every fed test panics on `feed`.
    #[test]
    fn distortion_declares_its_drive_feed() {
        let mut d = DistortionNode::new(ShapeKind::Tanh, 1.0);
        assert_eq!(
            d.param_feed().map(|f| f.params()),
            Some(&[UnitParam::Drive][..])
        );
        assert_eq!((d.inputs(), d.outputs()), (2, 2));
        assert_eq!(d.param_base(0), Some(1.0), "the base is the drive control");
    }

    #[test]
    fn distortion_unmodulated_matches_held_constant() {
        // A node whose fed drive is held at the same value as a plain node's
        // atomic must produce bit-identical output — the modulated path is a
        // faithful superset. Tanh at drive 5.0 saturates hard enough that any
        // divergence would show.
        let signal: Vec<f32> = (0..512).map(|i| 0.6 * (i as f32 * 0.05).sin()).collect();

        let mut plain = DistortionNode::new(ShapeKind::Tanh, 5.0);
        let plain_out = process_mono_through(&mut plain, &signal);

        let mut modn = DistortionNode::new(ShapeKind::Tanh, 5.0);
        let held = vec![5.0f32; signal.len()];
        let mod_out = crate::testing::tick_fed(&mut modn, &[&signal, &signal], &[Some(&held)]);
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
    /// step's frame on, and a node whose feed is then cleared reads its
    /// control again.
    ///
    /// Mutation (run): shape only channel 0 on the modulated path → the
    /// other channels do not move at the step → fails. Keep reading the
    /// feed after `clear` (ignore `any_live`) → the last block is still
    /// driven hard → fails.
    #[test]
    fn a_fed_drive_shapes_every_channel_of_a_wide_node() {
        let mut n = DistortionNode::with_channels(6, ShapeKind::HardClip, 1.0);
        assert_eq!((n.inputs(), n.outputs()), (6, 6));
        let x = vec![0.25f32; 64];
        let ins: Vec<&[f32]> = (0..6).map(|_| &x[..]).collect();
        let drive: Vec<f32> = (0..64).map(|i| if i < 32 { 1.0 } else { 3.0 }).collect();
        let out = crate::testing::process_fed(&mut n, &ins, &[Some(&drive)]);
        for (c, o) in out.iter().enumerate() {
            assert_eq!(o[31], 0.25, "channel {c} before the step");
            assert_eq!(o[32], 0.75, "channel {c} on the step's frame");
        }
        let out = crate::testing::process_fed(&mut n, &ins, &[None]);
        assert!(
            out.iter().all(|o| o.iter().all(|&y| y == 0.25)),
            "the control again"
        );
    }

    /// The curves, pinned.
    ///
    /// These used to be fundsp's `Shape` impls. When they moved here they were
    /// compared bit for bit against fundsp over 2.16 M (x, drive) points — six
    /// curves, nine drives from 0 to 100, x in [-2, 2] — and matched exactly.
    /// That comparison cannot outlive the move (it needs the fundsp types
    /// `tutti_core::dsp` stopped re-exporting), so what stays is these values.
    ///
    /// The four curves built from `+ * / round floor clamp abs` are pinned
    /// **exactly**: IEEE-754 rounds each of those correctly, so the result is
    /// the same on every platform. `tanh` and `atan` are libm
    /// quality-of-implementation and differ in the last ulp between C runtimes
    /// (the same reason `tutti-core`'s click digest is gated off MSVC), so those
    /// two are pinned against the f64 closed form with a tolerance instead.
    ///
    /// Mutation: dropping `Crush`'s `max(1.0)` floor fails the drive-0.5 row
    /// (round(0.6·0.5) is 0, not 1); dropping Atan's `2/π`
    /// rescale fails its row by ~0.36; swapping Softsign's `abs` for the signed
    /// value fails the −3 row (−3/−2 = 1.5).
    #[test]
    fn curves_are_pinned() {
        let exact = [
            (ShapeKind::HardClip, 0.75, 2.0, 1.0),
            (ShapeKind::HardClip, -0.2, 2.0, -0.4),
            (ShapeKind::HardClip, -3.0, 1.0, -1.0),
            (ShapeKind::Softsign, 1.0, 1.0, 0.5),
            (ShapeKind::Softsign, -3.0, 1.0, -0.75),
            (ShapeKind::Crush, 0.3, 4.0, 0.25),
            // Drive below one level per unit floors to one.
            (ShapeKind::Crush, 0.6, 0.5, 1.0),
            (ShapeKind::Crush, 0.3, 0.5, 0.0),
            // SoftCrush passes through each step's midpoint and each integer.
            (ShapeKind::SoftCrush, 1.5, 1.0, 1.5),
            (ShapeKind::SoftCrush, 1.0, 1.0, 1.0),
            (ShapeKind::SoftCrush, 0.25, 2.0, 0.25),
        ];
        for (kind, x, drive, want) in exact {
            let got = kind.apply(x, Drive(drive));
            assert_eq!(
                got.to_bits(),
                f32::to_bits(want),
                "{kind:?}({x}, drive {drive}) = {got}, want exactly {want}"
            );
        }

        // Off the midpoint the S-curve is a rational: smooth9(1/4) is
        // 12826 / 262144 exactly, so at hardness 1 a quarter step lands there.
        let quarter = ShapeKind::SoftCrush.apply(0.25, Drive(1.0));
        assert!((f64::from(quarter) - 12826.0 / 262144.0).abs() < 1e-7);

        let half_pi = std::f64::consts::FRAC_PI_2;
        let closed = [
            (ShapeKind::Tanh, 0.5f32, 2.0f32, 1.0f64.tanh()),
            (ShapeKind::Tanh, -1.0, 3.0, (-3.0f64).tanh()),
            (ShapeKind::Atan, 1.0, 1.0, half_pi.atan() / half_pi),
            (
                ShapeKind::Atan,
                -0.5,
                4.0,
                (-half_pi * 2.0).atan() / half_pi,
            ),
        ];
        for (kind, x, drive, want) in closed {
            let got = f64::from(kind.apply(x, Drive(drive)));
            assert!(
                (got - want).abs() < 1e-6,
                "{kind:?}({x}, drive {drive}) = {got}, want {want}"
            );
        }
    }
}
