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
use crate::ramp::{LastGood, Ramp};

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
    /// The `(mix, gain)` the previous block ended on — where this block's
    /// ramp starts. `None` until the first block and after `reset`, which then
    /// start on the current values instead of ramping in from nothing.
    last: Option<(Mix, Amplitude)>,
    /// Last finite mix and gain. Neither feeds recursive state here, but the
    /// block's end is the next block's ramp start, so a NaN would carry into
    /// the block after it (see [`LastGood`]).
    good: [LastGood; 2],
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
            last: None,
            good: [LastGood::new(0.5), LastGood::new(1.0)],
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
    /// dry input `x`, in place, with `mix` and `gain` ramped across the block
    /// (see [`BlendRamp`]). The samples stay raw.
    #[inline]
    fn blend_channel(dry: &mut DryAlign, x: &[f32], out: &mut [f32], ramp: &BlendRamp) {
        if let Some((mix, gain)) = ramp.held() {
            // Nothing moved: constants in the loop, the pre-ramp arithmetic.
            for (o, &s) in out.iter_mut().zip(x) {
                let wet = *o * gain.get();
                *o = mix.blend(dry.step(s), wet);
            }
            return;
        }
        for (i, (o, &s)) in out.iter_mut().zip(x).enumerate() {
            let wet = *o * ramp.gain.at(i);
            *o = Mix(ramp.mix.at(i)).blend(dry.step(s), wet);
        }
    }

    /// Read `mix` and `gain` once, and ramp from where the previous block
    /// ended. Both scale the output directly — a stepped wet gain or blend is a
    /// click — so a change between blocks glides across the next one and lands
    /// exactly on the new value (a block of one, `tick`, takes it at once).
    fn begin_block(&mut self, size: usize) -> BlendRamp {
        let (mix, gain) = self.params.load();
        let mix = Mix(self.good[0].read(mix.get()));
        let gain = Amplitude(self.good[1].read(gain.get()));
        let (from_mix, from_gain) = self.last.unwrap_or((mix, gain));
        self.last = Some((mix, gain));
        BlendRamp {
            mix: Ramp::new(from_mix.get(), mix.get(), size),
            gain: Ramp::new(from_gain.get(), gain.get(), size),
            target: (mix, gain),
        }
    }
}

/// A block's `mix` and `gain` ramps.
struct BlendRamp {
    mix: Ramp,
    gain: Ramp,
    target: (Mix, Amplitude),
}

impl BlendRamp {
    /// The held values when neither control moved this block.
    #[inline]
    fn held(&self) -> Option<(Mix, Amplitude)> {
        (self.mix.is_flat() && self.gain.is_flat()).then_some(self.target)
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
        self.last = None;
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        self.sample_rate = sample_rate;
    }

