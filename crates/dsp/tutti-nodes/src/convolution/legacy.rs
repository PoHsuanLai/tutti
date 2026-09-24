//! The pre-merge convolver pair, kept only until the width-generic
//! [`ConvolverNode`](super::ConvolverNode) is proven against it.
//!
//! [`StereoConvolverNode`] is still exported so its callers keep compiling for
//! one commit; `LegacyMonoConvolverNode` is the old 1-in/1-out node, test-only.
//! Both exist so the equivalence suite at the bottom can render old and new
//! side by side. The next commit deletes this file.

use tutti_core::Arc;
use tutti_core::AtomicF32;
use tutti_core::{
    fold_frame_to_mono, Amplitude, AudioUnit, BufferMut, BufferRef, Mix, SampleRate, Samples,
    SignalFrame,
};

use tutti_core::Tail;

use super::convolver::Convolver;
use super::node::{ring_out, DryAlign, IrChannelConfig};
use super::params::WetDry;
use crate::StereoPair;

/// Mono convolution reverb as an [`AudioUnit`].
///
/// Latency is one FFT block, reported through [`AudioUnit::route`] — for the
/// whole output: the dry half of the blend is delayed by the same block, so wet
/// and dry leave aligned.
#[cfg(test)]
#[derive(Clone)]
pub(crate) struct LegacyMonoConvolverNode {
    convolver: Convolver,
    dry: DryAlign,
    params: WetDry,
    /// The rate the host last announced. Seeded at [`SampleRate::DEFAULT`] and
    /// updated by `set_sample_rate`, but **never read** — no coefficient here
    /// derives from it, because an FIR convolution's only time constant is the
    /// IR itself. Kept so the node can answer for its rate if a future
    /// resampling path needs to know what it was built against; see the
    /// constructors for why resampling is deliberately not done.
    ///
    /// [`SampleRate::DEFAULT`]: tutti_core::SampleRate::DEFAULT
    sample_rate: SampleRate,
    latency_samples: usize,
}

#[cfg(test)]
// A test-only oracle: only what the equivalence suite drives is used.
#[allow(dead_code)]
impl LegacyMonoConvolverNode {
    /// Build from an impulse response with an explicit block size.
    ///
    /// **Unlike the other rate-dependent nodes in this crate, calling
    /// `AudioUnit::set_sample_rate` fixes nothing here** — it stores the rate
    /// and nothing else. `ir` is taken as a bare `&[f32]` with no rate attached,
    /// so the node cannot tell what rate it was measured at and has nothing to
    /// resample from or to.
    ///
    /// The consequence is the caller's to avoid: an IR captured at 44.1 kHz and
    /// convolved at 48 kHz plays back 8.8% short and correspondingly bright —
    /// a reverb tail that decays too fast, which reads as a different room
    /// rather than as an error. Supply an IR already at the graph's rate.
    pub fn new(ir: &[f32], block_size: usize) -> Self {
        let convolver = Convolver::new(ir, block_size);
        let latency_samples = convolver.latency();
        Self {
            dry: DryAlign::new(latency_samples),
            convolver,
            params: WetDry::default(),
            sample_rate: SampleRate::DEFAULT,
            latency_samples,
        }
    }

    /// Build from an impulse response using the default block size.
    ///
    /// The rate caveat on [`new`](Self::new) applies unchanged: the IR carries
    /// no rate, `set_sample_rate` resamples nothing, and supplying an IR at a
    /// rate other than the graph's skews the whole tail.
    pub fn with_ir(ir: &[f32]) -> Self {
        let convolver = Convolver::with_ir(ir);
        let latency_samples = convolver.latency();
        Self {
            dry: DryAlign::new(latency_samples),
            convolver,
            params: WetDry::default(),
            sample_rate: SampleRate::DEFAULT,
            latency_samples,
        }
    }

    /// The node's [`WetDry`] parameter block, for reading both cells at once.
    pub fn params(&self) -> &WetDry {
        &self.params
    }

