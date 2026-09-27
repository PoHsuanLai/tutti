//! Chorus and flanger — one width-generic modulated delay, two presets.
//!
//! The two effects were structurally identical (a delay line per channel swept
//! by an LFO, with feedback and a wet/dry mix) and differed only in numbers: a
//! chorus sits at a long base delay (10 ms) so the copy reads as a second voice
//! thickening the sound, a flanger at a short one (1 ms) so the copy combs
//! against the original and the sweep moves the notches. They were two node
//! types wrapping one stereo core; they are now one [`ModDelayNode`] and a
//! [`ModDelayConfig`], at any width.

use tutti_core::{Arc, AtomicF32};
use tutti_core::{ChannelLayout, Feedback, Hz, Mix, Phase, PhaseIncrement, SampleRate, Seconds};
use tutti_graph::{Cx, IntoNode, Io, Node, NodeParts, ParamNode, ParamSet, Prepare, Shape, Status};
use tutti_types::{Tail, UnitParam};

use super::shared::{LfoDrive, TimeModMix};
use crate::delay::{DelayLine, InterpolationMode};
use crate::ramp::Ramp;

/// Everything that distinguishes one modulated-delay effect from another: the
/// delay it sweeps around, how the channels' sweeps are staggered, and the
/// factory defaults.
///
/// The durations are [`Seconds`] and the offset a [`PhaseIncrement`] because
/// they were once three adjacent bare `f32`s: CHORUS's `0.01`/`0.05` could be
/// transposed (base > max, which the line then silently clamps at capacity)
/// and the offset could be written into either.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModDelayConfig {
    /// Centre delay the LFO modulates around.
    pub base_delay: Seconds,
    /// Maximum delay — the delay lines' capacity.
    pub max_delay: Seconds,
    /// How far each channel's sweep is staggered from the previous channel's,
    /// in LFO cycles: channel `c` runs `c × channel_phase_offset` ahead of
    /// channel 0, wrapped. At width 2 this is the old L/R offset exactly.
    /// Replace the whole set with [`ModDelayNode::with_phase_offsets`].
    pub channel_phase_offset: PhaseIncrement,
    /// The range [`ModDelayNode::set_depth`] clamps the sweep depth into.
    pub depth_min: Seconds,
    /// Upper end of that range — what keeps the sweep inside the line (chorus)
    /// or inside comb-filter range (flanger).
    pub depth_max: Seconds,
    /// Initial LFO rate.
    pub rate: Hz,
    /// Initial sweep depth.
    pub depth: Seconds,
    /// Initial feedback.
    pub feedback: Feedback,
    /// Initial wet/dry mix.
    pub mix: Mix,
}

impl ModDelayConfig {
    /// Chorus: 10 ms base delay, a quarter-cycle stagger between channels, 1 Hz
    /// rate, 5 ms sweep depth (clamped to `0..=40 ms`), 0.3 feedback, 50/50
    /// mix. The long base delay is what separates chorus from flanger — the copy
    /// sits far enough behind to read as a second voice, and the stagger sweeps
    /// the channels out of step, which is what makes it feel wide.
    pub const CHORUS: Self = Self {
        base_delay: Seconds(0.01),
        max_delay: Seconds(0.05),
        channel_phase_offset: PhaseIncrement(0.25),
        depth_min: Seconds(0.0),
        depth_max: Seconds(0.04),
        rate: Hz(1.0),
        depth: Seconds(0.005),
        feedback: Feedback(0.3),
        mix: Mix(0.5),
    };

    /// Flanger: 1 ms base delay, a half-cycle stagger, 0.5 Hz rate, 2 ms sweep
    /// depth (clamped to `0.1..=10 ms`), 0.7 feedback, 50/50 mix. The short base
    /// delay lands the copy close enough to interfere with the original,
    /// producing the comb notches whose sweep is the flanger's signature;
    /// feedback sharpens them into the metallic ring.
    pub const FLANGER: Self = Self {
        base_delay: Seconds(0.001),
        max_delay: Seconds(0.02),
        channel_phase_offset: PhaseIncrement(0.5),
        depth_min: Seconds(0.0001),
        depth_max: Seconds(0.01),
        rate: Hz(0.5),
        depth: Seconds(0.002),
        feedback: Feedback(0.7),
        mix: Mix(0.5),
    };
}

