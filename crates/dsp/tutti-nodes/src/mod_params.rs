//! Native `ModParams` impls — each tutti-nodes DSP node declares its own
//! **control-rate**-modulatable params.
//!
//! The [`ModParams`] trait itself lives in `tutti-mod` (it is node-agnostic —
//! it names only [`ParamAddr`] and [`ModTarget`]); the *impls* live here, beside
//! the nodes. The control-rate sibling of [`crate::ParamPorts`] (audio-rate
//! *ports*): a thing is control-rate-modulatable **iff** it implements
//! [`ModParams`], the enforced opt-in.
//!
//! Each impl is a thin dispatch over the node's existing `Arc<AtomicF32>`
//! accessors (`frequency()`, `q()`, `drive()`, …), wrapping the atomic in an
//! [`AtomicTarget`] that mirrors the modulated value straight back into it — so
//! the node's per-sample read (`self.frequency.load()`) is unchanged and the
//! node gains zero RT cost and zero new state.

use std::sync::Arc;

use tutti_core::dsp::Real;
use tutti_core::{ParamAddr, UnitParam};
use tutti_mod::{AtomicTarget, ModParams, ModTarget};

#[cfg(feature = "convolution")]
use crate::StereoConvolverNode;
use crate::{
    BrickwallLimiterNode, ChorusNode, CompressorNode, DistortionNode, EqBandNode, FlangerNode, GateNode,
    LimiterNode, StereoDelayLineNode, StereoLadderFilterNode, StereoPhaserNode,
    StereoSvfFilterNode,
};

/// Wrap a param's shared atomic in an [`AtomicTarget`] over `[min, max]`.
#[inline]
fn atomic_target(
    atomic: Arc<tutti_core::AtomicF32>,
    base: f32,
    min: f32,
    max: f32,
) -> Option<Arc<dyn ModTarget>> {
    Some(Arc::new(AtomicTarget::with_mirror(base, min, max, atomic)))
}

/// A native node only speaks the [`UnitParam`] vocabulary — extract it, or
/// `None` for a foreign [`ParamAddr::Id`].
#[inline]
fn as_unit(param: ParamAddr) -> Option<UnitParam> {
    match param {
        ParamAddr::Unit(u) => Some(u),
        ParamAddr::Id(_) => None,
    }
}

impl<F: Real> ModParams for StereoSvfFilterNode<F> {
    fn mod_target(
        &self,
        p: ParamAddr,
        base: f32,
        min: f32,
        max: f32,
    ) -> Option<Arc<dyn ModTarget>> {
        let atomic = match as_unit(p)? {
            UnitParam::Cutoff => self.frequency(),
            UnitParam::Q => self.q(),
            UnitParam::GainDb => self.gain_db(),
            _ => return None,
        };
        atomic_target(atomic, base, min, max)
    }
}

impl<F: Real> ModParams for StereoLadderFilterNode<F> {
    fn mod_target(
        &self,
        p: ParamAddr,
        base: f32,
        min: f32,
        max: f32,
    ) -> Option<Arc<dyn ModTarget>> {
        let atomic = match as_unit(p)? {
            UnitParam::Cutoff => self.frequency(),
            UnitParam::Q => self.resonance(),
            UnitParam::Drive => self.drive(),
            _ => return None,
        };
        atomic_target(atomic, base, min, max)
    }
}

impl ModParams for StereoDelayLineNode {
    fn mod_target(
        &self,
        p: ParamAddr,
        base: f32,
        min: f32,
        max: f32,
    ) -> Option<Arc<dyn ModTarget>> {
        let atomic = match as_unit(p)? {
            // Left/primary delay time; the stereo pair moves together for a
            // single `DelayTime` mod (per-channel offsets are a separate concern).
            UnitParam::DelayTime => self.delay_time_l(),
            UnitParam::Feedback => self.feedback(),
            UnitParam::Wet => self.mix(),
            _ => return None,
        };
        atomic_target(atomic, base, min, max)
    }
}