    /// The shared wet/dry [`Mix`] cell: `0.0` dry, `1.0` fully wet.
    pub fn mix(&self) -> Arc<AtomicF32> {
        self.params.mix_handle()
    }

    /// The shared wet-path [`Amplitude`] cell, applied before the blend.
    pub fn gain(&self) -> Arc<AtomicF32> {
        self.params.gain_handle()
    }

    /// Sets the wet/dry [`Mix`], clamped to `0.0..=1.0`.
    pub fn set_mix(&self, mix: impl Into<Mix>) {
        self.params.set_mix(mix);
    }

    /// Sets the wet-path [`Amplitude`], floored at 0.
    pub fn set_gain(&self, gain: impl Into<Amplitude>) {
        self.params.set_gain(gain);
    }

    /// The node's latency in [`Samples`] — one FFT block.
    ///
    /// Partitioned convolution cannot emit a sample until its first block is
    /// full, so this delay is inherent. A graph mixing this against a dry path
    /// must compensate it, or the two arrive misaligned and comb-filter.
    pub fn latency_samples(&self) -> Samples {
        Samples(self.latency_samples)
    }

    #[inline]
    /// `gain` is an [`Amplitude`] beside an already-typed [`Mix`] — it was the
    /// one bare control in the signature, and `params.load()` returns it typed.
    fn process_sample(&mut self, input: f32, mix: Mix, gain: Amplitude) -> f32 {
        let wet = self.convolver.process_sample(input) * gain.get();
        let dry = self.dry.step(input);
        mix.blend(dry, wet)
    }
}

#[cfg(test)]
impl AudioUnit for LegacyMonoConvolverNode {
    fn inputs(&self) -> usize {
        1
    }

    fn outputs(&self) -> usize {
        1
    }

