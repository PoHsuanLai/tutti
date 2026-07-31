//! `AudioUnit` wrappers around [`Convolver`] for use in a `NodeNetwork`.
//!
//! - [`ConvolverNode`]: mono (1-in, 1-out).
//! - [`StereoConvolverNode`]: stereo (2-in, 2-out), true stereo when
//!   constructed with separate L/R IRs, or mono-summed-to-stereo.
//!
//! Each node composes a [`Convolver`] (DSP engine) with a [`WetDry`]
//! parameter group (user-facing knobs).

use tutti_core::dsp::DEFAULT_SAMPLE_RATE;
use tutti_core::Arc;
use tutti_core::AtomicF32;
use tutti_core::{
    fold_frame_to_mono, Amplitude, AudioUnit, BufferMut, BufferRef, Mix, SampleRate, Samples,
    SignalFrame,
};

use super::convolver::Convolver;
use super::params::WetDry;
use crate::StereoPair;

/// Channel configuration for [`StereoConvolverNode`].
///
/// Mirrors Ardour's convolution-reverb wiring options.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IrChannelConfig {
    /// Same mono IR applied independently to L and R.
    Mono,
    /// Inputs summed to mono, convolved with each of two IRs to
    /// produce an L/R output pair.
    MonoToStereo,
    /// True stereo: L with IR-L, R with IR-R, no cross-coupling.
    Stereo,
}

/// Mono convolution reverb as an [`AudioUnit`].
///
/// Latency is one FFT block, reported through [`AudioUnit::route`].
#[derive(Clone)]
pub struct ConvolverNode {
    convolver: Convolver,
    params: WetDry,
    sample_rate: SampleRate,
    latency_samples: usize,
}

impl ConvolverNode {
    /// Build from an impulse response with an explicit block size.
    pub fn new(ir: &[f32], block_size: usize) -> Self {
        let convolver = Convolver::new(ir, block_size);
        let latency_samples = convolver.latency();
        Self {
            convolver,
            params: WetDry::default(),
            sample_rate: DEFAULT_SAMPLE_RATE,
            latency_samples,
        }
    }

    /// Build from an impulse response using the default block size.
    pub fn with_ir(ir: &[f32]) -> Self {
        let convolver = Convolver::with_ir(ir);
        let latency_samples = convolver.latency();
        Self {
            convolver,
            params: WetDry::default(),
            sample_rate: DEFAULT_SAMPLE_RATE,
            latency_samples,
        }
    }

    pub fn params(&self) -> &WetDry {
        &self.params
    }

    pub fn mix(&self) -> Arc<AtomicF32> {
        self.params.mix_handle()
    }

    pub fn gain(&self) -> Arc<AtomicF32> {
        self.params.gain_handle()
    }

    pub fn set_mix(&self, mix: impl Into<Mix>) {
        self.params.set_mix(mix);
    }

    pub fn set_gain(&self, gain: impl Into<Amplitude>) {
        self.params.set_gain(gain);
    }

    /// Latency (one FFT block).
    pub fn latency_samples(&self) -> Samples {
        Samples(self.latency_samples)
    }

    #[inline]
    /// `gain` is an [`Amplitude`] beside an already-typed [`Mix`] — it was the
    /// one bare control in the signature, and `params.load()` returns it typed.
    fn process_sample(&mut self, input: f32, mix: Mix, gain: Amplitude) -> f32 {
        let wet = self.convolver.process_sample(input) * gain.get();
        mix.blend(input, wet)
    }
}

impl AudioUnit for ConvolverNode {
    fn inputs(&self) -> usize {
        1
    }

    fn outputs(&self) -> usize {
        1
    }

    fn reset(&mut self) {
        self.convolver.reset();
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        self.sample_rate = sample_rate;
    }

    #[inline]
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        // Unwrapped once per block: `process_sample` is the per-sample RT path,
        // where the units are already resolved scratch.
        let (mix, gain) = self.params.load();
        output[0] = self.process_sample(input[0], mix, gain);
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        // Unwrapped once per block: `process_sample` is the per-sample RT path,
        // where the units are already resolved scratch.
        let (mix, gain) = self.params.load();
        for i in 0..size {
            let s = self.process_sample(input.at_f32(0, i), mix, gain);
            output.set_f32(0, i, s);
        }
    }

    fn set(&mut self, setting: tutti_core::dsp::Setting) {
        if let Some((tutti_core::UnitParam::Wet, value)) =
            tutti_core::unit_param::from_setting(&setting)
        {
            self.set_mix(value);
        }
    }

    fn get_id(&self) -> u64 {
        crate::node_id::CONVOLVER_ID
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let mut out = SignalFrame::new(1);
        out.set(0, input.at(0).delay(self.latency_samples as f64));
        out
    }

    fn footprint(&self) -> usize {
        core::mem::size_of::<Self>() + self.convolver.scratch_footprint()
    }
}

