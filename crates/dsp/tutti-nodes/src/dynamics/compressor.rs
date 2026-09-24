//! CompressorNode with external sidechain, soft knee and makeup gain.

use tutti_core::Arc;
use tutti_core::AtomicF32;
use tutti_core::{AudioUnit, BufferMut, BufferRef, SignalFrame, MAX_BUFFER_SIZE};
use tutti_types::ChannelLayout;

use super::envelope::EnvelopeFollower;
use super::params::{AttackRelease, ThresholdParams};
use super::utils::{
    amplitude_to_db, apply_gain_lane, compute_compressor_gain_reduction, db_to_amplitude, ramp_db,
    sidechain_level_buffer, sidechain_level_slice,
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
/// top of `process` / `tick`.
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
    /// [`SampleRate::DEFAULT`]; `CompressorNode::set_sample_rate` retunes it
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
/// # Port layout & audio-rate modulation
///
/// The audio inputs (`0..ch`) come first, then the sidechain inputs
/// (`ch..2*ch`). For audio-rate threshold modulation the node can grow **one
/// optional param-input port after all audio+sidechain inputs** (see
/// [`CompressorNode::with_param_inputs`]): the threshold port sits at index `2*ch`
/// — index 4 for a stereo compressor — and overrides the threshold atomic per
/// sample, in [`Db`]. Absent, the node is a plain `2*ch`-in node with zero
/// added cost, which is the common case.
///
/// Ask [`threshold_port`](Self::threshold_port) rather than computing the
/// index: it moves with the width.
pub struct CompressorNode {
    core: CompressorCore,
    channels: ChannelLayout,
    /// When true, a threshold param-input port (dB) follows all audio +
    /// sidechain inputs at index `2*channels` and overrides the threshold
    /// atomic per sample.
    mod_threshold: bool,
}

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
    /// **Starts at the placeholder [`SampleRate::DEFAULT`]**: `attack` and
    /// `release` become one-pole coefficients, a conversion from [`Seconds`]
    /// into a per-sample decay that needs the device rate. Call
    /// [`AudioUnit::set_sample_rate`] before the first `process`; it recomputes
    /// them from the times, which stay stored as [`Seconds`].
    ///
    /// Skipping it at 48 kHz makes both envelope times 8.8% short — a 10 ms
    /// attack behaves like 9.1 ms, so the compressor grabs transients slightly
    /// harder and recovers slightly sooner than asked. Nothing sounds broken; it
    /// is simply not the compressor that was configured. Every constructor on
    /// this type funnels through
    /// [`with_channels`](Self::with_channels) and inherits this. See the
    /// crate-level "born at a placeholder rate" section.
    ///
    /// [`SampleRate::DEFAULT`]: tutti_core::SampleRate::DEFAULT
    /// [`AudioUnit::set_sample_rate`]: tutti_core::AudioUnit::set_sample_rate
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
            mod_threshold: false,
        }
    }

    /// A compressor with an optional audio-rate threshold param-input port,
    /// appended after all audio + sidechain inputs. When present it overrides
    /// the threshold atomic per sample; the atomic still holds the base.
    pub fn with_param_inputs(
        threshold_db: impl Into<Db>,
        ratio: impl Into<CompressionRatio>,
        attack: impl Into<Seconds>,
        release: impl Into<Seconds>,
        channels: u8,
        mod_threshold: bool,
    ) -> Self {
        let mut node = Self::with_channels(threshold_db, ratio, attack, release, channels);
        node.mod_threshold = mod_threshold;
        node
    }

    /// Input-port index of the threshold param input, if present (right after
    /// all audio + sidechain inputs, i.e. at `2 * channels`).
    #[inline]
    pub fn threshold_port(&self) -> Option<usize> {
        self.mod_threshold
            .then_some(2 * self.channels.count() as usize)
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
    /// **A present threshold param-input port overrides this per sample.** Read
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
    /// With a threshold param-input port present this sets the *base* the port
    /// overrides, not what the compressor runs at.
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

impl AudioUnit for CompressorNode {
    fn inputs(&self) -> usize {
        2 * self.channels.count() as usize + self.mod_threshold as usize
    }

    fn outputs(&self) -> usize {
        self.channels.count() as usize
    }

    fn reset(&mut self) {
        self.core.reset();
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        self.core.set_sample_rate(sample_rate);
    }

    #[inline]
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        self.core.update_coefficients();
        // A tick is a block of one: the controls are read once here too, and
        // the makeup ramp lands on its end point in its only frame.
        let block = self.core.begin_block();
        let ch = self.channels.count() as usize;
        // A present threshold port (at 2*ch) overrides the atomic; the atomic
        // carries the base for the fast path / UI handle.
        let threshold = self.threshold_port().map(|p| Db(input[p]));
        let sc = sidechain_level_slice(input, ch);
        let gain = self.core.compute_gain(&block, 0, 1, sc, threshold);
        for c in 0..ch {
            output[c] = input[c] * gain;
        }
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        self.core.update_coefficients();
        let block = self.core.begin_block();
        let ch = self.channels.count() as usize;
        let threshold_port = self.threshold_port();

        // The detector is a recursive envelope, so it runs sample-outer into a
        // per-block gain lane; the gain is then applied channel-outer over
        // planar slices, a memoryless multiply the compiler can vectorize.
        // `size` is at most `MAX_BUFFER_SIZE` (a buffer's fixed width), so the
        // lane lives on the stack.
        let mut gains = [0.0f32; MAX_BUFFER_SIZE];
        for (i, g) in gains[..size].iter_mut().enumerate() {
            let sc = sidechain_level_buffer(input, ch, i);
            let threshold = threshold_port.map(|p| Db(input.at_f32(p, i)));
            *g = self.core.compute_gain(&block, i, size, sc, threshold);
        }
        apply_gain_lane(&gains[..size], ch, input, output);
    }

    fn set(&mut self, setting: tutti_core::Setting) {
        if let Some((param, value)) = tutti_core::unit_param::from_setting(&setting) {
            match param {
                tutti_core::UnitParam::Threshold => self.set_threshold(value),
                tutti_core::UnitParam::Ratio => self.set_ratio(value),
                tutti_core::UnitParam::Attack => self.set_attack(value),
                tutti_core::UnitParam::Release => self.set_release(value),
                tutti_core::UnitParam::GainDb => self.set_makeup(value),
                _ => {}
            }
        }
    }

    fn get_id(&self) -> u64 {
        if self.channels.is_mono() {
            crate::node_id::COMPRESSOR_ID
        } else {
            crate::node_id::STEREO_COMPRESSOR_ID
        }
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let ch = self.channels.count() as usize;
        let mut output = SignalFrame::new(ch);
        for c in 0..ch {
            output.set(c, input.at(c));
        }
        output
    }

    /// A gain processor stops with its input.
    ///
    /// The release envelope decays after the input goes silent, but it only
    /// scales: `output = input * gain`, so a silent input is a silent output
    /// whatever the envelope is doing. Declared rather than left `Unknown` —
    /// one unreporting node on the output path makes the whole graph's tail
    /// unspendable.
    fn tail(&mut self) -> Tail {
        Tail::None
    }

    fn footprint(&self) -> usize {
        core::mem::size_of::<Self>()
    }
}

