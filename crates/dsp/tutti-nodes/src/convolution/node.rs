//! [`ConvolverNode`]: [`Convolver`] as an `AudioUnit`, at any channel width.
//!
//! One node for every width. It used to be two — a 1-in/1-out `ConvolverNode`
//! and a 2-in/2-out `StereoConvolverNode` — whose bodies were the same
//! per-channel "convolve, gain, delay the dry, blend" step written once for
//! one channel and once for a hardcoded stereo pair. The width is now a
//! property of the value (how many convolvers it was built with), and the IR
//! wiring that made the stereo type more than two mono ones survives as
//! [`IrChannelConfig`], generalised to N channels.
//!
//! The node composes a [`Convolver`] per channel (the DSP engine) with one
//! [`WetDry`] parameter group (the user-facing knobs), shared across channels.
//!
//! **Each channel owns its own copy of its IR's FFT partitions**, including
//! under [`IrChannelConfig::Mono`] where every channel's IR is the same.
//! `fft-convolver` keeps the transformed partitions inside its convolver and
//! offers no way to share them, so storing the IR once (`Arc<[f32]>`) waits on
//! owning the FFT state here — design doc 013's convolver row, which needs the
//! graph's `Fork` to stop cloning the node on commit anyway.

use tutti_core::Arc;
use tutti_core::AtomicF32;
use tutti_core::{
    fold_frame_to_mono, Amplitude, AudioUnit, BufferMut, BufferRef, ChannelLayout, Mix, SampleRate,
    Samples, SignalFrame, MAX_BUFFER_SIZE,
};

use tutti_core::Tail;

use super::convolver::Convolver;
use super::params::WetDry;
use crate::buffer::CircularBuffer;

/// The dry path's alignment delay: holds the input for exactly the convolver's
/// latency, so the blend adds the dry sample that arrived *with* the wet one.
///
/// Without it the node blended the undelayed input against a wet signal one FFT
/// block late, while `route` reported the block for the whole output — so after
/// PDC the dry half of any `mix < 1` led the mix by `latency` samples, and the
/// blend itself comb-filtered. The latency is the convolver's block size, fixed
/// at construction and independent of the sample rate, so the ring is sized once
/// there and `set_sample_rate` has nothing to resize.
#[derive(Clone)]
pub(super) struct DryAlign {
    ring: CircularBuffer<f32>,
}

impl DryAlign {
    /// Allocates — construction only. `latency` is at least 2 (the convolver
    /// rounds its block up to a power of two no smaller than that), so the
    /// one-slot clamp in [`CircularBuffer::new`] never shortens the delay.
    pub(super) fn new(latency: usize) -> Self {
        debug_assert!(latency >= 1, "a zero-latency convolver needs no dry align");
        Self {
            ring: CircularBuffer::new(latency),
        }
    }

    /// Push `input`, return the sample pushed `latency` calls ago.
    ///
    /// The ring holds exactly `latency` samples, so the oldest one — read
    /// before the push overwrites its slot — is `latency` pushes old.
    #[inline]
    pub(super) fn step(&mut self, input: f32) -> f32 {
        let delayed = self.ring.read_back(self.ring.len() - 1);
        self.ring.push(input);
        delayed
    }

    pub(super) fn clear(&mut self) {
        self.ring.clear();
    }

    #[inline]
    pub(super) fn footprint(&self) -> usize {
        self.ring.len() * core::mem::size_of::<f32>()
    }
}

/// The tail of an FIR whose impulse response is `ir_length` samples long.
///
/// One less than the response, because a tail counts the frames produced *after*
/// the input stops: an IR of length `L` answers an impulse at frame 0 through
/// frame `L - 1`, so `L - 1` of them land past the input. Defined this way the
/// figure adds along a chain with no correction term, which is what
/// [`tutti_core::tail`] relies on.
///
/// A convolution is linear and finite, so this is exact rather than an estimate
/// — the only node here that can say that.
pub(super) fn ring_out(ir_length: usize) -> Tail {
    match ir_length {
        // An empty or single-sample IR produces nothing after its input stops.
        0 | 1 => Tail::None,
        n => Tail::Finite(Samples(n - 1)),
    }
}