impl ModParams for DistortionNode {
    fn mod_target(
        &self,
        p: ParamAddr,
        base: f32,
        min: f32,
        max: f32,
    ) -> Option<Arc<dyn ModTarget>> {
        let atomic = match as_unit(p)? {
            UnitParam::Drive => self.drive(),
            _ => return None,
        };
        atomic_target(atomic, base, min, max)
    }
}

impl ModParams for CompressorNode {
    fn mod_target(
        &self,
        p: ParamAddr,
        base: f32,
        min: f32,
        max: f32,
    ) -> Option<Arc<dyn ModTarget>> {
        let atomic = match as_unit(p)? {
            UnitParam::Threshold => self.threshold(),
            UnitParam::Ratio => self.ratio(),
            UnitParam::Attack => self.attack_time(),
            UnitParam::Release => self.release_time(),
            UnitParam::Makeup => self.makeup_gain(),
            _ => return None,
        };
        atomic_target(atomic, base, min, max)
    }
}

impl ModParams for GateNode {
    fn mod_target(
        &self,
        p: ParamAddr,
        base: f32,
        min: f32,
        max: f32,
    ) -> Option<Arc<dyn ModTarget>> {
        let atomic = match as_unit(p)? {
            UnitParam::Threshold => self.threshold(),
            UnitParam::Attack => self.attack_time(),
            UnitParam::Release => self.release_time(),
            _ => return None,
        };
        atomic_target(atomic, base, min, max)
    }
}

impl ModParams for LimiterNode {
    fn mod_target(
        &self,
        p: ParamAddr,
        base: f32,
        min: f32,
        max: f32,
    ) -> Option<Arc<dyn ModTarget>> {
        let atomic = match as_unit(p)? {
            UnitParam::Ceiling => self.ceiling(),
            UnitParam::Threshold => self.threshold(),
            UnitParam::Release => self.release_time(),
            _ => return None,
        };
        atomic_target(atomic, base, min, max)
    }
}

impl ModParams for BrickwallLimiterNode {
    fn mod_target(
        &self,
        p: ParamAddr,
        base: f32,
        min: f32,
        max: f32,
    ) -> Option<Arc<dyn ModTarget>> {
        // A brickwall limiter is a single-knob effect — only its ceiling moves.
        let atomic = match as_unit(p)? {
            UnitParam::Ceiling => self.ceiling(),
            _ => return None,
        };
        atomic_target(atomic, base, min, max)
    }
}

impl ModParams for ChorusNode {
    fn mod_target(
        &self,
        p: ParamAddr,
        base: f32,
        min: f32,
        max: f32,
    ) -> Option<Arc<dyn ModTarget>> {
        let atomic = match as_unit(p)? {
            UnitParam::Rate => self.rate(),
            UnitParam::Depth => self.depth(),
            UnitParam::Feedback => self.feedback(),
            UnitParam::Wet => self.mix(),
            _ => return None,
        };
        atomic_target(atomic, base, min, max)
    }
}

impl ModParams for FlangerNode {
    fn mod_target(
        &self,
        p: ParamAddr,
        base: f32,
        min: f32,
        max: f32,
    ) -> Option<Arc<dyn ModTarget>> {
        let atomic = match as_unit(p)? {
            UnitParam::Rate => self.rate(),
            UnitParam::Depth => self.depth(),
            UnitParam::Feedback => self.feedback(),
            UnitParam::Wet => self.mix(),
            _ => return None,
        };
        atomic_target(atomic, base, min, max)
    }
}

impl ModParams for StereoPhaserNode {
    fn mod_target(
        &self,
        p: ParamAddr,
        base: f32,
        min: f32,
        max: f32,
    ) -> Option<Arc<dyn ModTarget>> {
        let atomic = match as_unit(p)? {
            UnitParam::Rate => self.rate(),
            UnitParam::Depth => self.depth(),
            UnitParam::Feedback => self.feedback(),
            UnitParam::Wet => self.mix(),
            _ => return None,
        };
        atomic_target(atomic, base, min, max)
    }
}

