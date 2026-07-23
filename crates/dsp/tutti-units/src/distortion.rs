//! Waveshaping distortion node backed by fundsp's [`Shape`](tutti_core::dsp::Shape)
//! waveshapers.
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
//! kind is a different `EffectKind`, which dawai handles as remove + add (a
//! respawn), exactly like switching filter type.
//!
//! 2 inputs / 2 outputs. The shaper is memoryless, so the two channels are
//! fully independent and stereo is just the same curve applied per channel.
//!
//! # Port layout
//!
//! The default node is 2-in / 2-out (stereo audio on ports 0/1). For audio-rate
//! drive modulation it can grow an *optional drive param-input port* after the
//! audio inputs (see [`DistortionNode::with_param_inputs`]): when
//! [`DistortionNode::mod_drive`] is set, port 2 carries the drive and
//! **overrides** the `drive` atomic per sample (rebuilding the stateless shaper
//! when it moves). When the flag is unset the node is a plain 2-in/2-out node —
//! bit-identical output to the unmodulated path and zero added cost.

use tutti_core::Arc;
use tutti_core::AtomicF32;
use tutti_core::{
    dsp::{Atan, Clip, Crush, Shape, SoftCrush, Softsign, Tanh},
    AudioUnit, BufferMut, BufferRef, SignalFrame,
};
use tutti_core::{Linear, Param};

/// Selects which fundsp waveshaper a [`DistortionNode`] applies.
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
    /// Hard clip to ±1 (fundsp `Clip`).
    HardClip,
    /// Bitcrush-style staircase quantization.
    Crush,
    /// Smoothed staircase quantization.
    SoftCrush,
}

/// One of fundsp's stateless shapers, monomorphized behind an enum so the kind
/// is a runtime field. `drive` maps to each shaper's hardness/levels argument.
#[derive(Clone)]
enum Shaper {
    Tanh(Tanh),
    Atan(Atan),
    Softsign(Softsign),
    HardClip(Clip),
    Crush(Crush),
    SoftCrush(SoftCrush),
}

impl Shaper {
    fn build(kind: ShapeKind, drive: f32) -> Self {
        match kind {
            ShapeKind::Tanh => Shaper::Tanh(Tanh(drive)),
            ShapeKind::Atan => Shaper::Atan(Atan(drive)),
            ShapeKind::Softsign => Shaper::Softsign(Softsign(drive)),
            ShapeKind::HardClip => Shaper::HardClip(Clip(drive)),
            ShapeKind::Crush => Shaper::Crush(Crush(drive.max(1.0))),
            ShapeKind::SoftCrush => Shaper::SoftCrush(SoftCrush(drive.max(1.0))),
        }
    }

    #[inline]
    fn shape(&mut self, x: f32) -> f32 {
        match self {
            Shaper::Tanh(s) => s.shape(x),
            Shaper::Atan(s) => s.shape(x),
            Shaper::Softsign(s) => s.shape(x),
            Shaper::HardClip(s) => s.shape(x),
            Shaper::Crush(s) => s.shape(x),
            Shaper::SoftCrush(s) => s.shape(x),
        }
    }
}

/// Stereo waveshaping distortion. 2 inputs, 2 outputs.
///
/// `drive` is live-modulatable via the [`Param`] atomic (UI handle) and the
/// `UnitParam::Drive` setting path; the waveshape `kind` is set at construction.
pub struct DistortionNode {
    kind: ShapeKind,
    drive: Param<Linear>,
    shaper: Shaper,
    last_drive: f32,
    /// When true, a drive param-input port follows the two audio inputs (port 2)
    /// and overrides [`Self::drive`] per sample.
    mod_drive: bool,
}

impl DistortionNode {
    pub fn new(kind: ShapeKind, drive: impl Into<Linear>) -> Self {
        let drive = drive.into();
        let d = drive.get().max(0.0);
        Self {
            kind,
            drive: Param::new(drive),
            shaper: Shaper::build(kind, d),
            last_drive: d,
            mod_drive: false,
        }
    }

    /// A node with an optional audio-rate drive param-input port. `mod_drive`
    /// adds a drive param-input port after the two audio inputs (port 2),
    /// overriding the atomic per sample when present. The atomic still holds the
    /// base (it feeds the upstream param-sum's base port), so the UI handle path
    /// is unchanged.
    pub fn with_param_inputs(kind: ShapeKind, drive: impl Into<Linear>, mod_drive: bool) -> Self {
        let drive = drive.into();
        let d = drive.get().max(0.0);
        Self {
            kind,
            drive: Param::new(drive),
            shaper: Shaper::build(kind, d),
            last_drive: d,
            mod_drive,
        }
    }