/// How a [`ConvolverNode`]'s impulse responses map onto its channels.
///
/// Mirrors Ardour's convolution-reverb wiring options, generalised from a
/// stereo pair to any width. Every configuration has as many inputs as
/// outputs, and every channel's dry path is that channel's own input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IrChannelConfig {
    /// One IR applied independently to every channel — the same room heard
    /// from each channel's own position, with no cross-coupling. A 1-channel
    /// node is this configuration.
    Mono,
    /// All inputs folded to mono (the engine's [`fold_frame_to_mono`]), then
    /// convolved with one IR per output channel. At width 2 this is the classic
    /// mono-to-stereo reverb: one source, two decorrelated tails.
    MonoToStereo,
    /// One IR per channel, channel `c` convolved with IR `c` and nothing else.
    /// At width 2 this is true stereo (L with IR-L, R with IR-R).
    Stereo,
}

/// Convolution reverb as an [`AudioUnit`], `width`-in / `width`-out.
///
/// Built at a fixed width with an [`IrChannelConfig`] saying which IR each
/// channel hears; [`new`](Self::new) / [`with_ir`](Self::with_ir) build the
/// 1-channel node. Latency is one FFT block, the same on every channel, reported
/// through [`AudioUnit::route`] for the **whole** output: each channel's dry
/// half is delayed by the same block inside the node, so wet and dry leave
/// aligned (design doc 013, D3).
///
/// The wet/dry [`Mix`] and wet-path [`Amplitude`] are one [`WetDry`] block
/// shared by every channel, read once per `process` call.
///
/// # The sample rate is not this node's to fix
///
/// Unlike the other rate-dependent nodes in this crate, calling
/// `AudioUnit::set_sample_rate` fixes nothing here — it stores the rate and
/// nothing else. The IRs are taken as bare `&[f32]` with no rate attached, so
/// the node cannot tell what rate they were measured at and has nothing to
/// resample from or to.
///
/// The consequence is the caller's to avoid: an IR captured at 44.1 kHz and
/// convolved at 48 kHz plays back 8.8% short and correspondingly bright — a
/// reverb tail that decays too fast, which reads as a different room rather
/// than as an error. Supply IRs already at the graph's rate.
#[derive(Clone)]
pub struct ConvolverNode {
    /// One convolver per channel; `convolvers.len()` is the width. Built at
    /// construction — never resized in `tick`/`process` (RT no-alloc).
    convolvers: Vec<Convolver>,
    /// Per-channel dry alignment. Each channel's *own* input is what gets
    /// delayed — the dry half of a [`MonoToStereo`](IrChannelConfig::MonoToStereo)
    /// blend is still channel `c`'s input, not the folded mono the IRs see.
    dry: Vec<DryAlign>,
    config: IrChannelConfig,
    params: WetDry,
    /// The rate the host last announced. Seeded at [`SampleRate::DEFAULT`] and
    /// updated by `set_sample_rate`, but **never read** — no coefficient here
    /// derives from it, because an FIR convolution's only time constant is the
    /// IR itself. Kept so the node can answer for its rate if a future
    /// resampling path needs to know what it was built against; see the type
    /// docs for why resampling is deliberately not done.
    ///
    /// [`SampleRate::DEFAULT`]: tutti_core::SampleRate::DEFAULT
    sample_rate: SampleRate,
    latency_samples: usize,
    /// One frame of input, gathered per sample for the
    /// [`MonoToStereo`](IrChannelConfig::MonoToStereo) fold, which takes an
    /// interleaved frame. Width-long, allocated at construction; unused by the
    /// other configurations.
    frame: Vec<f32>,
}