/// The block's depth / feedback / mix — where the next block's ramps start.
#[derive(Clone, Copy)]
struct ModDelayControls {
    depth: f32,
    fb: f32,
    mix: f32,
}

/// A modulated delay of any width — chorus or flanger by configuration.
///
/// Each channel has its own delay line swept by one shared LFO, staggered per
/// channel by a phase offset (see [`ModDelayConfig::channel_phase_offset`] and
/// [`with_phase_offsets`](Self::with_phase_offsets)). At width 2 with the
/// [`CHORUS`](ModDelayConfig::CHORUS) or [`FLANGER`](ModDelayConfig::FLANGER)
/// preset it renders exactly what the old stereo `ChorusNode` / `FlangerNode`
/// did.
///
/// Rate ([`Hz`]), depth ([`Seconds`] of delay sweep), [`Feedback`] and [`Mix`]
/// are live [`Param`](tutti_core::Param)s shared across clones, all read **once
/// per block**. The LFO's phases for the block are computed once, into a block
/// buffer every channel reads; depth, feedback and mix ramp across the block
/// when they moved (a depth step would jump the delay time — a click). A
/// block of one takes a change whole.
///
/// **Depth is denominated in seconds of delay, not a fraction**: it is a
/// duration added to the base delay, unlike the phaser's unitless depth.
///
/// # In a graph
///
/// A graph node ([`IntoNode`]), `N` in and `N` out, with zero latency: the
/// modulated delay is the effect's sound, not processing latency, and PDC
/// would otherwise delay every other path by the base delay (design doc 013,
/// D1). Inserted, its controls are a [`ParamSet`] over rate
/// ([`UnitParam::Rate`]), depth ([`UnitParam::Depth`]), feedback
/// ([`UnitParam::Feedback`]) and mix ([`UnitParam::Wet`]); a fork starts from
/// the values last set through it. The graph prepares it at the device rate,
/// which sizes the lines, before its first block. Its tail is
/// [`Tail::Unknown`] (a recirculating line), so it is never skipped.
pub struct ModDelayNode {
    config: ModDelayConfig,
    /// One line per channel; `len()` is the audio width. Built at construction
    /// and on `prepare` — never in `process`.
    delays: Vec<DelayLine>,
    /// Per-channel LFO phase offset; same length as `delays`.
    phase_offsets: Vec<PhaseIncrement>,
    lfo: LfoDrive,
    controls: TimeModMix,
    sample_rate: SampleRate,
    /// `None` until the first block (and after `reset`): the first block starts
    /// on its targets instead of ramping in from nothing.
    last: Option<ModDelayControls>,
    /// The block's LFO phases: scratch, sized at `prepare` for the graph's
    /// maximum block.
    phases: Vec<Phase>,
}

impl ModDelayNode {
    /// A modulated delay `channels` wide (clamped to at least 1), configured by
    /// `config` and starting at its defaults. Channel `c` sweeps
    /// `c × config.channel_phase_offset` ahead of channel 0.
    ///
    /// Allocates its delay lines, so build before the node goes live. Both
    /// rate-dependent quantities — the lines, *allocated* in samples from
    /// `max_delay`, and the LFO's phase increment — take the rate
    /// [`Node::prepare`] hands it, which rebuilds the lines.
    pub fn new(channels: impl Into<ChannelLayout>, config: ModDelayConfig) -> Self {
        let n = usize::from(channels.into().count()).max(1);
        Self {
            delays: (0..n)
                .map(|_| DelayLine::from_seconds(config.max_delay, SampleRate::DEFAULT))
                .collect(),
            phase_offsets: (0..n)
                .map(|c| {
                    PhaseIncrement(
                        Phase::START
                            .advance(PhaseIncrement(config.channel_phase_offset.get() * c as f32))
                            .get(),
                    )
                })
                .collect(),
            lfo: LfoDrive::new(config.rate),
            controls: TimeModMix::new(config.depth, config.feedback, config.mix),
            sample_rate: SampleRate::DEFAULT,
            last: None,
            phases: Vec::new(),
            config,
        }
    }

    /// A chorus `channels` wide — [`ModDelayConfig::CHORUS`].
    pub fn chorus(channels: impl Into<ChannelLayout>) -> Self {
        Self::new(channels, ModDelayConfig::CHORUS)
    }

