//! Chorus and flanger — one width-generic modulated delay, two presets.
//!
//! The two effects were structurally identical (a delay line per channel swept
//! by an LFO, with feedback and a wet/dry mix) and differed only in numbers: a
//! chorus sits at a long base delay (10 ms) so the copy reads as a second voice
//! thickening the sound, a flanger at a short one (1 ms) so the copy combs
//! against the original and the sweep moves the notches. They were two node
//! types wrapping one stereo core; they are now one [`ModDelayNode`] and a
//! [`ModDelayConfig`], at any width.

use tutti_core::{Arc, AtomicF32, MAX_BUFFER_SIZE};
use tutti_core::{
    AudioUnit, BufferMut, BufferRef, ChannelLayout, Feedback, Hz, Mix, Phase, PhaseIncrement,
    SampleRate, Seconds, SignalFrame,
};

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
/// when they moved (a depth step would jump the delay time — a click). `tick`
/// is a block of one.
///
/// **Depth is denominated in seconds of delay, not a fraction**: it is a
/// duration added to the base delay, unlike the phaser's unitless depth.
pub struct ModDelayNode {
    config: ModDelayConfig,
    /// One line per channel; `len()` is the audio width. Built at construction
    /// and on `set_sample_rate` — never in `tick`/`process`.
    delays: Vec<DelayLine>,
    /// Per-channel LFO phase offset; same length as `delays`.
    phase_offsets: Vec<PhaseIncrement>,
    lfo: LfoDrive,
    controls: TimeModMix,
    sample_rate: SampleRate,
    /// `None` until the first block (and after `reset`): the first block starts
    /// on its targets instead of ramping in from nothing.
    last: Option<ModDelayControls>,
}

impl ModDelayNode {
    /// A modulated delay `channels` wide (clamped to at least 1), configured by
    /// `config` and starting at its defaults. Channel `c` sweeps
    /// `c × config.channel_phase_offset` ahead of channel 0.
    ///
    /// Allocates its delay lines, so build before the node goes live.
    ///
    /// **Starts at the placeholder [`SampleRate::DEFAULT`]**, and both
    /// rate-dependent quantities skew together if
    /// [`AudioUnit::set_sample_rate`] is not called before the first `process`:
    /// the lines are *allocated* in samples from `max_delay`, and the LFO's
    /// phase increment is its rate divided by the sample rate. At 48 kHz an
    /// uncorrected node sweeps 8.8% too little delay 8.8% too slowly — a chorus
    /// that is simply shallower and lazier than configured, a flanger whose
    /// notches all sit higher. Nothing reports it. `set_sample_rate` rebuilds
    /// the lines and so reallocates. See the crate-level "born at a placeholder
    /// rate" section.
    ///
    /// [`SampleRate::DEFAULT`]: tutti_core::SampleRate::DEFAULT
    /// [`AudioUnit::set_sample_rate`]: tutti_core::AudioUnit::set_sample_rate
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