impl ConvolverNode {
    /// The shared body behind every public constructor.
    ///
    /// Seeds the placeholder [`SampleRate::DEFAULT`], which `set_sample_rate`
    /// overwrites without resampling anything — see the type docs.
    ///
    /// [`SampleRate::DEFAULT`]: tutti_core::SampleRate::DEFAULT
    fn build(convolvers: Vec<Convolver>, config: IrChannelConfig) -> Self {
        debug_assert!(!convolvers.is_empty());
        let latency_samples = convolvers[0].latency();
        // Every constructor builds all channels with one block size, so one
        // figure is true of the whole node.
        debug_assert!(convolvers.iter().all(|c| c.latency() == latency_samples));
        let width = convolvers.len();
        Self {
            dry: (0..width).map(|_| DryAlign::new(latency_samples)).collect(),
            convolvers,
            config,
            params: WetDry::default(),
            sample_rate: SampleRate::DEFAULT,
            latency_samples,
            frame: vec![0.0; width],
        }
    }

    /// One convolver per IR, all at `block_size`.
    ///
    /// # Panics
    ///
    /// If `irs` is empty: a node needs at least one channel, and a zero-width
    /// convolver has no meaning to fall back to.
    fn per_ir(irs: &[&[f32]], block_size: usize, config: IrChannelConfig) -> Self {
        assert!(
            !irs.is_empty(),
            "a ConvolverNode needs at least one impulse response"
        );
        Self::build(
            irs.iter()
                .map(|ir| Convolver::new(ir, block_size))
                .collect(),
            config,
        )
    }

    /// A 1-channel convolver from an impulse response with an explicit block
    /// size (rounded up to a power of two; it is the latency).
    ///
    /// The rate caveat on the type applies: `ir` carries no rate and nothing
    /// resamples it.
    pub fn new(ir: &[f32], block_size: usize) -> Self {
        Self::build(vec![Convolver::new(ir, block_size)], IrChannelConfig::Mono)
    }

    /// A 1-channel convolver using the default block size.
    ///
    /// The rate caveat on the type applies unchanged.
    pub fn with_ir(ir: &[f32]) -> Self {
        Self::build(vec![Convolver::with_ir(ir)], IrChannelConfig::Mono)
    }

    /// The same IR applied independently to each of `channels` channels
    /// ([`IrChannelConfig::Mono`]); a width of 0 is clamped to 1.
    ///
    /// The IR is partitioned once and the convolver cloned per channel, so the
    /// FFT setup is paid once; each channel still holds its own copy (see the
    /// module docs).
    pub fn shared_ir(channels: impl Into<ChannelLayout>, ir: &[f32], block_size: usize) -> Self {
        let n = usize::from(channels.into().count()).max(1);
        let head = Convolver::new(ir, block_size);
        Self::build(vec![head; n], IrChannelConfig::Mono)
    }

    /// One IR per channel ([`IrChannelConfig::Stereo`]): channel `c` is
    /// convolved with `irs[c]`, and the width is `irs.len()`.
    ///
    /// # Panics
    ///
    /// If `irs` is empty.
    pub fn per_channel(irs: &[&[f32]], block_size: usize) -> Self {
        Self::per_ir(irs, block_size, IrChannelConfig::Stereo)
    }

    /// Every input folded to mono, then convolved with one IR per output
    /// ([`IrChannelConfig::MonoToStereo`]); the width is `irs.len()`.
    ///
    /// # Panics
    ///
    /// If `irs` is empty.
    pub fn folded(irs: &[&[f32]], block_size: usize) -> Self {
        Self::per_ir(irs, block_size, IrChannelConfig::MonoToStereo)
    }

    /// True stereo: L with `ir_l`, R with `ir_r`. Shorthand for
    /// [`per_channel(&[ir_l, ir_r], …)`](Self::per_channel).
    pub fn stereo(ir_l: &[f32], ir_r: &[f32], block_size: usize) -> Self {
        Self::per_channel(&[ir_l, ir_r], block_size)
    }