    /// A flanger `channels` wide — [`ModDelayConfig::FLANGER`].
    pub fn flanger(channels: impl Into<ChannelLayout>) -> Self {
        Self::new(channels, ModDelayConfig::FLANGER)
    }

    /// Replaces every channel's LFO phase offset, in cycles. Offsets are wrapped
    /// into one cycle.
    ///
    /// # Panics
    ///
    /// If `offsets.len()` is not the node's width — a build-time shape error.
    pub fn with_phase_offsets(mut self, offsets: &[PhaseIncrement]) -> Self {
        assert_eq!(
            offsets.len(),
            self.delays.len(),
            "one phase offset per channel"
        );
        for (o, &new) in self.phase_offsets.iter_mut().zip(offsets) {
            *o = PhaseIncrement(Phase::START.advance(new).get());
        }
        self
    }

    /// The configuration this node was built from.
    pub fn config(&self) -> ModDelayConfig {
        self.config
    }

    /// The shared LFO rate cell in [`Hz`] — how fast the delay sweeps.
    ///
    /// Chorus and flanger rates are slow, typically 0.1–2 Hz; faster reads as
    /// vibrato. Read once per block. Shared across clones.
    pub fn rate(&self) -> Arc<AtomicF32> {
        self.lfo.rate.as_atomic()
    }

    /// The shared sweep-depth cell in [`Seconds`] — how far the delay time
    /// moves either side of the base delay. Read once per block and ramped.
    pub fn depth(&self) -> Arc<AtomicF32> {
        self.controls.depth.as_atomic()
    }

    /// The shared [`Feedback`] cell — how much of the delayed signal
    /// recirculates. On a flanger it is the most characterful control: higher
    /// feedback sharpens the comb notches into a resonant sweep.
    ///
    /// Writing the raw cell bypasses [`set_feedback`](Self::set_feedback)'s
    /// stability clamp.
    pub fn feedback(&self) -> Arc<AtomicF32> {
        self.controls.feedback.as_atomic()
    }

    /// The shared wet/dry [`Mix`] cell: `0.0` dry, `1.0` fully wet.
    ///
    /// Both effects need both halves — at fully wet a chorus's detuned copy
    /// stands alone and a flanger's notches (wet and dry interfering) vanish.
    pub fn mix(&self) -> Arc<AtomicF32> {
        self.controls.mix.as_atomic()
    }

    /// Sets the LFO rate in [`Hz`], floored at 0.01 Hz.
    pub fn set_rate(&self, hz: impl Into<Hz>) {
        self.lfo.rate.store(Hz(hz.into().get().max(0.01)));
    }

    /// Sets the sweep depth in [`Seconds`], clamped to the configuration's
    /// `depth_min..=depth_max`.
    pub fn set_depth(&self, secs: impl Into<Seconds>) {
        let (lo, hi) = (self.config.depth_min.get(), self.config.depth_max.get());
        self.controls
            .depth
            .store(Seconds(secs.into().get().clamp(lo, hi)));
    }

    /// Sets the [`Feedback`], clamped to the stable range.
    pub fn set_feedback(&self, fb: impl Into<Feedback>) {
        self.controls
            .feedback
            .store(Feedback::new_clamped(fb.into().get()));
    }

    /// Sets the wet/dry [`Mix`], clamped to `0.0..=1.0`.
    pub fn set_mix(&self, mix: impl Into<Mix>) {
        self.controls.mix.store(Mix::new_clamped(mix.into().get()));
    }

    /// The render kernel behind `process`, over `size` frames. `size` is at
    /// most the phase scratch's length.
    fn render(
        &mut self,
        size: usize,
        x: impl Fn(usize, usize) -> f32,
        y: impl FnMut(usize, usize, f32),
    ) {
        debug_assert!(size <= self.phases.len());
        // Every control is read here, once, for the whole block.
        let (depth, fb, mix) = self.controls.load();
        let target = ModDelayControls {
            depth,
            fb,
            mix: mix.get(),
        };
        let from = self.last.unwrap_or(target);
        let depth_r = Ramp::new(from.depth, target.depth, size);
        let fb_r = Ramp::new(from.fb, target.fb, size);
        let mix_r = Ramp::new(from.mix, target.mix, size);

        // The LFO as a block buffer: the rate is read once and every channel
        // reads the same phases.
        self.lfo
            .fill_block(self.sample_rate, &mut self.phases[..size]);

        let lines = SweptLines {
            delays: &mut self.delays,
            offsets: &self.phase_offsets,
            phases: &self.phases[..size],
            // Narrowed once: the delay positions feed an interpolated read, so
            // they keep their fraction rather than going through
            // `Seconds::to_samples`.
            sr: self.sample_rate.get() as f32,
            base_delay: self.config.base_delay.get(),
        };
        if depth_r.is_flat() && fb_r.is_flat() && mix_r.is_flat() {
            // Nothing moved since the last block: constants in the loop.
            let (d, f, m) = (target.depth, target.fb, target.mix);
            lines.run(x, y, |_| d, |_| f, |_| m);
        } else {
            lines.run(x, y, |i| depth_r.at(i), |i| fb_r.at(i), |i| mix_r.at(i));
        }
        self.last = Some(target);
    }
}