    /// Input-port index of the drive param input, if present (right after the
    /// two audio inputs).
    #[inline]
    pub fn drive_port(&self) -> Option<usize> {
        self.mod_drive.then_some(2)
    }

    /// Atomic handle for the UI / automation to share the drive cell.
    pub fn drive(&self) -> Arc<AtomicF32> {
        self.drive.as_atomic()
    }

    pub fn set_drive(&self, drive: impl Into<Linear>) {
        self.drive.store(Linear(drive.into().get().max(0.0)));
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
            shaper: self.shaper.clone(),
            last_drive: self.last_drive,
            mod_drive: self.mod_drive,
        }
    }
}

impl AudioUnit for DistortionNode {
    fn inputs(&self) -> usize {
        2 + self.mod_drive as usize
    }

    fn outputs(&self) -> usize {
        2
    }

    fn reset(&mut self) {}

    #[inline]
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        // Effective drive: a present param-input port overrides the atomic (the
        // atomic carries the base, fed upstream into the param sum).
        match self.drive_port() {
            None => self.maybe_update(),
            Some(p) => self.maybe_update_modulated(input[p].max(0.0)),
        }
        output[0] = self.shaper.shape(input[0]);
        output[1] = self.shaper.shape(input[1]);
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        // Fast path: no drive port — block-rate shaper update, bit-identical to
        // before.
        let Some(drive_port) = self.drive_port() else {
            self.maybe_update();
            for i in 0..size {
                output.set_f32(0, i, self.shaper.shape(input.at_f32(0, i)));
                output.set_f32(1, i, self.shaper.shape(input.at_f32(1, i)));
            }
            return;
        };
        // Modulated path: read the drive port per sample and rebuild the shaper
        // when it moves before shaping both channels.
        for i in 0..size {
            self.maybe_update_modulated(input.at_f32(drive_port, i).max(0.0));
            output.set_f32(0, i, self.shaper.shape(input.at_f32(0, i)));
            output.set_f32(1, i, self.shaper.shape(input.at_f32(1, i)));
        }
    }

    fn set(&mut self, setting: tutti_core::dsp::Setting) {
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
        // mark both channels as unknown-value (latency 0, no constant prop).
        let mut out = SignalFrame::new(2);
        out.set(0, input.at(0).distort(0.0));
        out.set(1, input.at(1).distort(0.0));
        out
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

    // ── Audio-rate drive param-input port ────────────────────────────────────

    #[test]
    fn distortion_default_is_two_in_no_port() {
        let n = DistortionNode::new(ShapeKind::Tanh, 1.0);
        assert_eq!(n.inputs(), 2);
        assert_eq!(n.outputs(), 2);
        assert_eq!(n.drive_port(), None);
    }

    #[test]
    fn distortion_drive_port_arity() {
        let n = DistortionNode::with_param_inputs(ShapeKind::Tanh, 1.0, true);
        assert_eq!(n.inputs(), 3);
        assert_eq!(n.outputs(), 2);
        assert_eq!(n.drive_port(), Some(2));
    }

    #[test]
    fn distortion_unmodulated_matches_held_constant() {
        // A node whose drive port is held at the same value as a plain node's
        // atomic must produce bit-identical output — the modulated path is a
        // faithful superset. Tanh at drive 5.0 saturates hard enough that any
        // divergence would show.
        let signal: Vec<f32> = (0..512).map(|i| 0.6 * (i as f32 * 0.05).sin()).collect();

        let mut plain = DistortionNode::new(ShapeKind::Tanh, 5.0);
        let plain_out = process_mono_through(&mut plain, &signal);

        let mut modn = DistortionNode::with_param_inputs(ShapeKind::Tanh, 5.0, true);
        let mut mod_out = vec![0.0f32; signal.len()];
        for (i, &x) in signal.iter().enumerate() {
            let mut o = [0.0f32; 2];
            // 3-in/2-out: drive held at the atomic value on port 2.
            modn.tick(&[x, x, 5.0], &mut o);
            mod_out[i] = o[0];
        }
        for i in 0..signal.len() {
            assert!(
                (plain_out[i] - mod_out[i]).abs() < 1e-6,
                "modulated-held output diverges from plain at sample {i}: {} vs {}",
                plain_out[i],
                mod_out[i]
            );
        }
    }
}
