//! Chorus and flanger — one width-generic modulated delay, two presets.
//!
//! The two effects were structurally identical (a delay line per channel swept
//! by an LFO, with feedback and a wet/dry mix) and differed only in numbers: a
//! chorus sits at a long base delay (10 ms) so the copy reads as a second voice
//! thickening the sound, a flanger at a short one (1 ms) so the copy combs
//! against the original and the sweep moves the notches. They were two node
//! types wrapping one stereo core; they are now one [`ModDelayNode`] and a
//! [`ModDelayConfig`], at any width.

use std::sync::Arc;
use tutti_graph::{Cx, IntoNode, Io, Node, NodeParts, ParamNode, ParamSet, Prepare, Shape, Status};
use tutti_types::AtomicF32;
use tutti_types::{ChannelLayout, Feedback, Hz, Mix, Phase, PhaseIncrement, SampleRate, Seconds};
use tutti_types::{Tail, UnitParam};

use super::shared::{LfoDrive, TimeModMix};
use crate::ramp::block_driven;
use tutti_dsp::{COEFF_INTERVAL, Ramp};
use tutti_dsp::{DelayLine, SweptLines};

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
    /// channel 0, wrapped. At width 2 this is the L/R offset.
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
    /// The most feedback the node applies, whatever is set through its param
    /// or driven on its port: a value above it reads as it. [`Feedback::MAX_STABLE`]
    /// is the most the unit allows. [`Feedback::NONE`] makes the line a pure
    /// delay, with no recirculation, and so with a tail of exactly
    /// [`max_delay`](Self::max_delay) (see [`ModDelayNode`]'s "Tail").
    pub feedback_max: Feedback,
}

impl ModDelayConfig {
    /// A sweep depth in seconds as [`ModDelayNode::set_depth`] stores it:
    /// clamped to `depth_min..=depth_max`. Shared by the setter and the
    /// block's read, so a driven depth (or a raw `ParamSet` write) cannot
    /// sweep past the line.
    #[inline]
    ///
    /// `max` then `min` rather than `f32::clamp`, which panics on a config
    /// whose `depth_min` exceeds its `depth_max`; this is read on the audio
    /// thread. For a well-formed config the two agree.
    pub fn clamp_depth(&self, secs: f32) -> f32 {
        secs.max(self.depth_min.get()).min(self.depth_max.get())
    }

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
        feedback_max: Feedback::MAX_STABLE,
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
        feedback_max: Feedback::MAX_STABLE,
    };

    /// The chorus of a General MIDI synthesizer's chorus send, as SoundFont
    /// players have it: wet only (a send effect, summed with the dry signal
    /// elsewhere), a 2 ms centre delay swept 1.9 ms either side by a sine at
    /// 0.4 Hz, the right channel a quarter cycle ahead of the left, and no
    /// feedback at all.
    ///
    /// The dry signal is not in the output (`mix` 1), and the node can recirculate
    /// nothing (`feedback_max` 0), which is what lets it declare a finite tail:
    /// what it outputs was in its input at most `max_delay` ago.
    pub const GENERAL_MIDI_CHORUS: Self = Self {
        base_delay: Seconds(0.002),
        max_delay: Seconds(0.005),
        channel_phase_offset: PhaseIncrement(0.25),
        depth_min: Seconds(0.0),
        depth_max: Seconds(0.0019),
        rate: Hz(0.4),
        depth: Seconds(0.0019),
        feedback: Feedback::NONE,
        mix: Mix(1.0),
        feedback_max: Feedback::NONE,
    };
}