/// The delay lines and the block's LFO phases, borrowed apart from the node so
/// the per-block control closures can be chosen outside the loop.
struct SweptLines<'a> {
    delays: &'a mut [DelayLine],
    offsets: &'a [PhaseIncrement],
    phases: &'a [Phase],
    sr: f32,
    base_delay: f32,
}

impl SweptLines<'_> {
    /// Channel-outer over the lines. `depth_at` / `fb_at` / `mix_at` give the
    /// controls at sample `i`; constant closures compile to constants.
    #[inline(always)]
    fn run(
        self,
        x: impl Fn(usize, usize) -> f32,
        mut y: impl FnMut(usize, usize, f32),
        depth_at: impl Fn(usize) -> f32,
        fb_at: impl Fn(usize) -> f32,
        mix_at: impl Fn(usize) -> f32,
    ) {
        let sr = self.sr;
        let base_delay = self.base_delay * sr;
        for (c, (line, &offset)) in self.delays.iter_mut().zip(self.offsets).enumerate() {
            for (i, &phase) in self.phases.iter().enumerate() {
                // Channel 0's offset is zero, and the old left channel read the
                // phase directly; `offset_by(0)` is the same value.
                let lfo = phase.offset_by(offset).to_radians().get().sin();
                let delay = (base_delay + lfo * depth_at(i) * sr).max(1.0);
                let input = x(c, i);
                let fb_tap = line.read_sample(delay, InterpolationMode::Linear);
                line.push_sample(input + fb_tap * fb_at(i));
                let wet = line.read_sample(delay, InterpolationMode::Linear);
                y(c, i, Mix(mix_at(i)).blend(input, wet));
            }
        }
    }
}

impl Node for ModDelayNode {
    fn shape(&self) -> Shape {
        let width = ChannelLayout::from_count(self.delays.len() as u16);
        Shape::audio(width, width).with_tail(Tail::Unknown)
    }

    fn prepare(&mut self, p: &Prepare) {
        self.sample_rate = p.sample_rate();
        for d in &mut self.delays {
            *d = DelayLine::from_seconds(self.config.max_delay, self.sample_rate);
        }
        self.phases = vec![Phase::START; p.max_block().get()];
    }

    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let size = io.frames();
        let (inputs, mut outputs) = io.split();
        // A graph never hands a block past its `MaxBlock`, which the scratch
        // is sized for, so this is one call; a node driven by hand with a
        // longer block renders it in scratch-sized pieces.
        let step = self.phases.len().max(1);
        let mut at = 0;
        while at < size {
            let n = step.min(size - at);
            self.render(
                n,
                |c, i| inputs.get(c)[at + i],
                |c, i, v| outputs.get(c)[at + i] = v,
            );
            at += n;
        }
        Status::Modified
    }

    fn reset(&mut self) {
        for d in &mut self.delays {
            d.reset();
        }
        self.lfo.reset_phase();
        self.last = None;
    }
}

impl ParamNode for ModDelayNode {
    fn param_set(&self) -> ParamSet {
        ParamSet::builder()
            .param(UnitParam::Rate, self.lfo.rate.as_atomic())
            .param(UnitParam::Depth, self.controls.depth.as_atomic())
            .param(UnitParam::Feedback, self.controls.feedback.as_atomic())
            .param(UnitParam::Wet, self.controls.mix.as_atomic())
            .build()
    }

