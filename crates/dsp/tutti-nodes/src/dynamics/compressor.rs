//! CompressorNode with external sidechain, soft knee and makeup gain.

use tutti_core::Arc;
use tutti_core::AtomicF32;
use tutti_graph::{Cx, IntoNode, Io, Node, NodeParts, ParamNode, ParamSet, Prepare, Shape, Status};
use tutti_types::{ChannelLayout, UnitParam};

use super::envelope::EnvelopeFollower;
use super::params::{AttackRelease, ThresholdParams};
use super::utils::{
    amplitude_to_db, apply_gain_lane, compute_compressor_gain_reduction, db_to_amplitude, ramp_db,
    sidechain_level,
};
use tutti_core::{Amplitude, CompressionRatio, Db, Param, SampleRate, Seconds, Tail};

use crate::ramp::LastGood;

/// Shared compressor state used by the per-sample gain computation.
#[derive(Clone)]
pub(super) struct CompressorCore {
    pub threshold: ThresholdParams,
    pub ratio: Param<CompressionRatio>,
    pub timing: AttackRelease,
    pub makeup_db: Param<Db>,

    envelope: f32,
    follower: EnvelopeFollower,
    /// The makeup gain the previous block ended on, the start of this block's
    /// ramp. `None` until the first block, which then starts on its own value
    /// — a node has no "previous" makeup to ramp in from.
    last_makeup: Option<Db>,
    /// Last finite threshold, knee, ratio, makeup, attack and release. Every
    /// one of them reaches the envelope follower — a recursive state that a
    /// single NaN or ±∞ leaves NaN for good — so a non-finite write reads as
    /// unchanged (see [`LastGood`]).
    good: [LastGood; 6],
}

/// The controls one block runs on, read from their atomics **once** at the
/// top of `process`.
///
/// Four atomic loads per sample used to sit inside the gain computation. The
/// detector-side three (threshold, knee, ratio) are held for the block: they
/// decide a *target* reduction that the attack/release follower then smooths,
/// so a step between blocks reaches the output already ramped by the envelope.
/// Makeup is not smoothed by anything — it multiplies the output directly — so
/// it ramps linearly from the previous block's value to this one's.
#[derive(Clone, Copy)]
pub(super) struct CompressorBlock {
    threshold: Db,
    knee: Db,
    ratio: CompressionRatio,
    makeup_from: Db,
    makeup_to: Db,
}

impl CompressorCore {
    /// Builds the shared core. The follower is seeded at the placeholder
    /// [`SampleRate::DEFAULT`]; `CompressorNode`'s `prepare` retunes it
    /// from `timing`, which is why the times are kept as [`Seconds`] rather
    /// than only as coefficients — the seconds are the recoverable form, the
    /// coefficients are not.
    ///
    /// [`SampleRate::DEFAULT`]: tutti_core::SampleRate::DEFAULT
    pub fn new(
        threshold_db: impl Into<Db>,
        ratio: impl Into<CompressionRatio>,
        attack: impl Into<Seconds>,
        release: impl Into<Seconds>,
    ) -> Self {
        let threshold_db = threshold_db.into();
        let ratio = CompressionRatio::new_clamped(ratio.into().get());
        let attack = attack.into();
        let release = release.into();
        Self {
            threshold: ThresholdParams::new(threshold_db, Db(0.0)),
            ratio: Param::new(ratio),
            timing: AttackRelease::new(attack, release),
            makeup_db: Param::new(Db(0.0)),
            envelope: 0.0,
            follower: EnvelopeFollower::new(attack, release, SampleRate::DEFAULT),
            last_makeup: None,
            good: [
                LastGood::new(threshold_db.get()),
                LastGood::new(0.0),
                LastGood::new(ratio.get()),
                LastGood::new(0.0),
                LastGood::new(attack.get()),
                LastGood::new(release.get()),
            ],
        }
    }

    pub fn with_soft_knee(mut self, knee_db: impl Into<Db>) -> Self {
        self.threshold.knee = Param::new(Db(knee_db.into().get().max(0.0)));
        self
    }

    pub fn with_makeup(mut self, makeup_db: impl Into<Db>) -> Self {
        self.makeup_db = Param::new(makeup_db.into());
        self
    }