    /// The one render kernel behind `tick` and `process`. `size` is at most
    /// [`MAX_BUFFER_SIZE`], which `AudioUnit::process` guarantees.
    fn render(
        &mut self,
        size: usize,
        x: impl Fn(usize, usize) -> f32,
        y: impl FnMut(usize, usize, f32),
    ) {
        debug_assert!(size <= MAX_BUFFER_SIZE);
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
        let mut phases = [Phase::START; MAX_BUFFER_SIZE];
        self.lfo.fill_block(self.sample_rate, &mut phases[..size]);

        let lines = SweptLines {
            delays: &mut self.delays,
            offsets: &self.phase_offsets,
            phases: &phases[..size],
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

impl AudioUnit for ModDelayNode {
    fn inputs(&self) -> usize {
        self.delays.len()
    }

    fn outputs(&self) -> usize {
        self.delays.len()
    }

    /// Detach every control cell this node reads (see `Param::detach`), so
    /// a fork renders the controls as they were when it was taken, not the
    /// live knob moves made while it runs. Values are kept.
    fn isolate(&mut self) {
        self.lfo.detach();
        self.controls.detach();
    }

    fn reset(&mut self) {
        for d in &mut self.delays {
            d.reset();
        }
        self.lfo.reset_phase();
        self.last = None;
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        self.sample_rate = sample_rate;
        for d in &mut self.delays {
            *d = DelayLine::from_seconds(self.config.max_delay, sample_rate);
        }
    }

    #[inline]
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        self.render(1, |c, _| input[c], |c, _, v| output[c] = v);
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        if size == 0 {
            return;
        }
        self.render(
            size,
            |c, i| input.at_f32(c, i),
            |c, i, v| output.set_f32(c, i, v),
        );
    }

    fn set(&mut self, setting: tutti_core::Setting) {
        if let Some((param, value)) = tutti_core::unit_param::from_setting(&setting) {
            match param {
                tutti_core::UnitParam::Rate => self.set_rate(value),
                tutti_core::UnitParam::Depth => self.set_depth(value),
                tutti_core::UnitParam::Feedback => self.set_feedback(value),
                tutti_core::UnitParam::Wet => self.set_mix(value),
                _ => {}
            }
        }
    }

    fn get_id(&self) -> u64 {
        // The flanger preset keeps the flanger's id; everything else the
        // chorus's. Only the render hash reads it.
        if self.config == ModDelayConfig::FLANGER {
            crate::node_id::FLANGER_ID
        } else {
            crate::node_id::CHORUS_ID
        }
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    /// Zero latency: the modulated delay is the effect's sound, not processing
    /// latency, and PDC would otherwise delay every other path by the base
    /// delay (design doc 013, D1; the full argument is on
    /// [`DelayLineNode`](crate::DelayLineNode)'s `route`). `distort`, because a
    /// swept delay has no fixed frequency response.
    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let mut out = SignalFrame::new(self.delays.len());
        for c in 0..self.delays.len() {
            out.set(c, input.at(c).distort(0.0));
        }
        out
    }

    fn footprint(&self) -> usize {
        core::mem::size_of::<Self>()
            + self
                .delays
                .iter()
                .map(|d| d.buffer.len() * core::mem::size_of::<f32>())
                .sum::<usize>()
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{change_between_blocks, noise, tick_block};

    const SR: SampleRate = SampleRate(48_000.0);

    fn at_48k(mut n: ModDelayNode) -> ModDelayNode {
        n.set_sample_rate(SR);
        n
    }

    #[test]
    fn test_chorus_passthrough_dry() {
        let chorus = at_48k(ModDelayNode::chorus(ChannelLayout::STEREO));
        chorus.set_mix(0.0);
        let mut chorus = chorus;
        let mut out = [0.0f32; 2];
        chorus.tick(&[0.5, -0.3], &mut out);
        assert!((out[0] - 0.5).abs() < 0.001);
        assert!((out[1] - (-0.3)).abs() < 0.001);
    }

    #[test]
    fn test_chorus_stereo_difference() {
        let mut chorus = at_48k(ModDelayNode::chorus(ChannelLayout::STEREO));
        chorus.set_mix(1.0);
        let mut out = [0.0f32; 2];
        let (mut l_sum, mut r_sum) = (0.0f64, 0.0f64);
        for _ in 0..4410 {
            chorus.tick(&[1.0, 1.0], &mut out);
            l_sum += out[0] as f64;
            r_sum += out[1] as f64;
        }
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
        let mut out = [0.0f32; 2];
        flanger.tick(&[1.0, 1.0], &mut out);
        let mut max_output = 0.0f32;
        for _ in 0..500 {
            flanger.tick(&[0.0, 0.0], &mut out);
            max_output = max_output.max(out[0].abs());
        }
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
            let mut out = [0.0f32; 2];
            for _ in 0..100 {
                node.tick(&[1.0, 1.0], &mut out);
            }
            node.reset();
            node.tick(&[0.0, 0.0], &mut out);
            assert!(
                out[0].abs() < 0.01,
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
        let run = change_between_blocks(
            || at_48k(ModDelayNode::chorus(ChannelLayout::STEREO)),
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
        let wide_out = tick_block(&mut wide, &ins);
        let mut stereo = at_48k(ModDelayNode::chorus(ChannelLayout::STEREO));
        let stereo_out = tick_block(&mut stereo, &[&x, &x]);
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
        let out = tick_block(&mut node, &[&x, &x]);
        assert_eq!(out[0], out[1]);
    }
}