    /// A clone with every control cell detached (at its value now), its
    /// lines cleared and its LFO back at the start.
    fn fork_fresh(&self) -> Self {
        let mut fork = self.clone();
        fork.lfo.detach();
        fork.controls.detach();
        Node::reset(&mut fork);
        fork
    }
}

/// Inserted with its [`ParamSet`] as its controls and a fork that starts
/// from the values last set through it ([`tutti_graph::param_parts`]).
impl IntoNode for ModDelayNode {
    type Controls = ParamSet;

    fn into_parts(self) -> NodeParts<ParamSet> {
        tutti_graph::param_parts(self)
    }
}

impl Clone for ModDelayNode {
    fn clone(&self) -> Self {
        Self {
            config: self.config,
            delays: self.delays.clone(),
            phase_offsets: self.phase_offsets.clone(),
            lfo: self.lfo.clone(),
            controls: self.controls.clone(),
            sample_rate: self.sample_rate,
            last: self.last,
            phases: self.phases.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        change_between_node_blocks, drive_frames, noise, prepared as at_48k,
    };
    use tutti_graph::contract::assert_param_fork;

    /// A fork starts from the values last set through the node's
    /// `ParamSet` and shares no cell with it (see
    /// `tutti_graph::contract::assert_param_fork`).
    ///
    /// Mutation (run): drop `fork.lfo.detach()` in `fork_fresh` → "a live
    /// write reached the fork" for `Rate`. Leave `Depth` out of `param_set`
    /// → the address list below fails.
    #[test]
    fn a_fork_starts_from_the_authored_values_and_shares_nothing() {
        let node = ModDelayNode::flanger(ChannelLayout::STEREO);
        assert_eq!(
            node.param_set().params().collect::<Vec<_>>(),
            [
                UnitParam::Rate,
                UnitParam::Depth,
                UnitParam::Feedback,
                UnitParam::Wet
            ]
        );
        assert_param_fork(node);
    }

    /// A block longer than the prepared maximum (only a node driven by hand
    /// gets one) renders as that block cut at the maximum: the phase scratch
    /// is never overrun.
    ///
    /// Mutation (run): render the whole block in one `render` call → the
    /// scratch slice `phases[..size]` panics out of range.
    #[test]
    fn a_block_past_the_prepared_maximum_renders_in_pieces() {
        let x = noise(3, 300);
        let mut long = tutti_graph::contract::prepared(
            ModDelayNode::chorus(ChannelLayout::STEREO),
            SampleRate(48_000.0),
            128,
        );
        let got = tutti_graph::contract::drive(&mut long, SampleRate(48_000.0), &[&x, &x], &[]);
        let mut cut = tutti_graph::contract::prepared(
            ModDelayNode::chorus(ChannelLayout::STEREO),
            SampleRate(48_000.0),
            128,
        );
        let mut want = vec![Vec::new(), Vec::new()];
        for piece in [0..128, 128..256, 256..300] {
            let x = &x[piece];
            let o = tutti_graph::contract::drive(&mut cut, SampleRate(48_000.0), &[x, x], &[]);
            for (w, o) in want.iter_mut().zip(o) {
                w.extend(o);
            }
        }
        assert_eq!(got, want);
    }

    #[test]
    fn test_chorus_passthrough_dry() {
        let mut chorus = at_48k(ModDelayNode::chorus(ChannelLayout::STEREO));
        chorus.set_mix(0.0);
        let out = drive_frames(&mut chorus, &[&[0.5], &[-0.3]]);
        assert!((out[0][0] - 0.5).abs() < 0.001);
        assert!((out[1][0] - (-0.3)).abs() < 0.001);
    }

    #[test]
    fn test_chorus_stereo_difference() {
        let mut chorus = at_48k(ModDelayNode::chorus(ChannelLayout::STEREO));
        chorus.set_mix(1.0);
        let ones = [1.0f32; 4410];
        let out = drive_frames(&mut chorus, &[&ones, &ones]);
        let l_sum: f64 = out[0].iter().map(|&s| s as f64).sum();
        let r_sum: f64 = out[1].iter().map(|&s| s as f64).sum();
        assert!(
            (l_sum - r_sum).abs() > 0.01,
            "Stereo channels should differ"
        );
    }

    #[test]
    fn test_flanger_feedback_effect() {
        let mut flanger = at_48k(ModDelayNode::flanger(ChannelLayout::STEREO));
        flanger.set_feedback(0.9);
        flanger.set_mix(1.0);
        let mut x = [0.0f32; 501];
        x[0] = 1.0;
        let out = drive_frames(&mut flanger, &[&x, &x]);
        let max_output = out[0][1..].iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(
            max_output > 0.01,
            "High feedback should sustain signal: {max_output}"
        );
    }

    #[test]
    fn test_mod_delay_reset() {
        for mut node in [
            at_48k(ModDelayNode::chorus(ChannelLayout::STEREO)),
            at_48k(ModDelayNode::flanger(ChannelLayout::STEREO)),
        ] {
            let ones = [1.0f32; 100];
            drive_frames(&mut node, &[&ones, &ones]);
            Node::reset(&mut node);
            let out = drive_frames(&mut node, &[&[0.0], &[0.0]]);
            assert!(
                out[0][0].abs() < 0.01,
                "After reset, output should be near zero"
            );
        }
    }

    /// The two presets keep their own depth ranges.
    #[test]
    fn each_preset_clamps_depth_to_its_own_range() {
        let chorus = ModDelayNode::chorus(ChannelLayout::STEREO);
        chorus.set_depth(1.0);
        assert_eq!(chorus.controls.depth.load(), Seconds(0.04));
        let flanger = ModDelayNode::flanger(ChannelLayout::STEREO);
        flanger.set_depth(0.0);
        assert_eq!(flanger.controls.depth.load(), Seconds(0.0001));
    }

    /// A mix change made between blocks fades across the next block.
    ///
    /// Mutation: `Ramp::new(target.mix, target.mix, size)` (a jump) fails.
    #[test]
    fn a_mix_change_fades_across_the_next_block() {
        let x = noise(8, 128);
        let run = change_between_node_blocks(
            || ModDelayNode::chorus(ChannelLayout::STEREO),
            |n| n.set_mix(0.0),
            &[&x[..64], &x[..64]],
            &[&x[64..], &x[64..]],
        );
        run.assert_ramps_in("chorus mix");
        assert_eq!(run.r[1][63], x[127], "the block ends fully dry");
    }

    /// Six channels: the first two are exactly the stereo chorus, channel 4's
    /// stagger (4 × a quarter cycle) wraps back onto channel 0's, and a silent
    /// channel stays silent — no channel reads another's line.
    ///
    /// Mutation: dropping the `c ×` from the per-channel offset (every channel
    /// on channel 0's phase) fails the channel-1 check.
    #[test]
    fn six_channels_extend_the_stereo_chorus() {
        // Longer than the 10 ms base delay, so the wet half is sounding.
        let x = noise(11, 2_048);
        let silent = [0.0f32; 2_048];
        let mut wide = at_48k(ModDelayNode::chorus(6usize));
        let ins: [&[f32]; 6] = [&x, &x, &x, &silent, &x, &x];
        let wide_out = drive_frames(&mut wide, &ins);
        let mut stereo = at_48k(ModDelayNode::chorus(ChannelLayout::STEREO));
        let stereo_out = drive_frames(&mut stereo, &[&x, &x]);
        assert_eq!(wide_out[0], stereo_out[0], "channel 0 is the stereo left");
        assert_eq!(wide_out[1], stereo_out[1], "channel 1 is the stereo right");
        assert_eq!(
            wide_out[4], wide_out[0],
            "a full-cycle stagger is no stagger"
        );
        assert!(
            wide_out[3].iter().all(|&s| s == 0.0),
            "no bleed into a silent channel"
        );
        assert_ne!(wide_out[2], wide_out[0], "a half-cycle stagger differs");
    }

    /// Explicit offsets replace the preset's stagger: all-zero puts every
    /// channel on one phase, so identical inputs render identically.
    ///
    /// Mutation: ignoring `with_phase_offsets` (keeping the preset stagger)
    /// fails.
    #[test]
    fn explicit_phase_offsets_replace_the_stagger() {
        // Longer than the 10 ms base delay, so the wet half is sounding.
        let x = noise(12, 2_048);
        let mut node = at_48k(
            ModDelayNode::chorus(ChannelLayout::STEREO)
                .with_phase_offsets(&[PhaseIncrement(0.0), PhaseIncrement(0.0)]),
        );
        let out = drive_frames(&mut node, &[&x, &x]);
        assert_eq!(out[0], out[1]);
    }
}