    pub fn gain_reduction_db(&self) -> Db {
        Db(self.follower.value())
    }

    /// The sidechain peak the detector last saw. An `Amplitude`, not a
    /// unitless envelope — `GateCore::gate_level` has the same shape and name
    /// but is a 0..1 open-fraction, and the types are what stop the two being
    /// swapped.
    pub fn envelope_level(&self) -> Amplitude {
        Amplitude(self.envelope)
    }

    pub fn reset(&mut self) {
        self.envelope = 0.0;
        self.follower.reset();
        // Ramp history is state like the envelope: a reset node starts its
        // next block on the current makeup rather than ramping in from a
        // value it held before the discontinuity.
        self.last_makeup = None;
    }

    pub fn set_sample_rate(&mut self, sample_rate: impl Into<tutti_core::SampleRate>) {
        let (attack, release) = self.times();
        self.follower.set_sample_rate(sample_rate, attack, release);
    }

    #[inline]
    pub fn update_coefficients(&mut self) {
        let (attack, release) = self.times();
        self.follower.update_coefficients(attack, release);
    }

    /// Attack and release, with non-finite writes held off.
    #[inline]
    fn times(&mut self) -> (Seconds, Seconds) {
        (
            Seconds(self.good[4].read(self.timing.attack.load().get())),
            Seconds(self.good[5].read(self.timing.release.load().get())),
        )
    }

    /// Read every block-rate control once, and move the makeup ramp's start
    /// to this block's end.
    #[inline]
    pub fn begin_block(&mut self) -> CompressorBlock {
        let (threshold, knee) = self.threshold.load();
        let threshold = Db(self.good[0].read(threshold.get()));
        let knee = Db(self.good[1].read(knee.get()));
        let makeup_to = Db(self.good[3].read(self.makeup_db.load().get()));
        let makeup_from = self.last_makeup.unwrap_or(makeup_to);
        self.last_makeup = Some(makeup_to);
        CompressorBlock {
            threshold,
            knee,
            ratio: CompressionRatio(self.good[2].read(self.ratio.load().get())),
            makeup_from,
            makeup_to,
        }
    }

    /// Compressor gain (linear) for frame `i` of an `n`-frame block, given the
    /// sidechain level and an optional per-sample threshold override (dB).
    /// `None` uses the block's threshold (the fast path); `Some(db)` overrides
    /// it — the audio-rate modulation path, which stays per sample because it
    /// is an audio signal, not an atomic. No extra clamp: threshold has no
    /// min/max in the setter.
    #[inline]
    pub fn compute_gain(
        &mut self,
        block: &CompressorBlock,
        i: usize,
        n: usize,
        sc_level: f32,
        threshold_override: Option<Db>,
    ) -> f32 {
        let input_db = amplitude_to_db(sc_level);
        // A non-finite port sample falls back to the block's threshold rather
        // than reaching the follower.
        let threshold_db = threshold_override
            .filter(|t| t.get().is_finite())
            .unwrap_or(block.threshold);
        let target_reduction =
            compute_compressor_gain_reduction(input_db, threshold_db, block.ratio, block.knee);
        let gain_reduction = self.follower.smooth(target_reduction.get());
        self.envelope = sc_level;
        let makeup = ramp_db(block.makeup_from, block.makeup_to, i, n);
        db_to_amplitude(-gain_reduction + makeup.get()).get()
    }
}