/// Stereo convolution reverb as an [`AudioUnit`]. 2-in, 2-out.
#[derive(Clone)]
pub struct StereoConvolverNode {
    channels: StereoPair<Convolver>,
    config: IrChannelConfig,
    params: WetDry,
    sample_rate: SampleRate,
    latency_samples: usize,
}

impl StereoConvolverNode {
    fn build(l: Convolver, r: Convolver, config: IrChannelConfig) -> Self {
        let latency_samples = l.latency();
        Self {
            channels: StereoPair::new(l, r),
            config,
            params: WetDry::default(),
            sample_rate: DEFAULT_SAMPLE_RATE,
            latency_samples,
        }
    }

    /// Apply the same mono IR to both channels.
    pub fn mono(ir: &[f32], block_size: usize) -> Self {
        Self::build(
            Convolver::new(ir, block_size),
            Convolver::new(ir, block_size),
            IrChannelConfig::Mono,
        )
    }

    /// Sum L/R to mono, then process through two independent IRs to
    /// produce a stereo output.
    pub fn mono_to_stereo(ir_l: &[f32], ir_r: &[f32], block_size: usize) -> Self {
        Self::build(
            Convolver::new(ir_l, block_size),
            Convolver::new(ir_r, block_size),
            IrChannelConfig::MonoToStereo,
        )
    }

    /// True stereo: L with `ir_l`, R with `ir_r`.
    pub fn stereo(ir_l: &[f32], ir_r: &[f32], block_size: usize) -> Self {
        Self::build(
            Convolver::new(ir_l, block_size),
            Convolver::new(ir_r, block_size),
            IrChannelConfig::Stereo,
        )
    }

    pub fn config(&self) -> IrChannelConfig {
        self.config
    }

    pub fn params(&self) -> &WetDry {
        &self.params
    }

    pub fn mix(&self) -> Arc<AtomicF32> {
        self.params.mix_handle()
    }

    pub fn gain(&self) -> Arc<AtomicF32> {
        self.params.gain_handle()
    }

    pub fn set_mix(&self, mix: impl Into<Mix>) {
        self.params.set_mix(mix);
    }

    pub fn set_gain(&self, gain: impl Into<Amplitude>) {
        self.params.set_gain(gain);
    }

    pub fn latency_samples(&self) -> Samples {
        Samples(self.latency_samples)
    }

    #[inline]
    /// See the mono twin: `gain` is an [`Amplitude`]. `in_l`/`in_r` stay raw —
    /// they are sample values, not roster quantities.
    fn process_sample(&mut self, in_l: f32, in_r: f32, mix: Mix, gain: Amplitude) -> (f32, f32) {
        let (wet_l, wet_r) = match self.config {
            IrChannelConfig::Mono | IrChannelConfig::Stereo => (
                self.channels.l.process_sample(in_l),
                self.channels.r.process_sample(in_r),
            ),
            IrChannelConfig::MonoToStereo => {
                // The engine's one fold rather than a local `* 0.5` — same value
                // at width 2, one owner for the coefficient.
                let mono = fold_frame_to_mono(&[in_l, in_r]);
                (
                    self.channels.l.process_sample(mono),
                    self.channels.r.process_sample(mono),
                )
            }
        };
        let wet_l = wet_l * gain.get();
        let wet_r = wet_r * gain.get();
        (mix.blend(in_l, wet_l), mix.blend(in_r, wet_r))
    }
}

impl AudioUnit for StereoConvolverNode {
    fn inputs(&self) -> usize {
        2
    }

    fn outputs(&self) -> usize {
        2
    }

    fn reset(&mut self) {
        self.channels.l.reset();
        self.channels.r.reset();
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        self.sample_rate = sample_rate;
    }