    /// L and R summed to mono, then convolved with `ir_l` and `ir_r` into a
    /// stereo pair. Shorthand for [`folded(&[ir_l, ir_r], …)`](Self::folded).
    pub fn mono_to_stereo(ir_l: &[f32], ir_r: &[f32], block_size: usize) -> Self {
        Self::folded(&[ir_l, ir_r], block_size)
    }

    /// How the node's impulse responses map onto its channels.
    ///
    /// Fixed at construction — the config picks which constructor built it, so
    /// changing it means building a new node.
    pub fn config(&self) -> IrChannelConfig {
        self.config
    }

    /// The channel width this node was built for (inputs == outputs).
    pub fn layout(&self) -> ChannelLayout {
        ChannelLayout::from(self.width())
    }

    #[inline]
    fn width(&self) -> usize {
        self.convolvers.len()
    }

    /// The node's [`WetDry`] parameter block, shared across every channel.
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

    /// Sets the wet/dry [`Mix`] for every channel, clamped to `0.0..=1.0`.
    pub fn set_mix(&self, mix: impl Into<Mix>) {
        self.params.set_mix(mix);
    }

    /// Sets the wet-path [`Amplitude`] for every channel, floored at 0.
    pub fn set_gain(&self, gain: impl Into<Amplitude>) {
        self.params.set_gain(gain);
    }

    /// The node's latency in [`Samples`] — one FFT block, the same on every
    /// channel.
    ///
    /// Partitioned convolution cannot emit a sample until its first block is
    /// full, so this delay is inherent. A graph mixing this against a dry path
    /// must compensate it, or the two arrive misaligned and comb-filter.
    pub fn latency_samples(&self) -> Samples {
        Samples(self.latency_samples)
    }

    /// Blend channel `c`'s already-convolved block `out` against its delayed
    /// dry input `x`, in place. `gain` is an [`Amplitude`] beside a typed
    /// [`Mix`]; the samples stay raw.
    #[inline]
    fn blend_channel(dry: &mut DryAlign, x: &[f32], out: &mut [f32], mix: Mix, gain: Amplitude) {
        for (o, &s) in out.iter_mut().zip(x) {
            let wet = *o * gain.get();
            *o = mix.blend(dry.step(s), wet);
        }
    }
}

impl AudioUnit for ConvolverNode {
    fn inputs(&self) -> usize {
        self.width()
    }

    fn outputs(&self) -> usize {
        self.width()
    }

    fn reset(&mut self) {
        for c in &mut self.convolvers {
            c.reset();
        }
        for d in &mut self.dry {
            d.clear();
        }
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        self.sample_rate = sample_rate;
    }