/// CompressorNode with external sidechain. Channel-count is runtime-configurable:
/// `channels = N` means N audio inputs + N sidechain inputs + N outputs, with
/// a single linked gain computed from the max-abs of the sidechain channels.
///
/// - `CompressorNode::mono(..)` — 2 inputs (audio + sidechain), 1 output.
/// - `CompressorNode::stereo(..)` — 4 inputs (L, R, SC-L, SC-R), 2 outputs, linked gain.
/// - `CompressorNode::with_channels(.., n)` — arbitrary N (1..=8 in practice).
///
/// # Modulated threshold
///
/// The audio inputs (`0..ch`) come first, then the sidechain inputs
/// (`ch..2*ch`). The threshold is modulatable by the graph (design doc 013
/// item 6; [`COMPRESSOR_PARAMS`]): a per-frame threshold in [`Db`] on the
/// param port ([`Io::param`](tutti_graph::Io::param)) overrides the threshold
/// cell per sample. Unmodulated, the node reads its cell once per block,
/// which is the common case; the arity never changes.
///
/// # In a graph
///
/// A native node ([`IntoNode`]): inserted, its controls are a [`ParamSet`]
/// over threshold, ratio, attack, release and makeup (as
/// [`UnitParam::GainDb`]), and a fork of it starts from the values last set
/// through that set. The graph prepares it at the device rate before its
/// first block.
pub struct CompressorNode {
    core: CompressorCore,
    channels: ChannelLayout,
    /// The per-frame gain lane, sized to the prepared `MaxBlock`.
    gains: Vec<f32>,
}

/// The params a [`CompressorNode`] lets the graph modulate, in port order.
pub const COMPRESSOR_PARAMS: [UnitParam; 1] = [UnitParam::Threshold];

impl CompressorNode {
    /// Mono + mono sidechain: 2 inputs (audio, sidechain), 1 output.
    ///
    /// `threshold_db` is the level in [`Db`] above which reduction begins —
    /// typically negative, since 0 dB is full scale. `ratio` is the
    /// [`CompressionRatio`]: `4.0` means 4 dB in yields 1 dB out above the
    /// threshold, and it is clamped to at least `1.0` (no expansion). `attack`
    /// and `release` are [`Seconds`] envelope times — short attacks catch
    /// transients, long releases sound smoother.
    ///
    /// Knee is hard and makeup is 0 dB; add them with
    /// [`with_soft_knee`](Self::with_soft_knee) / [`with_makeup`](Self::with_makeup).
    ///
    /// The envelope's one-pole coefficients are derived from the times at
    /// the rate [`Node::prepare`] hands it.
    pub fn mono(
        threshold_db: impl Into<Db>,
        ratio: impl Into<CompressionRatio>,
        attack: impl Into<Seconds>,
        release: impl Into<Seconds>,
    ) -> Self {
        Self::with_channels(threshold_db, ratio, attack, release, 1)
    }

    /// Stereo + stereo sidechain: 4 inputs (L, R, SC-L, SC-R), 2 outputs.
    ///
    /// The gain is **linked** — one reduction computed from the loudest
    /// sidechain channel and applied to both — so the stereo image does not
    /// shift when one side is louder. Parameters are as [`mono`](Self::mono).
    pub fn stereo(
        threshold_db: impl Into<Db>,
        ratio: impl Into<CompressionRatio>,
        attack: impl Into<Seconds>,
        release: impl Into<Seconds>,
    ) -> Self {
        Self::with_channels(threshold_db, ratio, attack, release, 2)
    }

    /// Arbitrary channel count — 4 for quad, 6 for 5.1 — clamped to at least 1.
    ///
    /// `N` audio inputs, then `N` sidechain inputs, then `N` outputs, with one
    /// **linked** gain computed from the loudest sidechain channel. Parameters
    /// are as [`mono`](Self::mono).
    pub fn with_channels(
        threshold_db: impl Into<Db>,
        ratio: impl Into<CompressionRatio>,
        attack: impl Into<Seconds>,
        release: impl Into<Seconds>,
        channels: u8,
    ) -> Self {
        Self {
            core: CompressorCore::new(threshold_db, ratio, attack, release),
            channels: ChannelLayout::from(channels.max(1) as u16),
            gains: Vec::new(),
        }
    }

    /// Softens the threshold over a `knee_db`-wide band in [`Db`], floored at
    /// 0.
    ///
    /// The ratio eases in across the knee rather than switching on at the
    /// threshold, which is what makes compression on vocals and busses sound
    /// gradual instead of grabbing. `0.0` is a hard knee — the default. Typical
    /// musical values are 6–12 dB; the band straddles the threshold, so half
    /// sits below it.
    pub fn with_soft_knee(mut self, knee_db: impl Into<Db>) -> Self {
        self.core = self.core.with_soft_knee(knee_db);
        self
    }