/// The block's depth / feedback / mix — where the next block's ramps start.
#[derive(Clone, Copy, Debug)]
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
/// preset it is a stereo chorus or flanger.
///
/// Rate ([`Hz`]), depth ([`Seconds`] of delay sweep), [`Feedback`] and [`Mix`]
/// are live [`Param`](tutti_types::Param)s shared across clones, all read **once
/// per block**. The LFO's phases for the block are computed once, into a block
/// buffer every channel reads, and its sine is evaluated only every 16
/// samples (on a grid counted across blocks, so the output
/// does not depend on how the stream is cut into blocks) and interpolated
/// between; depth, feedback and mix ramp across the block
/// when they moved (a depth step would jump the delay time — a click). A
/// block of one takes a change whole.
///
/// **Depth is denominated in seconds of delay, not a fraction**: it is a
/// duration added to the base delay, unlike the phaser's unitless depth.
///
/// # In a graph
///
/// A graph node ([`IntoNode`]), `N` in and `N` out, with zero latency: the
/// modulated delay is the effect's sound, not processing latency, and PDC would
/// otherwise delay every other path by the base delay. Inserted, its controls
/// are a [`ParamSet`] over rate ([`UnitParam::Rate`]), depth
/// ([`UnitParam::Depth`]), feedback ([`UnitParam::Feedback`]) and mix
/// ([`UnitParam::Wet`]); a fork starts from the values last set through it. The
/// graph prepares it at the device rate, which sizes the lines, before its
/// first block.
///
/// # Tail
///
/// [`Tail::Unknown`] (a recirculating line), so it is never skipped, unless the
/// config's [`feedback_max`](ModDelayConfig::feedback_max) is [`Feedback::NONE`]:
/// then nothing recirculates, an output frame is made of input at most
/// [`max_delay`](ModDelayConfig::max_delay) old, and the tail is exactly that.
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
    /// Per channel, the LFO's sine at the two control points around the
    /// current position (see [`SweptLines::run`]).
    points: Vec<[f32; 2]>,
    /// Samples since the last control point, in `0..COEFF_INTERVAL`. Counted
    /// across blocks, so where the points fall does not depend on block size.
    seg_pos: usize,
    /// Whether `points` hold the sines of the current segment: false until the
    /// first block and after `reset` or `prepare`.
    points_valid: bool,
    /// The step the last block ran at, which stepped on the pending control
    /// point; `None` before the first block and after `reset` or `prepare`.
    last_inc: Option<PhaseIncrement>,
}