    #[inline]
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        // Unwrapped once per block: `process_sample` is the per-sample RT path,
        // where the units are already resolved scratch.
        let (mix, gain) = self.params.load();
        let (out_l, out_r) = self.process_sample(input[0], input[1], mix, gain);
        output[0] = out_l;
        output[1] = out_r;
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        // Unwrapped once per block: `process_sample` is the per-sample RT path,
        // where the units are already resolved scratch.
        let (mix, gain) = self.params.load();
        for i in 0..size {
            let (out_l, out_r) =
                self.process_sample(input.at_f32(0, i), input.at_f32(1, i), mix, gain);
            output.set_f32(0, i, out_l);
            output.set_f32(1, i, out_r);
        }
    }

    fn set(&mut self, setting: tutti_core::dsp::Setting) {
        if let Some((tutti_core::UnitParam::Wet, value)) =
            tutti_core::unit_param::from_setting(&setting)
        {
            self.set_mix(value);
        }
    }

    fn get_id(&self) -> u64 {
        crate::node_id::STEREO_CONVOLVER_ID
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let mut out = SignalFrame::new(2);
        let latency = self.latency_samples as f64;
        out.set(0, input.at(0).delay(latency));
        out.set(1, input.at(1).delay(latency));
        out
    }

    fn footprint(&self) -> usize {
        core::mem::size_of::<Self>()
            + self.channels.l.scratch_footprint()
            + self.channels.r.scratch_footprint()
    }
}

#[cfg(test)]
mod tests {
    use super::super::ir::{generate_room_ir, generate_test_ir};
    use super::*;

    #[test]
    fn convolver_produces_output_after_latency() {
        let ir = generate_test_ir(1024, 0.5, 48_000.0);
        let mut node = ConvolverNode::new(&ir, 64);
        node.set_sample_rate(tutti_core::SampleRate(48_000.0));
        node.set_mix(1.0);

        let mut out = [0.0f32; 1];
        node.tick(&[1.0], &mut out);
        for _ in 0..512 {
            node.tick(&[0.0], &mut out);
        }
        assert!(
            out[0].is_finite(),
            "convolver output must stay finite, got {}",
            out[0]
        );
    }

    #[test]
    fn mix_is_clamped() {
        let ir = vec![1.0; 64];
        let node = ConvolverNode::new(&ir, 64);
        node.set_mix(1.5);
        assert_eq!(node.mix().load(core::sync::atomic::Ordering::Acquire), 1.0);
        node.set_mix(-0.5);
        assert_eq!(node.mix().load(core::sync::atomic::Ordering::Acquire), 0.0);
    }

    #[test]
    fn dry_mix_is_passthrough() {
        let ir = vec![1.0; 64];
        let mut node = ConvolverNode::new(&ir, 64);
        node.set_sample_rate(tutti_core::SampleRate(48_000.0));
        node.set_mix(0.0);

        let mut out = [0.0f32; 1];
        node.tick(&[0.5], &mut out);
        assert!((out[0] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn reset_zeroes_state() {
        let ir = generate_test_ir(256, 0.2, 48_000.0);
        let mut node = ConvolverNode::new(&ir, 64);
        node.set_sample_rate(tutti_core::SampleRate(48_000.0));

        let mut out = [0.0f32; 1];
        for _ in 0..100 {
            node.tick(&[1.0], &mut out);
        }
        node.reset();
        node.set_mix(1.0);
        node.tick(&[0.0], &mut out);
        assert!(out[0].abs() < 1e-6);
    }

    #[test]
    fn stereo_convolver_true_stereo_produces_finite_output() {
        let ir_l = generate_test_ir(512, 0.3, 48_000.0);
        let ir_r = generate_test_ir(512, 0.4, 48_000.0);
        let mut node = StereoConvolverNode::stereo(&ir_l, &ir_r, 64);
        node.set_sample_rate(tutti_core::SampleRate(48_000.0));
        node.set_mix(0.5);

        let mut out = [0.0f32; 2];
        node.tick(&[1.0, 0.5], &mut out);
        assert!(out[0].is_finite() && out[1].is_finite());
    }

    #[test]
    fn room_ir_has_content() {
        let ir = generate_room_ir(0.5, 1.0, 48_000.0);
        assert!(!ir.is_empty());
        assert_eq!(ir[0], 1.0);
        assert!(ir.iter().any(|&x| x != 0.0));
    }
}