    /// Adds `makeup_db` of output gain in [`Db`], applied after reduction.
    ///
    /// Compression lowers the peaks, so makeup restores the perceived level —
    /// it is what makes a compressed signal comparable to the uncompressed one.
    /// Applied unconditionally, including when nothing is being reduced.
    pub fn with_makeup(mut self, makeup_db: impl Into<Db>) -> Self {
        self.core = self.core.with_makeup(makeup_db);
        self
    }

    /// The audio channel width this compressor was built for.
    ///
    /// It has `2 * channels` inputs (audio then sidechain) and `channels`
    /// outputs, plus a threshold port if one was requested.
    pub fn channels(&self) -> u8 {
        self.channels.count() as u8
    }

    /// The width this compressor was built for, as the engine's channel
    /// vocabulary. [`channels`](Self::channels) is the same number as a bare
    /// count, kept for callers doing port arithmetic.
    pub fn layout(&self) -> ChannelLayout {
        self.channels
    }

    /// The shared threshold cell in [`Db`] — the level above which reduction
    /// begins.
    ///
    /// **A threshold the graph feeds overrides this per sample.** Read
    /// once per sample otherwise. Shared across clones.
    pub fn threshold(&self) -> Arc<AtomicF32> {
        self.core.threshold.threshold.as_atomic()
    }

    /// The shared [`CompressionRatio`] cell: dB in per dB out above the
    /// threshold.
    ///
    /// Writing the raw cell bypasses [`set_ratio`](Self::set_ratio)'s clamp to
    /// at least `1.0`; below that a compressor would expand.
    pub fn ratio(&self) -> Arc<AtomicF32> {
        self.core.ratio.as_atomic()
    }

    /// The shared attack-time cell in [`Seconds`] — how fast the envelope rises
    /// toward a new, louder level.
    ///
    /// Shorter catches transients, longer lets them through. Coefficients are
    /// recomputed once per block from this.
    pub fn attack_time(&self) -> Arc<AtomicF32> {
        self.core.timing.attack.as_atomic()
    }

    /// The shared release-time cell in [`Seconds`] — how fast the envelope
    /// falls once the signal drops.
    ///
    /// Too short pumps audibly on sustained material; longer sounds smoother.
    pub fn release_time(&self) -> Arc<AtomicF32> {
        self.core.timing.release.as_atomic()
    }

    /// The shared makeup-gain cell in [`Db`], applied after reduction.
    pub fn makeup_gain(&self) -> Arc<AtomicF32> {
        self.core.makeup_db.as_atomic()
    }

    /// The shared knee-width cell in [`Db`]. `0.0` is a hard knee.
    pub fn knee_width(&self) -> Arc<AtomicF32> {
        self.core.threshold.knee.as_atomic()
    }

    /// Sets the threshold in [`Db`], unclamped.
    ///
    /// While the graph modulates the threshold this sets the *base* its
    /// modulation rides on, not what the compressor runs at.
    pub fn set_threshold(&self, db: impl Into<Db>) {
        self.core.threshold.threshold.store(db.into());
    }

    /// Sets the [`CompressionRatio`], clamped to at least `1.0`.
    ///
    /// `1.0` is no compression; higher reduces more. The clamp is what keeps a
    /// compressor from becoming an expander.
    pub fn set_ratio(&self, ratio: impl Into<CompressionRatio>) {
        self.core
            .ratio
            .store(CompressionRatio::new_clamped(ratio.into().get()));
    }

    /// Sets the attack time in [`Seconds`], floored at 0.
    pub fn set_attack(&self, seconds: impl Into<Seconds>) {
        self.core
            .timing
            .attack
            .store(Seconds(seconds.into().get().max(0.0)));
    }

    /// Sets the release time in [`Seconds`], floored at 0.
    pub fn set_release(&self, seconds: impl Into<Seconds>) {
        self.core
            .timing
            .release
            .store(Seconds(seconds.into().get().max(0.0)));
    }

    /// Sets the makeup gain in [`Db`], applied after reduction.
    pub fn set_makeup(&self, db: impl Into<Db>) {
        self.core.makeup_db.store(db.into());
    }