impl std::fmt::Debug for ModDelayNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModDelayNode")
            .field("config", &self.config)
            .field("delays", &self.delays)
            .field("phase_offsets", &self.phase_offsets)
            .field("lfo", &self.lfo)
            .field("controls", &self.controls)
            .field("sample_rate", &self.sample_rate)
            .field("last", &self.last)
            .field("phases", &self.phases)
            .field("seg_pos", &self.seg_pos)
            .field("points_valid", &self.points_valid)
            .field("last_inc", &self.last_inc)
            .finish_non_exhaustive()
    }
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
            points: vec![[0.0; 2]; n],
            seg_pos: 0,
            points_valid: false,
            last_inc: None,
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
    #[must_use]
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

    /// The tail the node declares (see "Tail" on [`ModDelayNode`]).
    fn tail(&self) -> Tail {
        if self.config.feedback_max == Feedback::NONE {
            Tail::Finite(self.config.max_delay.to_samples_ceil(self.sample_rate))
        } else {
            Tail::Unknown
        }
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
        self.lfo
            .rate
            .store(Hz(LfoDrive::clamp_rate(hz.into().get())));
    }

    /// Sets the sweep depth in [`Seconds`], clamped to the configuration's
    /// `depth_min..=depth_max`.
    pub fn set_depth(&self, secs: impl Into<Seconds>) {
        self.controls
            .depth
            .store(Seconds(self.config.clamp_depth(secs.into().get())));
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
    ///
    /// `driven` is the block's rate, depth, feedback and mix from the
    /// node's param ports ([`MOD_DELAY_PARAMS`] order), each `None` where
    /// nothing drives it.
    fn render(
        &mut self,
        size: usize,
        driven: [Option<f32>; 4],
        x: impl Fn(usize, usize) -> f32,
        y: impl FnMut(usize, usize, f32),
    ) {
        debug_assert!(size <= self.phases.len());
        // Every control is read here, once, for the whole block.
        let [rate, depth, fb, mix] = driven;
        let (depth, fb, mix) = self.controls.load([depth, fb, mix]);
        let target = ModDelayControls {
            depth: self.config.clamp_depth(depth),
            fb: fb.min(self.config.feedback_max.get()),
            mix: mix.get(),
        };
        let from = self.last.unwrap_or(target);
        let depth_r = Ramp::new(from.depth, target.depth, size);
        let fb_r = Ramp::new(from.fb, target.fb, size);
        let mix_r = Ramp::new(from.mix, target.mix, size);

        // The LFO as a block buffer: the rate is read once and every channel
        // reads the same phases.
        let inc = self
            .lfo
            .fill_block(self.sample_rate, &mut self.phases[..size], rate);

        let lines = SweptLines {
            points: &mut self.points,
            seg_pos: self.seg_pos,
            primed: self.points_valid,
            // A rate change since the last block: the pending control point
            // was stepped on at the old step.
            restep: self.points_valid && self.last_inc.is_some_and(|last| last != inc),
            after: self.lfo.phase,
            inc,
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
        self.seg_pos = (self.seg_pos + size) % COEFF_INTERVAL;
        self.points_valid = true;
        self.last_inc = Some(inc);
        self.last = Some(target);
    }
}

/// The params a [`ModDelayNode`] lets the graph modulate, in port order:
/// all four of its controls. Each is block-rate in the node (read once per
/// block and ramped across it), so a driven port is read the same way
/// (`block_driven`).
pub const MOD_DELAY_PARAMS: [UnitParam; 4] = [
    UnitParam::Rate,
    UnitParam::Depth,
    UnitParam::Feedback,
    UnitParam::Wet,
];

impl Node for ModDelayNode {
    fn shape(&self) -> Shape {
        let width =
            ChannelLayout::try_from(self.delays.len()).expect("the width came from a layout");
        Shape::audio(width, width)
            .with_cost(tutti_graph::Cost::Moderate)
            .with_tail(self.tail())
            .with_params(&MOD_DELAY_PARAMS)
    }

    fn prepare(&mut self, p: &Prepare) {
        self.sample_rate = p.sample_rate();
        for d in &mut self.delays {
            *d = DelayLine::from_seconds(self.config.max_delay, self.sample_rate);
        }
        self.phases = vec![Phase::START; p.max_block().get()];
        self.seg_pos = 0;
        self.points_valid = false;
        self.last_inc = None;
    }

    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let size = io.frames();
        let driven = [0, 1, 2, 3].map(|k| block_driven(&io, k));
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
                driven,
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
        self.seg_pos = 0;
        self.points_valid = false;
        self.last_inc = None;
        self.last = None;
    }

    fn param_base(&self, k: usize) -> Option<f32> {
        match k {
            0 => Some(self.lfo.rate.load().get()),
            1 => Some(self.controls.depth.load().get()),
            2 => Some(self.controls.feedback.load().get()),
            3 => Some(self.controls.mix.load().get()),
            _ => None,
        }
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
            points: self.points.clone(),
            seg_pos: self.seg_pos,
            points_valid: self.points_valid,
            last_inc: self.last_inc,
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

    /// A mono, fully wet, feedback-free sweep: base 20 ms, 10 ms deep, 20 Hz.
    /// The fastest rate a chorus is run at is the worst case for the
    /// interpolation between control points.
    fn trajectory_node(depth: f32) -> ModDelayNode {
        let node = ModDelayNode::new(
            1u16,
            ModDelayConfig {
                base_delay: Seconds(0.02),
                max_delay: Seconds(0.08),
                channel_phase_offset: PhaseIncrement(0.0),
                depth_min: Seconds(0.0),
                depth_max: Seconds(0.04),
                rate: Hz(TRAJ_RATE),
                depth: Seconds(depth),
                feedback: Feedback(0.0),
                mix: Mix(1.0),
                feedback_max: Feedback::MAX_STABLE,
            },
        );
        tutti_graph::contract::prepared(node, SampleRate(48_000.0), 128)
    }

    const TRAJ_RATE: f32 = 20.0;
    const TRAJ_SR: f64 = 48_000.0;
    const TRAJ_BASE: f64 = 0.02 * TRAJ_SR;
    /// The first frame whose delay line is full of the ramp.
    const TRAJ_FROM: usize = 1_600;

    /// The delay time at each frame, in samples, by feeding the ramp
    /// `x[n] = n` through the node cut into `pieces` (cycled): a linear
    /// interpolated read of a ramp is exact, so `n - y[n]` is the delay the
    /// read used, up to the line's fixed read offset.
    fn delay_trace(depth: f32, len: usize, pieces: &[usize]) -> Vec<f64> {
        delay_trace_stepped(depth, len, pieces, None)
    }

    /// [`delay_trace`] with the rate set to `.1` Hz before the block that
    /// starts at frame `.0` (which the cut must put on a block boundary).
    fn delay_trace_stepped(
        depth: f32,
        len: usize,
        pieces: &[usize],
        step: Option<(usize, f32)>,
    ) -> Vec<f64> {
        let mut node = trajectory_node(depth);
        let x: Vec<f32> = (0..len).map(|n| n as f32).collect();
        let mut y = Vec::with_capacity(len);
        let (mut at, mut k) = (0, 0);
        while at < len {
            let n = pieces[k % pieces.len()].min(len - at);
            if let Some((frame, hz)) = step
                && at == frame
            {
                node.set_rate(hz);
            }
            let xs = &x[at..at + n];
            let o = tutti_graph::contract::drive(&mut node, SampleRate(48_000.0), &[xs], &[]);
            y.extend_from_slice(&o[0]);
            at += n;
            k += 1;
        }
        y.iter()
            .enumerate()
            .map(|(n, &v)| n as f64 - f64::from(v))
            .collect()
    }

    /// The line's fixed read offset: the delay it reads at zero depth, less
    /// the base.
    fn read_offset() -> f64 {
        delay_trace(0.0, 2_000, &[128])[TRAJ_FROM] - TRAJ_BASE
    }

    /// The exact sweep's delay at every frame, in samples: the sine of the
    /// LFO's own phase, which accumulates in f32 (`Phase::advance`) and so
    /// runs a little off the ideal `rate * n / sr` over a long render.
    fn exact_delays(len: usize, depth: f32) -> Vec<f64> {
        exact_delays_stepped(len, depth, None)
    }

    /// [`exact_delays`] with the rate changed to `.1` Hz from frame `.0`.
    fn exact_delays_stepped(len: usize, depth: f32, step: Option<(usize, f32)>) -> Vec<f64> {
        let inc = PhaseIncrement::per_sample(Hz(TRAJ_RATE), SampleRate(TRAJ_SR));
        let mut phase = Phase::START;
        (0..len)
            .map(|n| {
                let inc = match step {
                    Some((frame, hz)) if n >= frame => {
                        PhaseIncrement::per_sample(Hz(hz), SampleRate(TRAJ_SR))
                    }
                    _ => inc,
                };
                let turns = f64::from(phase.get());
                phase = phase.advance(inc);
                TRAJ_BASE + f64::from(depth) * TRAJ_SR * (std::f64::consts::TAU * turns).sin()
            })
            .collect()
    }

    /// The delay time never leaves the declared depth around the centre, and
    /// reaches it.
    ///
    /// Mutation (run): interpolate to `next * 1.1` (an overshooting slope) →
    /// the excursion bound fails; scale the sine by 0.5 → the peak assertion
    /// fails.
    #[test]
    fn the_delay_stays_within_the_declared_depth() {
        let depth = 0.01f32;
        let off = read_offset();
        let amp = f64::from(depth) * TRAJ_SR;
        let trace = delay_trace(depth, 12_000, &[128]);
        let (mut lo, mut hi) = (f64::MAX, f64::MIN);
        for &d in &trace[TRAJ_FROM..] {
            let e = d - off - TRAJ_BASE;
            assert!(e.abs() <= amp + 0.01, "{e} past +-{amp}");
            lo = lo.min(e);
            hi = hi.max(e);
        }
        assert!(hi > amp * 0.999 && lo < -amp * 0.999, "{lo}..{hi}");
    }

    /// At a control point (every 16th frame, counted from the start) the
    /// delay is the exact sine's.
    ///
    /// Mutation (run): look the segment's end point up one sample early
    /// (`i + COEFF_INTERVAL - 1`), or scale the sine by 0.5 → the points
    /// sit samples off.
    #[test]
    fn the_delay_equals_the_exact_sine_at_control_points() {
        let depth = 0.01f32;
        let off = read_offset();
        let trace = delay_trace(depth, 12_000, &[128]);
        let exact = exact_delays(12_000, depth);
        for n in (TRAJ_FROM..12_000).filter(|n| n % COEFF_INTERVAL == 0) {
            let err = trace[n] - off - exact[n];
            assert!(err.abs() < 0.05, "frame {n}: off by {err}");
        }
    }

    /// Between control points the linear chord misses the sine by at most
    /// `h^2 / 8` of the depth, `h` the radians one control interval spans.
    ///
    /// Mutation (run): space the points 64 apart (`COEFF_INTERVAL = 64`) →
    /// the bound, 1.7 samples, fails the spacing assertion.
    #[test]
    fn the_error_between_control_points_is_within_the_chord_bound() {
        let depth = 0.01f32;
        let off = read_offset();
        let trace = delay_trace(depth, 12_000, &[128]);
        let h = std::f64::consts::TAU * f64::from(TRAJ_RATE) * COEFF_INTERVAL as f64 / TRAJ_SR;
        let bound = f64::from(depth) * TRAJ_SR * h * h / 8.0;
        let exact = exact_delays(12_000, depth);
        let mut worst = 0.0f64;
        for n in TRAJ_FROM..12_000 {
            worst = worst.max((trace[n] - off - exact[n]).abs());
        }
        assert!(worst <= bound + 0.05, "worst {worst} over bound {bound}");
        // The render itself, not only the constant: at 20 Hz and 10 ms the
        // delay is off the sine by about a tenth of a sample.
        assert!(worst < 0.2, "worst {worst} samples off the exact sine");
        // The spacing itself: at this worst-case rate and a 480-sample sweep
        // the delay is off by a tenth of a sample (2 us) at most.
        assert!(
            bound < 0.11,
            "a {COEFF_INTERVAL}-sample spacing is {bound} samples off"
        );
        // Not vacuous: the chord does miss, by most of the bound.
        assert!(worst > bound * 0.5, "worst {worst}, bound {bound}");
    }

    /// A rate change at a block boundary mid-segment reaches the delay at the
    /// block's first sample: the segment it begins in ends on a control point
    /// stepped at the new rate, not the old one (and starts on the new
    /// rate's sine).
    ///
    /// Frame 2500 is 4 samples into a segment and the sweep is near a zero
    /// crossing there (the delay moves fastest), so a point stepped at the
    /// old rate (20 Hz, not 30) is several samples off.
    ///
    /// Mutation (run): make `restep` always false → the delay is several
    /// samples off the new-rate sine over the segment and the assertion
    /// fails; drop only the `prev` line → 1.8 samples off.
    #[test]
    fn a_rate_change_at_a_block_boundary_reaches_the_delay_at_once() {
        let depth = 0.01f32;
        let (frame, hz, len) = (2_500, 30.0f32, 4_000);
        assert_ne!(frame % COEFF_INTERVAL, 0, "mid-segment");
        let off = read_offset();
        let trace = delay_trace_stepped(depth, len, &[100], Some((frame, hz)));
        let exact = exact_delays_stepped(len, depth, Some((frame, hz)));
        // The chord bound at the faster of the two rates.
        let h = std::f64::consts::TAU * f64::from(hz) * COEFF_INTERVAL as f64 / TRAJ_SR;
        let bound = f64::from(depth) * TRAJ_SR * h * h / 8.0;
        let mut worst = 0.0f64;
        for n in frame..len {
            worst = worst.max((trace[n] - off - exact[n]).abs());
        }
        assert!(worst <= bound + 0.05, "worst {worst} over bound {bound}");
    }

    /// `reset` and `prepare` re-prime the segment count and the control
    /// points: a node that rendered some frames, then was reset (or
    /// prepared again for a different maximum block), renders ragged blocks
    /// exactly as a fresh node does. The comparison is an invariant of the
    /// node against itself, not a reference render.
    ///
    /// Mutation (run): delete `self.seg_pos = 0` and `self.points_valid =
    /// false` from `reset` (or from `prepare`) → the first segment starts
    /// mid-count on stale points and the renders differ.
    #[test]
    fn reset_and_prepare_reprime_the_control_points() {
        let x = noise(9, 700);
        let pieces = [5usize, 17, 40, 1, 64];
        let render = |node: &mut ModDelayNode| {
            let mut out = vec![Vec::new(), Vec::new()];
            let (mut at, mut k) = (0, 0);
            while at < x.len() {
                let n = pieces[k % pieces.len()].min(x.len() - at);
                let xs = &x[at..at + n];
                let o = tutti_graph::contract::drive(node, SampleRate(48_000.0), &[xs, xs], &[]);
                for (w, o) in out.iter_mut().zip(o) {
                    w.extend(o);
                }
                at += n;
                k += 1;
            }
            out
        };
        let fresh = |max_block: usize| {
            let node = tutti_graph::contract::prepared(
                ModDelayNode::chorus(ChannelLayout::STEREO),
                SampleRate(48_000.0),
                max_block,
            );
            node.set_rate(7.0);
            node
        };
        // 37 frames leaves the count at 5 of 16, with points solved.
        let warm = |node: &mut ModDelayNode| {
            let ones = [0.5f32; 37];
            tutti_graph::contract::drive(node, SampleRate(48_000.0), &[&ones, &ones], &[]);
        };
        let want = render(&mut fresh(64));

        let mut reset = fresh(64);
        warm(&mut reset);
        Node::reset(&mut reset);
        assert_eq!(render(&mut reset), want, "after reset");

        // `prepare` rebuilds the lines and keeps the LFO's phase, so the
        // phase goes back by hand: what is left to differ is the priming.
        let mut again = fresh(128);
        warm(&mut again);
        again.prepare(&Prepare::new(
            SampleRate(48_000.0),
            tutti_types::Samples(64),
        ));
        again.lfo.reset_phase();
        assert_eq!(render(&mut again), want, "after prepare at another block");
    }

    /// The sweep keeps its period: successive upward crossings of the centre
    /// are `sr / rate` apart.
    ///
    /// Mutation (run): step the phase by `inc` twice per sample in
    /// `LfoDrive::fill_block` → the sweep runs twice as fast.
    #[test]
    fn the_modulation_period_is_preserved() {
        let depth = 0.01f32;
        let off = read_offset();
        let trace = delay_trace(depth, 16_000, &[128]);
        let mut ups = Vec::new();
        for n in TRAJ_FROM..15_999 {
            let (a, b) = (trace[n] - off - TRAJ_BASE, trace[n + 1] - off - TRAJ_BASE);
            if a < 0.0 && b >= 0.0 {
                ups.push(n as f64 + (-a) / (b - a));
            }
        }
        assert!(ups.len() >= 5, "{} crossings", ups.len());
        let period = (ups[ups.len() - 1] - ups[0]) / (ups.len() - 1) as f64;
        let want = TRAJ_SR / f64::from(TRAJ_RATE);
        assert!((period - want).abs() < 0.5, "{period} vs {want}");
    }

    /// The result does not depend on how the stream is cut into blocks: the
    /// control points sit on a grid counted across blocks, and a segment's end
    /// point past a block's edge is stepped on from the block's own phase.
    ///
    /// Mutation (run): restart the segment count each block (`self.seg_pos =
    /// 0` at the end of `render`) → blocks that are not a multiple of 16
    /// frames move the points and the outputs differ.
    #[test]
    fn ragged_blocks_render_what_one_block_does() {
        let x = noise(5, 2_000);
        let render = |pieces: &[usize]| {
            let mut node = tutti_graph::contract::prepared(
                ModDelayNode::chorus(ChannelLayout::STEREO),
                SampleRate(48_000.0),
                128,
            );
            node.set_rate(7.0);
            let mut out = vec![Vec::new(), Vec::new()];
            let (mut at, mut k) = (0, 0);
            while at < x.len() {
                let n = pieces[k % pieces.len()].min(x.len() - at);
                let xs = &x[at..at + n];
                let o =
                    tutti_graph::contract::drive(&mut node, SampleRate(48_000.0), &[xs, xs], &[]);
                for (w, o) in out.iter_mut().zip(o) {
                    w.extend(o);
                }
                at += n;
                k += 1;
            }
            out
        };
        let whole = render(&[128]);
        for pieces in [&[1][..], &[5, 17, 40, 1, 64][..], &[16][..], &[127, 3][..]] {
            assert_eq!(render(pieces), whole, "pieces {pieces:?}");
        }
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
        let mut wide = at_48k(ModDelayNode::chorus(6u16));
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