    #[inline]
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        // A block of one: the ramp lands on the new values at once.
        let (mix, gain) = self.begin_block(1).target;
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
    /// `mix` and `gain` are read once, here, and ramped if they moved.
    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        debug_assert!(size <= MAX_BUFFER_SIZE);
        if size == 0 {
            return;
        }
        let ramp = self.begin_block(size);
        let n = self.width();
        match self.config {
            IrChannelConfig::Mono | IrChannelConfig::Stereo => {
                for c in 0..n {
                    let x = &input.channel_f32(c)[..size];
                    let out = &mut output.channel_f32_mut(c)[..size];
                    self.convolvers[c].process_block(x, out);
                    Self::blend_channel(&mut self.dry[c], x, out, &ramp);
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
                    Self::blend_channel(&mut self.dry[c], x, out, &ramp);
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

#[cfg(test)]
mod golden {
    use super::super::ir::generate_test_ir;
    use super::*;
    use tutti_core::BufferVec;

    const BLOCK: usize = 64;
    const FRAMES: usize = 1500;
    const STRIDE: usize = 131;

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

    fn render(node: &mut ConvolverNode, x: &[Vec<f32>]) -> Vec<Vec<f32>> {
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

    fn cases() -> Vec<(&'static str, ConvolverNode)> {
        let ir_a = generate_test_ir(700, 0.3, 48_000.0);
        let ir_b = generate_test_ir(300, 0.5, 48_000.0);
        vec![
            ("width1", ConvolverNode::new(&ir_a, 64)),
            ("mono", ConvolverNode::shared_ir(2usize, &ir_a, 32)),
            ("stereo", ConvolverNode::stereo(&ir_a, &ir_b, 128)),
            (
                "mono_to_stereo",
                ConvolverNode::mono_to_stereo(&ir_a, &ir_b, 16),
            ),
        ]
    }

    /// Every 131st output sample of each case, captured from this node after
    /// the side-by-side suite proved it bit-identical to the `ConvolverNode` /
    /// `StereoConvolverNode` pair it replaced (the commit before the pair was
    /// deleted). They are the old pair's output, kept as numbers.
    const GOLDEN: [(&str, &[&[f32]]); 4] = [
        (
            "width1",
            &[&[
                0e0,
                -4.6238965e-1,
                3.242588e-1,
                -3.8649054e0,
                -5.7200737e0,
                -4.854725e0,
                -2.3011494e0,
                -1.6904869e0,
                4.523817e-1,
                7.797972e0,
                3.062982e0,
                1.6569192e0,
            ]],
        ),
        (
            "mono",
            &[
                &[
                    0e0,
                    -1.3010089e-1,
                    1.5013993e-2,
                    -3.71195e0,
                    -6.8336673e0,
                    -4.933599e0,
                    -2.8803647e0,
                    -2.3532262e0,
                    2.5494246e0,
                    6.5531964e0,
                    3.8352256e0,
                    8.0454767e-1,
                ],
                &[
                    0e0,
                    -1.3993324e0,
                    -2.1900237e0,
                    -4.236314e0,
                    -5.1030354e0,
                    -5.424291e0,
                    -5.6778274e0,
                    -1.3237945e0,
                    2.1840265e0,
                    3.3884676e0,
                    3.9472766e0,
                    6.0074105e0,
                ],
            ],
        ),
        (
            "stereo",
            &[
                &[
                    0e0,
                    -4.1371405e-1,
                    -4.7441882e-1,
                    -8.431916e-2,
                    -4.752711e0,
                    -5.539441e0,
                    -4.1922398e0,
                    -3.5576627e0,
                    -1.9319957e0,
                    5.6217127e0,
                    6.4064994e0,
                    3.5402339e0,
                ],
                &[
                    0e0,
                    -3.7926427e-1,
                    -2.362441e0,
                    -2.0382354e0,
                    -3.772161e0,
                    -4.292098e0,
                    -6.989455e-1,
                    9.266571e-1,
                    1.429973e0,
                    6.654087e-1,
                    1.5628669e0,
                    3.0640595e0,
                ],
            ],
        ),
        (
            "mono_to_stereo",
            &[
                &[
                    0e0,
                    -1.02339e0,
                    -1.7648482e0,
                    -4.0002947e0,
                    -6.1680894e0,
                    -4.7346864e0,
                    -3.2233002e0,
                    -1.5443184e0,
                    2.4112165e0,
                    4.835819e0,
                    4.30898e0,
                    3.0811038e0,
                ],
                &[
                    0e0,
                    -4.0649652e-1,
                    -1.80603e0,
                    -4.211236e0,
                    -4.4937367e0,
                    -1.5020998e0,
                    2.0637734e0,
                    1.6323472e0,
                    2.229262e0,
                    1.9734172e0,
                    1.4570696e0,
                    8.7183356e-1,
                ],
            ],
        ),
    ];

    /// The merged node still renders what the pair it replaced rendered, at
    /// width 1 and in all three stereo configurations, through `process`.
    ///
    /// The tolerance is relative (`1e-5` of the value, floored at `1e-5`) and
    /// not bit-exact on purpose: the FFT's twiddle factors come from the
    /// platform's `sin`/`cos`, which differ in the last ulp between C runtimes
    /// (CLAUDE.md, "one live platform difference"), and a convolution sums
    /// hundreds of products. Every mutation below moves samples by orders of
    /// magnitude more.
    ///
    /// Mutation: dropping `* gain.get()`, blending the undelayed input instead
    /// of `dry.step(s)`, convolving every channel with `convolvers[0]`, or
    /// blending every channel against channel 0's input each fail this.
    #[test]
    fn output_matches_the_pinned_pre_merge_render() {
        for ((name, mut node), (gname, want)) in cases().into_iter().zip(GOLDEN) {
            assert_eq!(name, gname);
            node.set_mix(0.4);
            node.set_gain(0.7);
            let x = signal(node.inputs(), FRAMES);
            let out = render(&mut node, &x);
            assert_eq!(out.len(), want.len(), "{name}: width");
            for (c, (oc, wc)) in out.iter().zip(want).enumerate() {
                let got: Vec<f32> = oc.iter().step_by(STRIDE).copied().collect();
                assert_eq!(got.len(), wc.len(), "{name} ch{c}: sample count");
                for (k, (&g, &w)) in got.iter().zip(wc.iter()).enumerate() {
                    let tol = 1e-5 * w.abs().max(1.0);
                    assert!(
                        (g - w).abs() <= tol,
                        "{name} ch{c} frame {}: {g} vs pinned {w}",
                        k * STRIDE
                    );
                }
            }
        }
    }

    /// Six channels, six IRs: each channel is exactly a width-1 node with its
    /// own IR on its own input — so nothing bleeds between channels and the
    /// width is not secretly two.
    ///
    /// Distinct inputs per channel are what make "no bleed" follow from the
    /// equality: a channel that read any other channel's input would diverge.
    ///
    /// Mutation: convolving every channel with `self.convolvers[0]`, or blending
    /// channel `c` against `self.dry[0]`/channel 0's input, fails this.
    #[test]
    fn six_channels_convolve_independently() {
        let irs: Vec<Vec<f32>> = (0..6)
            .map(|c| generate_test_ir(200 + 90 * c, 0.2 + 0.05 * c as f32, 48_000.0))
            .collect();
        let refs: Vec<&[f32]> = irs.iter().map(Vec::as_slice).collect();
        let mut wide = ConvolverNode::per_channel(&refs, 64);
        assert_eq!((wide.inputs(), wide.outputs()), (6, 6));
        assert_eq!(wide.layout(), ChannelLayout::from(6u16));
        wide.set_mix(0.3);
        let x = signal(6, 900);
        let got = render(&mut wide, &x);
        for c in 0..6 {
            let mut solo = ConvolverNode::new(&irs[c], 64);
            solo.set_mix(0.3);
            let want = render(&mut solo, &x[c..=c]);
            for (i, (g, w)) in got[c].iter().zip(&want[0]).enumerate() {
                assert_eq!(g.to_bits(), w.to_bits(), "ch{c} frame {i}: {g} vs {w}");
            }
        }
    }

    /// A shared IR at width 6: an impulse on channel 3 comes out of channel 3
    /// alone, dry and wet both exactly `latency_samples` late.
    ///
    /// Mutation: blending the undelayed input instead of `dry.step(s)` puts
    /// channel 3's dry impulse at frame 0; blending every channel against
    /// channel 0's input, or convolving every channel with `convolvers[0]`,
    /// leaks the impulse into the other channels. Both fail this.
    #[test]
    fn a_six_channel_shared_ir_keeps_each_channel_to_itself_and_aligned() {
        let mut node = ConvolverNode::shared_ir(6usize, &[1.0], 64);
        node.set_mix(0.5);
        let latency = node.latency_samples().0;
        assert_eq!(node.latency(), Some(latency as f64));
        let mut x = vec![vec![0.0f32; 4 * latency]; 6];
        x[3][0] = 1.0;
        let out = render(&mut node, &x);
        for (c, oc) in out.iter().enumerate() {
            for (i, &s) in oc.iter().enumerate() {
                let want = if c == 3 && i == latency { 1.0 } else { 0.0 };
                assert!(
                    (s - want).abs() < 1e-6,
                    "ch{c} frame {i}: {s}, expected {want}"
                );
            }
        }
    }

    /// Folded at width 6: every output is the engine's 6-to-1 fold of the frame
    /// convolved with that output's IR (fully wet, so only the IR path shows).
    ///
    /// Mutation: folding with a plain average instead of `fold_frame_to_mono`,
    /// or convolving the channel's own input instead of the fold, fails this.
    #[test]
    fn a_six_channel_fold_convolves_the_engine_fold() {
        let irs: Vec<Vec<f32>> = (0..6)
            .map(|c| generate_test_ir(150 + 40 * c, 0.3, 48_000.0))
            .collect();
        let refs: Vec<&[f32]> = irs.iter().map(Vec::as_slice).collect();
        let mut node = ConvolverNode::folded(&refs, 32);
        node.set_mix(1.0);
        let x = signal(6, 700);
        let got = render(&mut node, &x);
        let mono: Vec<f32> = (0..700)
            .map(|i| {
                let frame: Vec<f32> = x.iter().map(|xc| xc[i]).collect();
                fold_frame_to_mono(&frame)
            })
            .collect();
        for c in 0..6 {
            let mut solo = ConvolverNode::new(&irs[c], 32);
            solo.set_mix(1.0);
            let want = render(&mut solo, std::slice::from_ref(&mono));
            for (i, (g, w)) in got[c].iter().zip(&want[0]).enumerate() {
                assert_eq!(g.to_bits(), w.to_bits(), "ch{c} frame {i}: {g} vs {w}");
            }
        }
    }

    /// A mix or wet-gain change made between blocks is read by the next block
    /// and ramped across it (both scale the output directly, so a step
    /// clicks), ending exactly on the new value.
    ///
    /// Mutation (each run, each fails): ignoring `self.last` in `begin_block`
    /// (both ramps start on the target); starting only the gain ramp on the
    /// target; reading the gain ramp one sample late (the block no longer ends
    /// on the new value).
    #[test]
    fn a_mix_or_gain_change_is_read_next_block_and_ramped_across_it() {
        use crate::test_support::{change_between_blocks, noise};
        let ir = generate_test_ir(256, 0.2, 48_000.0);
        let x = noise(21, 64 * 8);
        let (hist, block) = (&x[..64 * 7], &x[64 * 7..]);
        let make = || {
            let mut n = ConvolverNode::shared_ir(ChannelLayout::STEREO, &ir, 64);
            n.set_sample_rate(SampleRate(48_000.0));
            n
        };
        type Change = fn(&ConvolverNode);
        let changes: [(&str, Change); 2] =
            [("mix", |n| n.set_mix(1.0)), ("gain", |n| n.set_gain(4.0))];
        for (what, change) in changes {
            let run = change_between_blocks(make, change, &[hist, hist], &[block, block]);
            run.assert_ramps_in(what);
            for c in 0..2 {
                assert_eq!(
                    run.r[c][63].to_bits(),
                    run.j[c][63].to_bits(),
                    "{what}: ch{c}: the block must end exactly on the new value"
                );
            }
        }
    }
}