    /// The gain reduction currently applied, in [`Db`] — a **measurement**, for
    /// driving a reduction meter.
    ///
    /// [`Db::UNITY`] means nothing is being reduced; larger values mean more
    /// reduction. Reflects the smoothed envelope, so it follows the attack and
    /// release times rather than the instantaneous level.
    pub fn gain_reduction_db(&self) -> Db {
        self.core.gain_reduction_db()
    }

    /// The sidechain peak the detector last saw, as an [`Amplitude`] — a
    /// **measurement**, for driving an input meter.
    ///
    /// This is the level *fed to* the detector, before any gain decision.
    /// Distinct from `GateNode`'s similarly-shaped reading, which is an open
    /// fraction rather than a level; the unit types are what keep them apart.
    pub fn envelope_level(&self) -> Amplitude {
        self.core.envelope_level()
    }
}

impl Node for CompressorNode {
    /// `2 * channels` in (audio, then sidechain), `channels` out, the
    /// threshold modulatable ([`COMPRESSOR_PARAMS`]).
    ///
    /// No tail: the release envelope decays after the input goes silent,
    /// but it only scales (`output = input * gain`), so a silent input is a
    /// silent output whatever the envelope is doing.
    fn shape(&self) -> Shape {
        let n = self.channels.count();
        Shape::audio(ChannelLayout::from_count(2 * n), self.channels)
            .with_tail(Tail::None)
            .with_params(&COMPRESSOR_PARAMS)
    }

    fn prepare(&mut self, p: &Prepare) {
        self.core.set_sample_rate(p.sample_rate());
        self.gains = vec![0.0; p.max_block().get()];
    }

    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let size = io.frames();
        self.core.update_coefficients();
        let block = self.core.begin_block();
        let ch = self.channels.count() as usize;
        // A modulated threshold overrides the cell per sample; the cell is
        // the base the graph's modulation rides on.
        let threshold_fed = io.param(0).frames();
        let (inputs, mut outputs) = io.split();

        // The detector is a recursive envelope, so it runs sample-outer into a
        // per-block gain lane; the gain is then applied channel-outer over
        // planar slices, a memoryless multiply the compiler can vectorize.
        // The lane is sized to the prepared `MaxBlock`.
        let gains = &mut self.gains[..size];
        for (i, g) in gains.iter_mut().enumerate() {
            let sc = sidechain_level(&inputs, ch, i);
            let threshold = threshold_fed.map(|v| Db(v[i]));
            *g = self.core.compute_gain(&block, i, size, sc, threshold);
        }
        apply_gain_lane(gains, ch, &inputs, &mut outputs);
        Status::Modified
    }

    fn reset(&mut self) {
        self.core.reset();
    }

    fn param_base(&self, k: usize) -> Option<f32> {
        (k == 0).then(|| self.core.threshold.threshold.load().get())
    }
}

impl ParamNode for CompressorNode {
    /// Threshold, ratio, attack, release, and the makeup gain as
    /// [`UnitParam::GainDb`] (the address `set(Setting)` took it by).
    fn param_set(&self) -> ParamSet {
        ParamSet::builder()
            .param(UnitParam::Threshold, self.threshold())
            .param(UnitParam::Ratio, self.ratio())
            .param(UnitParam::Attack, self.attack_time())
            .param(UnitParam::Release, self.release_time())
            .param(UnitParam::GainDb, self.makeup_gain())
            .build()
    }

    /// A clone with every control cell detached (the knee too, which has no
    /// address but is read per block), its envelope cleared.
    fn fork_fresh(&self) -> Self {
        let mut fork = self.clone();
        fork.core.threshold.detach();
        fork.core.ratio.detach();
        fork.core.timing.detach();
        fork.core.makeup_db.detach();
        Node::reset(&mut fork);
        fork
    }
}

/// Inserted with its [`ParamSet`] as its controls and a fork from the values
/// last set through it ([`tutti_graph::param_parts`]).
impl IntoNode for CompressorNode {
    type Controls = ParamSet;

    fn into_parts(self) -> NodeParts<ParamSet> {
        tutti_graph::param_parts(self)
    }
}