impl Clone for CompressorNode {
    fn clone(&self) -> Self {
        Self {
            core: self.core.clone(),
            channels: self.channels,
            mod_threshold: self.mod_threshold,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::Ordering;

    #[test]
    fn test_compressor_mono_reduces_gain_on_loud_sidechain() {
        let mut comp = CompressorNode::mono(-20.0, 4.0, 0.0001, 0.1);
        comp.set_sample_rate(tutti_core::SampleRate(44100.0));

        let mut output = [0.0f32];

        for _ in 0..1000 {
            comp.tick(&[0.5, 0.9], &mut output);
        }

        assert!(comp.gain_reduction_db() > Db::UNITY);
        assert!(output[0] < 0.5);
    }

    #[test]
    fn test_compressor_mono_no_reduction_below_threshold() {
        let mut comp = CompressorNode::mono(-10.0, 4.0, 0.001, 0.1);
        comp.set_sample_rate(tutti_core::SampleRate(44100.0));

        let mut output = [0.0f32];

        for _ in 0..1000 {
            comp.tick(&[0.5, 0.1], &mut output);
        }

        assert!(comp.gain_reduction_db() < Db(1.0));
    }

    #[test]
    fn test_compressor_soft_knee_differs_from_hard_knee() {
        let mut hard = CompressorNode::mono(-20.0, 4.0, 0.0001, 0.1);
        hard.set_sample_rate(tutti_core::SampleRate(44100.0));

        let mut soft = CompressorNode::mono(-20.0, 4.0, 0.0001, 0.1).with_soft_knee(12.0);
        soft.set_sample_rate(tutti_core::SampleRate(44100.0));

        let mut hard_out = [0.0f32];
        let mut soft_out = [0.0f32];

        for _ in 0..1000 {
            hard.tick(&[0.5, 0.15], &mut hard_out);
            soft.tick(&[0.5, 0.15], &mut soft_out);
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
        let mut comp = CompressorNode::mono(-20.0, 4.0, 0.0001, 0.1).with_makeup(6.0);
        comp.set_sample_rate(tutti_core::SampleRate(44100.0));

        let mut comp_no_makeup = CompressorNode::mono(-20.0, 4.0, 0.0001, 0.1);
        comp_no_makeup.set_sample_rate(tutti_core::SampleRate(44100.0));

        let mut output = [0.0f32];
        let mut output_no_makeup = [0.0f32];

        for _ in 0..1000 {
            comp.tick(&[0.5, 0.9], &mut output);
            comp_no_makeup.tick(&[0.5, 0.9], &mut output_no_makeup);
        }

        assert!(output[0] > output_no_makeup[0]);
    }

    #[test]
    fn test_compressor_reset() {
        let mut comp = CompressorNode::mono(-20.0, 4.0, 0.0001, 0.1);
        comp.set_sample_rate(tutti_core::SampleRate(44100.0));

        let mut output = [0.0f32];
        for _ in 0..1000 {
            comp.tick(&[0.5, 0.9], &mut output);
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
        let mut comp = CompressorNode::stereo(-20.0, 4.0, 0.0001, 0.1);
        comp.set_sample_rate(tutti_core::SampleRate(44100.0));

        let mut output = [0.0f32; 2];

        for _ in 0..1000 {
            comp.tick(&[0.5, 0.5, 0.9, 0.9], &mut output);
        }

        assert!((output[0] - output[1]).abs() < 0.001);
        assert!(output[0] < 0.5);
    }

    #[test]
    fn test_compressor_stereo_channel_count() {
        let mono = CompressorNode::mono(-20.0, 4.0, 0.001, 0.1);
        assert_eq!(mono.channels(), 1);
        assert_eq!(mono.inputs(), 2);
        assert_eq!(mono.outputs(), 1);

        let stereo = CompressorNode::stereo(-20.0, 4.0, 0.001, 0.1);
        assert_eq!(stereo.channels(), 2);
        assert_eq!(stereo.inputs(), 4);
        assert_eq!(stereo.outputs(), 2);

        let quad = CompressorNode::with_channels(-20.0, 4.0, 0.001, 0.1, 4);
        assert_eq!(quad.channels(), 4);
        assert_eq!(quad.inputs(), 8);
        assert_eq!(quad.outputs(), 4);
    }

    #[test]
    fn test_compressor_get_id_distinguishes_mono_and_stereo() {
        let mono = CompressorNode::mono(-20.0, 4.0, 0.001, 0.1);
        let stereo = CompressorNode::stereo(-20.0, 4.0, 0.001, 0.1);
        assert_eq!(mono.get_id(), crate::node_id::COMPRESSOR_ID);
        assert_eq!(stereo.get_id(), crate::node_id::STEREO_COMPRESSOR_ID);
    }

    // ── Audio-rate threshold param-input port ────────────────────────────────

    #[test]
    fn compressor_param_port_arity_and_indices() {
        // Plain constructors declare no port at either width.
        let dm = CompressorNode::mono(-20.0, 4.0, 0.001, 0.1);
        assert_eq!(dm.inputs(), 2);
        assert_eq!(dm.threshold_port(), None);
        let ds = CompressorNode::stereo(-20.0, 4.0, 0.001, 0.1);
        assert_eq!(ds.inputs(), 4);
        assert_eq!(ds.threshold_port(), None);
        // Mono: audio(1) + sidechain(1) = 2, threshold port at index 2.
        let mono = CompressorNode::with_param_inputs(-20.0, 4.0, 0.001, 0.1, 1, true);
        assert_eq!(mono.inputs(), 3);
        assert_eq!(mono.threshold_port(), Some(2));
        // Stereo: audio(2) + sidechain(2) = 4, threshold port at index 4
        // (strictly AFTER the audio+sidechain inputs).
        let stereo = CompressorNode::with_param_inputs(-20.0, 4.0, 0.001, 0.1, 2, true);
        assert_eq!(stereo.inputs(), 5);
        assert_eq!(stereo.threshold_port(), Some(4));
        // Flag false → no port, arity unchanged.
        let off = CompressorNode::with_param_inputs(-20.0, 4.0, 0.001, 0.1, 2, false);
        assert_eq!(off.inputs(), 4);
        assert_eq!(off.threshold_port(), None);
    }

    #[test]
    fn compressor_unmodulated_matches_held_constant() {
        // A modulated mono compressor whose threshold port is held at the same
        // value as a plain compressor's atomic must produce identical output.
        let mut plain = CompressorNode::mono(-20.0, 4.0, 0.0001, 0.1);
        plain.set_sample_rate(tutti_core::SampleRate(44100.0));

        let mut modn = CompressorNode::with_param_inputs(-20.0, 4.0, 0.0001, 0.1, 1, true);
        modn.set_sample_rate(tutti_core::SampleRate(44100.0));

        let mut plain_out = [0.0f32];
        let mut mod_out = [0.0f32];
        for n in 0..1000 {
            // Feed the same audio + sidechain; modulated node also gets the
            // threshold held at its atomic value (-20.0) on the param port.
            let audio = 0.5;
            let sc = if n % 2 == 0 { 0.9 } else { 0.3 };
            plain.tick(&[audio, sc], &mut plain_out);
            modn.tick(&[audio, sc, -20.0], &mut mod_out);
            assert!(
                (plain_out[0] - mod_out[0]).abs() < 1e-6,
                "modulated-held output diverges from plain at sample {n}: {} vs {}",
                plain_out[0],
                mod_out[0]
            );
        }
    }
}