    #[inline]
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        let (mix, gain) = self.params.load();
        let n = self.width();
        let folded = match self.config {
            IrChannelConfig::MonoToStereo => Some(fold_frame_to_mono(&input[..n])),
            IrChannelConfig::Mono | IrChannelConfig::Stereo => None,
        };
        for c in 0..n {
            let x = folded.unwrap_or(input[c]);
            let wet = self.convolvers[c].process_sample(x) * gain.get();
            output[c] = mix.blend(self.dry[c].step(input[c]), wet);
        }
    }

    /// Channel-outer over planar slices: each channel's convolver consumes its
    /// whole block in partition-sized runs, then the blend runs over the block.
    /// `mix` and `gain` are read once, here.
    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        debug_assert!(size <= MAX_BUFFER_SIZE);
        let (mix, gain) = self.params.load();
        let n = self.width();
        match self.config {
            IrChannelConfig::Mono | IrChannelConfig::Stereo => {
                for c in 0..n {
                    let x = &input.channel_f32(c)[..size];
                    let out = &mut output.channel_f32_mut(c)[..size];
                    self.convolvers[c].process_block(x, out);
                    Self::blend_channel(&mut self.dry[c], x, out, mix, gain);
                }
            }
            IrChannelConfig::MonoToStereo => {
                // Folded once per block, not once per output channel. The fold
                // takes an interleaved frame, so each frame is gathered into the
                // width-long scratch first; the result lives on the stack.
                let mut mono = [0.0f32; MAX_BUFFER_SIZE];
                for (i, m) in mono.iter_mut().enumerate().take(size) {
                    for (c, f) in self.frame.iter_mut().enumerate() {
                        *f = input.at_f32(c, i);
                    }
                    *m = fold_frame_to_mono(&self.frame);
                }
                for c in 0..n {
                    let x = &input.channel_f32(c)[..size];
                    let out = &mut output.channel_f32_mut(c)[..size];
                    self.convolvers[c].process_block(&mono[..size], out);
                    Self::blend_channel(&mut self.dry[c], x, out, mix, gain);
                }
            }
        }
    }

    fn set(&mut self, setting: tutti_core::Setting) {
        if let Some((tutti_core::UnitParam::Wet, value)) =
            tutti_core::unit_param::from_setting(&setting)
        {
            self.set_mix(value);
        }
    }

    /// Width 1 keeps the old mono node's id and every wider node the old stereo
    /// one's, so a graph diff sees the same identities it did before the merge.
    fn get_id(&self) -> u64 {
        if self.width() == 1 {
            crate::node_id::CONVOLVER_ID
        } else {
            crate::node_id::STEREO_CONVOLVER_ID
        }
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    /// One FFT block on every channel — for the whole output, wet and dry,
    /// because the dry half is delayed by the same block inside the node.
    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let n = self.width();
        let latency = self.latency_samples as f64;
        let mut out = SignalFrame::new(n);
        for c in 0..n {
            out.set(c, input.at(c).delay(latency));
        }
        out
    }

    /// The longest IR's ring-out: the node falls silent when its slowest
    /// channel does. Exact rather than estimated — a convolution is linear and
    /// finite.
    fn tail(&mut self) -> Tail {
        ring_out(
            self.convolvers
                .iter()
                .map(Convolver::ir_length)
                .max()
                .unwrap_or(0),
        )
    }

    fn footprint(&self) -> usize {
        core::mem::size_of::<Self>()
            + self
                .convolvers
                .iter()
                .map(Convolver::scratch_footprint)
                .sum::<usize>()
            + self.dry.iter().map(DryAlign::footprint).sum::<usize>()
            + self.frame.len() * core::mem::size_of::<f32>()
    }
}

#[cfg(test)]
mod tests {
    use super::super::ir::{generate_room_ir, generate_test_ir};
    use super::*;

    #[test]
    fn mix_is_clamped() {
        let ir = vec![1.0; 64];
        let node = ConvolverNode::new(&ir, 64);
        node.set_mix(1.5);
        assert_eq!(node.mix().load(core::sync::atomic::Ordering::Acquire), 1.0);
        node.set_mix(-0.5);
        assert_eq!(node.mix().load(core::sync::atomic::Ordering::Acquire), 0.0);
    }

    /// Fully dry is the input unchanged — but, since design doc 013's D3 fix,
    /// unchanged *and* `latency_samples` late, because the node reports that
    /// latency for its whole output and PDC compensates the whole output by it.
    /// (This test used to assert the value on the very first tick, which pinned
    /// the defect: a dry half that led the reported latency.)
    #[test]
    fn dry_mix_is_passthrough() {
        let ir = vec![1.0; 64];
        let mut node = ConvolverNode::new(&ir, 64);
        node.set_sample_rate(tutti_core::SampleRate(48_000.0));
        node.set_mix(0.0);
        let latency = node.latency_samples().0;

        let mut out = [0.0f32; 1];
        let mut got = Vec::new();
        for i in 0..latency * 2 {
            node.tick(&[0.5 + i as f32 * 1e-3], &mut out);
            got.push(out[0]);
        }
        for (i, &s) in got.iter().enumerate() {
            let want = if i < latency {
                0.0
            } else {
                0.5 + (i - latency) as f32 * 1e-3
            };
            assert!((s - want).abs() < 1e-6, "frame {i}: {s}, expected {want}");
        }
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
        let mut node = ConvolverNode::stereo(&ir_l, &ir_r, 64);
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