    fn reset(&mut self) {
        self.convolver.reset();
        self.dry.clear();
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

    fn set(&mut self, setting: tutti_core::Setting) {
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

    /// Exactly the impulse response's ring-out — the one node here whose tail is
    /// known rather than estimated.
    fn tail(&mut self) -> Tail {
        ring_out(self.convolver.ir_length())
    }

    fn footprint(&self) -> usize {
        core::mem::size_of::<Self>() + self.convolver.scratch_footprint() + self.dry.footprint()
    }
}

/// Stereo convolution reverb as an [`AudioUnit`]. 2-in, 2-out.

#[derive(Clone)]
pub struct StereoConvolverNode {
    channels: StereoPair<Convolver>,
    /// Per-channel dry alignment, as on the mono node. Each channel's *own*
    /// input is what gets delayed — the dry half of a `MonoToStereo` blend is
    /// still `in_l`/`in_r`, not the folded mono the IRs see.
    dry: StereoPair<DryAlign>,
    config: IrChannelConfig,
    params: WetDry,
    /// As `ConvolverNode::sample_rate`: stored, never read.
    sample_rate: SampleRate,
    latency_samples: usize,
}

impl StereoConvolverNode {
    /// The shared body behind [`mono`](Self::mono), [`stereo`](Self::stereo) and
    /// [`mono_to_stereo`](Self::mono_to_stereo).
    ///
    /// Seeds the placeholder [`SampleRate::DEFAULT`], which `set_sample_rate`
    /// overwrites without resampling anything — the rate caveat on
    /// `ConvolverNode::new` applies to every public constructor here.
    ///
    /// [`SampleRate::DEFAULT`]: tutti_core::SampleRate::DEFAULT
    fn build(l: Convolver, r: Convolver, config: IrChannelConfig) -> Self {
        let latency_samples = l.latency();
        // Both convolvers are built with the same block size by every
        // constructor, so one figure is true of both channels.
        debug_assert_eq!(latency_samples, r.latency());
        Self {
            dry: StereoPair::new(
                DryAlign::new(latency_samples),
                DryAlign::new(latency_samples),
            ),
            channels: StereoPair::new(l, r),
            config,
            params: WetDry::default(),
            sample_rate: SampleRate::DEFAULT,
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

    /// How the node's impulse responses map onto its two channels.
    ///
    /// Fixed at construction — the config picks which constructor built it, so
    /// changing it means building a new node.
    pub fn config(&self) -> IrChannelConfig {
        self.config
    }

    /// The node's [`WetDry`] parameter block, shared across both channels.
    pub fn params(&self) -> &WetDry {
        &self.params
    }

    /// The shared wet/dry [`Mix`] cell, governing both channels.
    pub fn mix(&self) -> Arc<AtomicF32> {
        self.params.mix_handle()
    }

    /// The shared wet-path [`Amplitude`] cell, governing both channels.
    pub fn gain(&self) -> Arc<AtomicF32> {
        self.params.gain_handle()
    }

    /// Sets the wet/dry [`Mix`] for both channels, clamped to `0.0..=1.0`.
    pub fn set_mix(&self, mix: impl Into<Mix>) {
        self.params.set_mix(mix);
    }

    /// Sets the wet-path [`Amplitude`] for both channels, floored at 0.
    pub fn set_gain(&self, gain: impl Into<Amplitude>) {
        self.params.set_gain(gain);
    }

    /// The node's latency in [`Samples`] — one FFT block, the same on both
    /// channels.
    ///
    /// Must be compensated by any graph mixing this against a dry path.
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
        let dry_l = self.dry.l.step(in_l);
        let dry_r = self.dry.r.step(in_r);
        (mix.blend(dry_l, wet_l), mix.blend(dry_r, wet_r))
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
        self.dry.l.clear();
        self.dry.r.clear();
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

    fn set(&mut self, setting: tutti_core::Setting) {
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

    /// The longer of the two IRs' ring-outs: the pair falls silent when its
    /// slower channel does.
    fn tail(&mut self) -> Tail {
        ring_out(self.channels.l.ir_length().max(self.channels.r.ir_length()))
    }

    fn footprint(&self) -> usize {
        core::mem::size_of::<Self>()
            + self.channels.l.scratch_footprint()
            + self.channels.r.scratch_footprint()
            + self.dry.l.footprint()
            + self.dry.r.footprint()
    }
}

/// Old against new, side by side on the same input: the merged node must be
/// the pair it replaces, bit for bit, at width 1 and in all three stereo
/// configurations, through both `process` (64-frame planar blocks) and `tick`.
///
/// Mutation: in `ConvolverNode::process`, feeding the *folded* mono to the dry
/// path instead of the channel's own input fails `mono_to_stereo`; swapping
/// `mix.blend(dry, wet)` operands fails every case; dropping `* gain.get()`
/// fails every case (gain is 0.7); skipping `process_block`'s cursor advance
/// fails every `/process` case.
#[cfg(test)]
mod equivalence {
    use super::super::ir::generate_test_ir;
    use super::*;
    use crate::ConvolverNode;
    use tutti_core::BufferVec;

    const BLOCK: usize = 64;

    /// Deterministic, broadband, different per channel.
    fn signal(channels: usize, frames: usize) -> Vec<Vec<f32>> {
        let mut state = 0x1234_5678_u32;
        (0..channels)
            .map(|_| {
                (0..frames)
                    .map(|_| {
                        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                        (state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
                    })
                    .collect()
            })
            .collect()
    }

    fn render_process(node: &mut dyn AudioUnit, x: &[Vec<f32>]) -> Vec<Vec<f32>> {
        let ch = node.outputs();
        let frames = x[0].len();
        let mut out = vec![vec![0.0f32; frames]; ch];
        let mut ib = BufferVec::new(node.inputs());
        let mut ob = BufferVec::new(ch);
        let mut at = 0;
        while at < frames {
            let n = BLOCK.min(frames - at);
            for (c, xc) in x.iter().enumerate() {
                for i in 0..n {
                    ib.set_f32(c, i, xc[at + i]);
                }
            }
            node.process(n, &ib.buffer_ref(), &mut ob.buffer_mut());
            for (c, oc) in out.iter_mut().enumerate() {
                for i in 0..n {
                    oc[at + i] = ob.at_f32(c, i);
                }
            }
            at += n;
        }
        out
    }

    fn render_tick(node: &mut dyn AudioUnit, x: &[Vec<f32>]) -> Vec<Vec<f32>> {
        let ch = node.outputs();
        let frames = x[0].len();
        let mut out = vec![vec![0.0f32; frames]; ch];
        let mut fi = vec![0.0f32; x.len()];
        let mut fo = vec![0.0f32; ch];
        for i in 0..frames {
            for (c, xc) in x.iter().enumerate() {
                fi[c] = xc[i];
            }
            node.tick(&fi, &mut fo);
            for (c, oc) in out.iter_mut().enumerate() {
                oc[i] = fo[c];
            }
        }
        out
    }

    fn assert_bits(name: &str, a: &[Vec<f32>], b: &[Vec<f32>]) {
        assert_eq!(a.len(), b.len(), "{name}: width");
        for (c, (ac, bc)) in a.iter().zip(b).enumerate() {
            for (i, (x, y)) in ac.iter().zip(bc).enumerate() {
                assert_eq!(
                    x.to_bits(),
                    y.to_bits(),
                    "{name}: ch{c} frame {i} differs: old {x} new {y}"
                );
            }
        }
    }

    /// Render `old` and `new` both ways, after `set` has given each the same
    /// non-default mix and gain through its own cells, and require equal bits.
    /// 1500 frames: several partitions, and a ragged final block.
    fn check<A: AudioUnit + Clone, B: AudioUnit + Clone>(
        name: &str,
        old: A,
        new: B,
        set: impl Fn(&A, &B),
    ) {
        let x = signal(old.inputs(), 1500);
        set(&old, &new);
        let (mut o1, mut n1) = (old.clone(), new.clone());
        assert_bits(
            &format!("{name}/process"),
            &render_process(&mut o1, &x),
            &render_process(&mut n1, &x),
        );
        let (mut o2, mut n2) = (old, new);
        assert_bits(
            &format!("{name}/tick"),
            &render_tick(&mut o2, &x),
            &render_tick(&mut n2, &x),
        );
    }

    #[test]
    fn the_merged_node_is_bit_identical_to_the_pair_it_replaces() {
        let ir_a = generate_test_ir(700, 0.3, 48_000.0);
        let ir_b = generate_test_ir(300, 0.5, 48_000.0);

        check(
            "width 1",
            LegacyMonoConvolverNode::new(&ir_a, 64),
            ConvolverNode::new(&ir_a, 64),
            |o, n| {
                o.set_mix(0.4);
                o.set_gain(0.7);
                n.set_mix(0.4);
                n.set_gain(0.7);
            },
        );
        check(
            "width 1, default block",
            LegacyMonoConvolverNode::with_ir(&ir_b),
            ConvolverNode::with_ir(&ir_b),
            |_, _| {},
        );
        let set = |o: &StereoConvolverNode, n: &ConvolverNode| {
            o.set_mix(0.4);
            o.set_gain(0.7);
            n.set_mix(0.4);
            n.set_gain(0.7);
        };
        check(
            "mono",
            StereoConvolverNode::mono(&ir_a, 32),
            ConvolverNode::shared_ir(2usize, &ir_a, 32),
            set,
        );
        check(
            "stereo",
            StereoConvolverNode::stereo(&ir_a, &ir_b, 128),
            ConvolverNode::stereo(&ir_a, &ir_b, 128),
            set,
        );
        check(
            "mono_to_stereo",
            StereoConvolverNode::mono_to_stereo(&ir_a, &ir_b, 16),
            ConvolverNode::mono_to_stereo(&ir_a, &ir_b, 16),
            set,
        );
    }
}