impl<F: Real> ModParams for EqBandNode<F> {
    fn mod_target(
        &self,
        p: ParamAddr,
        base: f32,
        min: f32,
        max: f32,
    ) -> Option<Arc<dyn ModTarget>> {
        let atomic = match as_unit(p)? {
            UnitParam::Cutoff => self.frequency(),
            UnitParam::Q => self.q(),
            UnitParam::GainDb => self.gain_db(),
            _ => return None,
        };
        atomic_target(atomic, base, min, max)
    }
}

#[cfg(feature = "convolution")]
impl ModParams for StereoConvolverNode {
    fn mod_target(
        &self,
        p: ParamAddr,
        base: f32,
        min: f32,
        max: f32,
    ) -> Option<Arc<dyn ModTarget>> {
        // Room size / early-reflection shape is baked into the IR at load time
        // (no live atomic), so only the wet/dry mix is control-rate modulatable.
        let atomic = match as_unit(p)? {
            UnitParam::Wet => self.mix(),
            _ => return None,
        };
        atomic_target(atomic, base, min, max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LadderType;
    use tutti_mod::LayerKey;

    /// `UnitParam` → `ParamAddr::Unit` (via `From`) at the call site.
    fn unit(p: UnitParam) -> ParamAddr {
        p.into()
    }

    #[test]
    fn filter_cutoff_is_modulatable_and_moves_the_atomic() {
        let node = StereoSvfFilterNode::<f32>::new(crate::SvfType::LowPass, 1000.0, 0.7);
        let atomic = node.frequency(); // the node reads this per-sample
        let target = node
            .mod_target(unit(UnitParam::Cutoff), 1000.0, 20.0, 20000.0)
            .expect("cutoff is modulatable");

        // Accumulating an offset moves the node's OWN atomic.
        target.accumulate(LayerKey(1), 500.0);
        assert!(
            (atomic.load(core::sync::atomic::Ordering::Acquire) - 1500.0).abs() < 1e-3,
            "the node's frequency atomic reflects the modulation"
        );
        target.clear(LayerKey(1));
        assert!((atomic.load(core::sync::atomic::Ordering::Acquire) - 1000.0).abs() < 1e-3);
    }

    #[test]
    fn unmodulatable_param_returns_none() {
        let node = StereoSvfFilterNode::<f32>::new(crate::SvfType::LowPass, 1000.0, 0.7);
        // SVF has no Drive param.
        assert!(node
            .mod_target(unit(UnitParam::Drive), 0.0, 0.0, 1.0)
            .is_none());
    }

    #[test]
    fn native_node_ignores_a_foreign_id() {
        // A native node speaks UnitParam only — a ParamAddr::Id (a plugin's own
        // numbering) is not its vocabulary, so it returns None.
        let node = StereoSvfFilterNode::<f32>::new(crate::SvfType::LowPass, 1000.0, 0.7);
        assert!(node
            .mod_target(ParamAddr::Id(0), 1000.0, 20.0, 20000.0)
            .is_none());
        assert!(node
            .mod_target(ParamAddr::Id(47), 1000.0, 20.0, 20000.0)
            .is_none());
    }

    #[test]
    fn ladder_maps_q_to_resonance_and_has_drive() {
        let node = StereoLadderFilterNode::<f32>::new(LadderType::LP24, 800.0, 0.5);
        assert!(node
            .mod_target(unit(UnitParam::Cutoff), 800.0, 20.0, 20000.0)
            .is_some());
        assert!(node.mod_target(unit(UnitParam::Q), 0.5, 0.0, 1.0).is_some());
        assert!(node
            .mod_target(unit(UnitParam::Drive), 1.0, 0.0, 4.0)
            .is_some());
        assert!(node
            .mod_target(unit(UnitParam::Feedback), 0.0, 0.0, 1.0)
            .is_none());
    }

    // ── End-to-end: modulate a REAL audio node through the ModMatrix ──

    #[test]
    fn lfo_sweeps_a_real_filter_cutoff_through_the_matrix() {
        use tutti_core::{Beat, BeatDuration, Seconds};
        use tutti_mod::{Lfo, LfoShape, ModMatrix, SourceRate};

        // A real filter node. The node reads `frequency()` per sample.
        let filter = StereoSvfFilterNode::<f32>::new(crate::SvfType::LowPass, 1000.0, 0.7);
        let cutoff_atomic = filter.frequency();

        // Ask the node for its cutoff target and hand it to the matrix.
        let target = filter
            .mod_target(unit(UnitParam::Cutoff), 1000.0, 20.0, 20000.0)
            .expect("cutoff is modulatable");

        let mut m = ModMatrix::new();
        let cutoff = m.add_target(target); // register the node's OWN target
                                           // Beat-synced at 1 cycle/beat → phase == beat, so passing beat = i/16
                                           // walks a full sine cycle, exactly as the old bare-phase test did.
        m.route(
            Lfo::new(LfoShape::Sine),
            SourceRate::beat_synced(BeatDuration(1.0), 0.0),
        )
        .to(&cutoff)
        .depth(0.5);
        let mut driver = m.build();

        // Drive the matrix each frame → the filter's real cutoff atomic sweeps.
        let mut moved_up = false;
        let mut moved_down = false;
        for i in 0..16 {
            driver.run(Beat(i as f64 / 16.0), Seconds(0.0));
            let hz = cutoff_atomic.load(core::sync::atomic::Ordering::Acquire);
            assert!(
                (20.0..=20000.0).contains(&hz),
                "cutoff left its range: {hz}"
            );
            if hz > 1000.5 {
                moved_up = true;
            }
            if hz < 999.5 {
                moved_down = true;
            }
        }
        assert!(
            moved_up && moved_down,
            "a sine LFO should sweep the cutoff both ways"
        );
    }

    #[test]
    fn chorus_rate_is_modulatable_and_moves_the_atomic() {
        let node = ChorusNode::new();
        let rate_atomic = node.rate();
        let base = rate_atomic.load(core::sync::atomic::Ordering::Acquire);
        let target = node
            .mod_target(unit(UnitParam::Rate), base, 0.01, 10.0)
            .expect("chorus rate is modulatable");

        target.accumulate(LayerKey(1), 2.0);
        assert!(
            (rate_atomic.load(core::sync::atomic::Ordering::Acquire) - (base + 2.0)).abs() < 1e-3,
            "chorus rate atomic reflects the modulation"
        );
        target.clear(LayerKey(1));
        assert!((rate_atomic.load(core::sync::atomic::Ordering::Acquire) - base).abs() < 1e-3);

        // A param it doesn't expose returns None.
        assert!(node
            .mod_target(unit(UnitParam::Cutoff), 0.0, 0.0, 1.0)
            .is_none());
    }

    #[cfg(feature = "convolution")]
    #[test]
    fn convolver_only_exposes_wet() {
        // Room size is baked into the IR; only Wet is control-rate modulatable.
        // A minimal unit IR is fine — we only probe the param surface, not audio.
        let node = StereoConvolverNode::mono(&[1.0], 64);
        assert!(node
            .mod_target(unit(UnitParam::Wet), 0.5, 0.0, 1.0)
            .is_some());
        assert!(node
            .mod_target(unit(UnitParam::RoomSize), 0.5, 0.0, 1.0)
            .is_none());
        assert!(node
            .mod_target(unit(UnitParam::Cutoff), 0.5, 0.0, 1.0)
            .is_none());
    }
}