/// Shares the control cells (the template [`tutti_graph::param_parts`]
/// forks from); the detector state is copied.
impl Clone for CompressorNode {
    fn clone(&self) -> Self {
        Self {
            core: self.core.clone(),
            channels: self.channels,
            gains: self.gains.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{prepared_at, tick, tick_fed, RATE_44K};
    use core::sync::atomic::Ordering;
    use tutti_graph::contract::{assert_param_fork, drive};

    /// A fork starts from the values last set through the node's
    /// `ParamSet` and shares no cell with it.
    ///
    /// Mutation (run): drop `fork.core.ratio.detach()` in `fork_fresh` → "a
    /// live write reached the fork" for `Ratio` → fails.
    #[test]
    fn a_fork_shares_no_cell() {
        assert_param_fork(CompressorNode::stereo(-20.0, 4.0, 0.001, 0.1).with_makeup(3.0));
    }

    /// A block longer than the old 64-frame stack lane renders whole: the
    /// gain lane is sized from the prepared `MaxBlock`, not a constant.
    ///
    /// Mutation (run): size the lane `vec![0.0; 64]` in `prepare` → the
    /// 1024-frame block panics indexing past it → fails.
    #[test]
    fn a_block_up_to_the_prepared_maximum_renders() {
        let mut comp = prepared_at(CompressorNode::mono(-20.0, 4.0, 0.0001, 0.1), RATE_44K);
        let audio = vec![0.5f32; 1024];
        let sc = vec![0.9f32; 1024];
        let out = drive(&mut comp, RATE_44K, &[&audio, &sc], &[]).remove(0);
        assert!(out[1023] < 0.5, "the loud sidechain reduced the tail");
    }

    #[test]
    fn test_compressor_mono_reduces_gain_on_loud_sidechain() {
        let mut comp = prepared_at(CompressorNode::mono(-20.0, 4.0, 0.0001, 0.1), RATE_44K);

        let mut output = [0.0f32];

        for _ in 0..1000 {
            tick(&mut comp, &[0.5, 0.9], &mut output);
        }

        assert!(comp.gain_reduction_db() > Db::UNITY);
        assert!(output[0] < 0.5);
    }

    #[test]
    fn test_compressor_mono_no_reduction_below_threshold() {
        let mut comp = prepared_at(CompressorNode::mono(-10.0, 4.0, 0.001, 0.1), RATE_44K);

        let mut output = [0.0f32];

        for _ in 0..1000 {
            tick(&mut comp, &[0.5, 0.1], &mut output);
        }

        assert!(comp.gain_reduction_db() < Db(1.0));
    }

    #[test]
    fn test_compressor_soft_knee_differs_from_hard_knee() {
        let mut hard = prepared_at(CompressorNode::mono(-20.0, 4.0, 0.0001, 0.1), RATE_44K);

        let mut soft = prepared_at(
            CompressorNode::mono(-20.0, 4.0, 0.0001, 0.1).with_soft_knee(12.0),
            RATE_44K,
        );

        let mut hard_out = [0.0f32];
        let mut soft_out = [0.0f32];

        for _ in 0..1000 {
            tick(&mut hard, &[0.5, 0.15], &mut hard_out);
            tick(&mut soft, &[0.5, 0.15], &mut soft_out);
        }

        assert!(
            (hard_out[0] - soft_out[0]).abs() > 0.001,
            "Soft knee should produce different output near threshold: hard={}, soft={}",
            hard_out[0],
            soft_out[0]
        );
    }

    #[test]
    fn test_compressor_makeup_gain() {
        let mut comp = prepared_at(
            CompressorNode::mono(-20.0, 4.0, 0.0001, 0.1).with_makeup(6.0),
            RATE_44K,
        );

        let mut comp_no_makeup =
            prepared_at(CompressorNode::mono(-20.0, 4.0, 0.0001, 0.1), RATE_44K);

        let mut output = [0.0f32];
        let mut output_no_makeup = [0.0f32];

        for _ in 0..1000 {
            tick(&mut comp, &[0.5, 0.9], &mut output);
            tick(&mut comp_no_makeup, &[0.5, 0.9], &mut output_no_makeup);
        }

        assert!(output[0] > output_no_makeup[0]);
    }

    #[test]
    fn test_compressor_reset() {
        let mut comp = prepared_at(CompressorNode::mono(-20.0, 4.0, 0.0001, 0.1), RATE_44K);

        let mut output = [0.0f32];
        for _ in 0..1000 {
            tick(&mut comp, &[0.5, 0.9], &mut output);
        }
        assert!(comp.gain_reduction_db() > Db::UNITY);

        comp.reset();
        assert_eq!(comp.gain_reduction_db(), Db::UNITY);
        assert_eq!(comp.envelope_level(), Amplitude::SILENT);
    }

    #[test]
    fn test_compressor_ratio_clamps_to_minimum() {
        let comp = CompressorNode::mono(-20.0, 4.0, 0.001, 0.1);
        comp.set_ratio(0.5);
        assert_eq!(comp.ratio().load(Ordering::Acquire), 1.0);
    }

    #[test]
    fn test_compressor_stereo_reduces_on_loud_sidechain() {
        let mut comp = prepared_at(CompressorNode::stereo(-20.0, 4.0, 0.0001, 0.1), RATE_44K);

        let mut output = [0.0f32; 2];

        for _ in 0..1000 {
            tick(&mut comp, &[0.5, 0.5, 0.9, 0.9], &mut output);
        }

        assert!((output[0] - output[1]).abs() < 0.001);
        assert!(output[0] < 0.5);
    }

    #[test]
    fn test_compressor_stereo_channel_count() {
        let mono = CompressorNode::mono(-20.0, 4.0, 0.001, 0.1);
        assert_eq!(mono.channels(), 1);
        assert_eq!(mono.shape().audio_in.count(), 2);
        assert_eq!(mono.shape().audio_out.count(), 1);

        let stereo = CompressorNode::stereo(-20.0, 4.0, 0.001, 0.1);
        assert_eq!(stereo.channels(), 2);
        assert_eq!(stereo.shape().audio_in.count(), 4);
        assert_eq!(stereo.shape().audio_out.count(), 2);

        let quad = CompressorNode::with_channels(-20.0, 4.0, 0.001, 0.1, 4);
        assert_eq!(quad.channels(), 4);
        assert_eq!(quad.shape().audio_in.count(), 8);
        assert_eq!(quad.shape().audio_out.count(), 4);
    }

    // ── Modulated threshold (the graph's param feed) ────────────────────────

    /// The shape declares the threshold, and it never changes the arity:
    /// audio plus sidechain, at every width.
    ///
    /// Mutation (run): declare no params in `shape` → the first assertion
    /// fails.
    #[test]
    fn compressor_declares_its_threshold_param() {
        let m = CompressorNode::mono(-20.0, 4.0, 0.0001, 0.1);
        assert_eq!(m.shape().params.as_slice(), &[UnitParam::Threshold][..]);
        assert_eq!(m.shape().audio_in.count(), 2);
        assert_eq!(
            m.param_base(0),
            Some(-20.0),
            "the base is the threshold control"
        );
        let s = CompressorNode::stereo(-20.0, 4.0, 0.0001, 0.1);
        assert_eq!(s.shape().audio_in.count(), 4);
    }

    #[test]
    fn compressor_unmodulated_matches_held_constant() {
        // A modulated mono compressor whose fed threshold is held at the same
        // value as a plain compressor's atomic must produce identical output.
        let mut plain = prepared_at(CompressorNode::mono(-20.0, 4.0, 0.0001, 0.1), RATE_44K);

        let mut modn = prepared_at(CompressorNode::mono(-20.0, 4.0, 0.0001, 0.1), RATE_44K);

        let mut plain_out = [0.0f32];
        let mut mod_out = [0.0f32];
        for n in 0..1000 {
            // Feed the same audio + sidechain; modulated node also gets the
            // threshold fed at its atomic value (-20.0).
            let audio = 0.5;
            let sc = if n % 2 == 0 { 0.9 } else { 0.3 };
            tick(&mut plain, &[audio, sc], &mut plain_out);
            tick_fed(&mut modn, &[audio, sc], &[Some(-20.0)], &mut mod_out);
            assert!(
                (plain_out[0] - mod_out[0]).abs() < 1e-6,
                "modulated-held output diverges from plain at sample {n}: {} vs {}",
                plain_out[0],
                mod_out[0]
            );
        }
    }
}
