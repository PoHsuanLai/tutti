use tutti_core::Arc;
use tutti_core::AtomicF32;
use tutti_core::{dsp::DEFAULT_SR, AudioUnit, BufferMut, BufferRef, SignalFrame};
use tutti_types::ChannelLayout;

use super::envelope::EnvelopeFollower;
use super::params::{AttackRelease, ThresholdParams};
use super::utils::{
    amplitude_to_db, compute_compressor_gain_reduction, db_to_amplitude, sidechain_level_buffer,
    sidechain_level_slice,
};
use tutti_core::{Amplitude, CompressionRatio, Db, Param, Seconds, Tail};

/// Shared compressor state used by the per-sample gain computation.
#[derive(Clone)]
pub(super) struct CompressorCore {
    pub threshold: ThresholdParams,
    pub ratio: Param<CompressionRatio>,
    pub timing: AttackRelease,
    pub makeup_db: Param<Db>,

    envelope: f32,
    follower: EnvelopeFollower,
}

impl CompressorCore {
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
            follower: EnvelopeFollower::new(attack, release, DEFAULT_SR),
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
    /// but is a 0..1 open-fraction, and the two used to be swappable.
    pub fn envelope_level(&self) -> Amplitude {
        Amplitude(self.envelope)
    }

    pub fn reset(&mut self) {
        self.envelope = 0.0;
        self.follower.reset();
    }

    pub fn set_sample_rate(&mut self, sample_rate: impl Into<tutti_core::SampleRate>) {
        let attack = self.timing.attack.load();
        let release = self.timing.release.load();
        self.follower.set_sample_rate(sample_rate, attack, release);
    }

    #[inline]
    pub fn update_coefficients(&mut self) {
        let attack = self.timing.attack.load();
        let release = self.timing.release.load();
        self.follower.update_coefficients(attack, release);
    }

    /// Compute compressor gain (linear) for the given sidechain level, with an
    /// optional per-sample threshold override (dB). `None` reads the atomic
    /// (the fast path); `Some(db)` overrides it (the audio-rate modulation
    /// path). The override is clamped the same way [`Compressor::set_threshold`]
    /// would store it (no extra clamp — threshold has no min/max in the setter).
    #[inline]
    pub fn compute_gain_with_threshold(
        &mut self,
        sc_level: f32,
        threshold_override: Option<Db>,
    ) -> f32 {
        let input_db = amplitude_to_db(sc_level);
        let (atomic_threshold_db, knee_db) = self.threshold.load();
        let threshold_db = threshold_override.unwrap_or(atomic_threshold_db);
        let target_reduction =
            compute_compressor_gain_reduction(input_db, threshold_db, self.ratio.load(), knee_db);
        let gain_reduction = self.follower.smooth(target_reduction.get());
        self.envelope = sc_level;
        db_to_amplitude(-gain_reduction + self.makeup_db.load().get()).get()
    }
}

/// Compressor with external sidechain. Channel-count is runtime-configurable:
/// `channels = N` means N audio inputs + N sidechain inputs + N outputs, with
/// a single linked gain computed from the max-abs of the sidechain channels.
///
/// - `Compressor::mono(..)` — 2 inputs (audio + sidechain), 1 output.
/// - `Compressor::stereo(..)` — 4 inputs (L, R, SC-L, SC-R), 2 outputs, linked gain.
/// - `Compressor::with_channels(.., n)` — arbitrary N (1..=8 in practice).
///
/// # Port layout & audio-rate modulation
///
/// The audio inputs (`0..ch`) come first, then the sidechain inputs
/// (`ch..2*ch`). For audio-rate threshold modulation the node can grow **one
/// optional param-input port after all audio+sidechain inputs** (see
/// [`Compressor::with_param_inputs`]): if [`Compressor::mod_threshold`] is set,
/// the threshold port sits at index `2*ch` (e.g. index 4 for a stereo
/// compressor) and overrides the threshold atomic per sample. Absent → a plain
/// `2*ch`-in node, bit-identical output to the unmodulated path and zero added
/// cost (the common case).
pub struct Compressor {
    core: CompressorCore,
    channels: ChannelLayout,
    /// When true, a threshold param-input port (dB) follows all audio +
    /// sidechain inputs at index `2*channels` and overrides the threshold
    /// atomic per sample.
    mod_threshold: bool,
}

impl Compressor {
    /// Mono + mono sidechain: 2 inputs, 1 output.
    pub fn mono(
        threshold_db: impl Into<Db>,
        ratio: impl Into<CompressionRatio>,
        attack: impl Into<Seconds>,
        release: impl Into<Seconds>,
    ) -> Self {
        Self::with_channels(threshold_db, ratio, attack, release, 1)
    }

    /// Stereo + stereo sidechain: 4 inputs, 2 outputs, linked gain.
    pub fn stereo(
        threshold_db: impl Into<Db>,
        ratio: impl Into<CompressionRatio>,
        attack: impl Into<Seconds>,
        release: impl Into<Seconds>,
    ) -> Self {
        Self::with_channels(threshold_db, ratio, attack, release, 2)
    }

    /// Arbitrary channel count (e.g. 4 for quad, 6 for 5.1).
    pub fn with_channels(
        threshold_db: impl Into<Db>,
        ratio: impl Into<CompressionRatio>,
        attack: impl Into<Seconds>,
        release: impl Into<Seconds>,
        channels: u8,
    ) -> Self {
        Self {
            core: CompressorCore::new(threshold_db, ratio, attack, release),
            channels: ChannelLayout::from_count(channels.max(1) as u16),
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

    pub fn with_soft_knee(mut self, knee_db: impl Into<Db>) -> Self {
        self.core = self.core.with_soft_knee(knee_db);
        self
    }

    pub fn with_makeup(mut self, makeup_db: impl Into<Db>) -> Self {
        self.core = self.core.with_makeup(makeup_db);
        self
    }

    pub fn channels(&self) -> u8 {
        self.channels.count() as u8
    }

    /// The width this compressor was built for, as the engine's channel
    /// vocabulary. [`channels`](Self::channels) is the same number as a bare
    /// count, kept for callers doing port arithmetic.
    pub fn layout(&self) -> ChannelLayout {
        self.channels
    }

    pub fn threshold(&self) -> Arc<AtomicF32> {
        self.core.threshold.threshold.as_atomic()
    }

    pub fn ratio(&self) -> Arc<AtomicF32> {
        self.core.ratio.as_atomic()
    }

    pub fn attack_time(&self) -> Arc<AtomicF32> {
        self.core.timing.attack.as_atomic()
    }

    pub fn release_time(&self) -> Arc<AtomicF32> {
        self.core.timing.release.as_atomic()
    }

    pub fn makeup_gain(&self) -> Arc<AtomicF32> {
        self.core.makeup_db.as_atomic()
    }

    pub fn knee_width(&self) -> Arc<AtomicF32> {
        self.core.threshold.knee.as_atomic()
    }

    pub fn set_threshold(&self, db: impl Into<Db>) {
        self.core.threshold.threshold.store(db.into());
    }

    pub fn set_ratio(&self, ratio: impl Into<CompressionRatio>) {
        self.core
            .ratio
            .store(CompressionRatio::new_clamped(ratio.into().get()));
    }

    pub fn set_attack(&self, seconds: impl Into<Seconds>) {
        self.core
            .timing
            .attack
            .store(Seconds(seconds.into().get().max(0.0)));
    }

    pub fn set_release(&self, seconds: impl Into<Seconds>) {
        self.core
            .timing
            .release
            .store(Seconds(seconds.into().get().max(0.0)));
    }

    pub fn set_makeup(&self, db: impl Into<Db>) {
        self.core.makeup_db.store(db.into());
    }

    pub fn gain_reduction_db(&self) -> Db {
        self.core.gain_reduction_db()
    }

    pub fn envelope_level(&self) -> Amplitude {
        self.core.envelope_level()
    }
}

impl AudioUnit for Compressor {
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
        let ch = self.channels.count() as usize;
        // A present threshold port (at 2*ch) overrides the atomic; the atomic
        // carries the base for the fast path / UI handle.
        let threshold = self.threshold_port().map(|p| Db(input[p]));
        let gain = self
            .core
            .compute_gain_with_threshold(sidechain_level_slice(input, ch), threshold);
        for c in 0..ch {
            output[c] = input[c] * gain;
        }
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        self.core.update_coefficients();
        let ch = self.channels.count() as usize;
        let threshold_port = self.threshold_port();

        for i in 0..size {
            let sc = sidechain_level_buffer(input, ch, i);
            let threshold = threshold_port.map(|p| Db(input.at_f32(p, i)));
            let gain = self.core.compute_gain_with_threshold(sc, threshold);
            for c in 0..ch {
                output.set_f32(c, i, input.at_f32(c, i) * gain);
            }
        }
    }

    fn set(&mut self, setting: tutti_core::dsp::Setting) {
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

impl Clone for Compressor {
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
        let mut comp = Compressor::mono(-20.0, 4.0, 0.0001, 0.1);
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
        let mut comp = Compressor::mono(-10.0, 4.0, 0.001, 0.1);
        comp.set_sample_rate(tutti_core::SampleRate(44100.0));

        let mut output = [0.0f32];

        for _ in 0..1000 {
            comp.tick(&[0.5, 0.1], &mut output);
        }

        assert!(comp.gain_reduction_db() < Db(1.0));
    }

    #[test]
    fn test_compressor_soft_knee_differs_from_hard_knee() {
        let mut hard = Compressor::mono(-20.0, 4.0, 0.0001, 0.1);
        hard.set_sample_rate(tutti_core::SampleRate(44100.0));

        let mut soft = Compressor::mono(-20.0, 4.0, 0.0001, 0.1).with_soft_knee(12.0);
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
        let mut comp = Compressor::mono(-20.0, 4.0, 0.0001, 0.1).with_makeup(6.0);
        comp.set_sample_rate(tutti_core::SampleRate(44100.0));

        let mut comp_no_makeup = Compressor::mono(-20.0, 4.0, 0.0001, 0.1);
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
        let mut comp = Compressor::mono(-20.0, 4.0, 0.0001, 0.1);
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
        let comp = Compressor::mono(-20.0, 4.0, 0.001, 0.1);
        comp.set_ratio(0.5);
        assert_eq!(comp.ratio().load(Ordering::Acquire), 1.0);
    }

    #[test]
    fn test_compressor_stereo_reduces_on_loud_sidechain() {
        let mut comp = Compressor::stereo(-20.0, 4.0, 0.0001, 0.1);
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
        let mono = Compressor::mono(-20.0, 4.0, 0.001, 0.1);
        assert_eq!(mono.channels(), 1);
        assert_eq!(mono.inputs(), 2);
        assert_eq!(mono.outputs(), 1);

        let stereo = Compressor::stereo(-20.0, 4.0, 0.001, 0.1);
        assert_eq!(stereo.channels(), 2);
        assert_eq!(stereo.inputs(), 4);
        assert_eq!(stereo.outputs(), 2);

        let quad = Compressor::with_channels(-20.0, 4.0, 0.001, 0.1, 4);
        assert_eq!(quad.channels(), 4);
        assert_eq!(quad.inputs(), 8);
        assert_eq!(quad.outputs(), 4);
    }

    #[test]
    fn test_compressor_get_id_distinguishes_mono_and_stereo() {
        let mono = Compressor::mono(-20.0, 4.0, 0.001, 0.1);
        let stereo = Compressor::stereo(-20.0, 4.0, 0.001, 0.1);
        assert_eq!(mono.get_id(), crate::node_id::COMPRESSOR_ID);
        assert_eq!(stereo.get_id(), crate::node_id::STEREO_COMPRESSOR_ID);
    }

    // ── Audio-rate threshold param-input port ────────────────────────────────

    #[test]
    fn compressor_default_no_ports() {
        let mono = Compressor::mono(-20.0, 4.0, 0.001, 0.1);
        assert_eq!(mono.inputs(), 2);
        assert_eq!(mono.threshold_port(), None);

        let stereo = Compressor::stereo(-20.0, 4.0, 0.001, 0.1);
        assert_eq!(stereo.inputs(), 4);
        assert_eq!(stereo.threshold_port(), None);
    }

    #[test]
    fn compressor_param_port_arity_and_indices() {
        // Mono: audio(1) + sidechain(1) = 2, threshold port at index 2.
        let mono = Compressor::with_param_inputs(-20.0, 4.0, 0.001, 0.1, 1, true);
        assert_eq!(mono.inputs(), 3);
        assert_eq!(mono.threshold_port(), Some(2));
        // Stereo: audio(2) + sidechain(2) = 4, threshold port at index 4
        // (strictly AFTER the audio+sidechain inputs).
        let stereo = Compressor::with_param_inputs(-20.0, 4.0, 0.001, 0.1, 2, true);
        assert_eq!(stereo.inputs(), 5);
        assert_eq!(stereo.threshold_port(), Some(4));
        // Flag false → no port, arity unchanged.
        let off = Compressor::with_param_inputs(-20.0, 4.0, 0.001, 0.1, 2, false);
        assert_eq!(off.inputs(), 4);
        assert_eq!(off.threshold_port(), None);
    }

    #[test]
    fn compressor_unmodulated_matches_held_constant() {
        // A modulated mono compressor whose threshold port is held at the same
        // value as a plain compressor's atomic must produce identical output.
        let mut plain = Compressor::mono(-20.0, 4.0, 0.0001, 0.1);
        plain.set_sample_rate(tutti_core::SampleRate(44100.0));

        let mut modn = Compressor::with_param_inputs(-20.0, 4.0, 0.0001, 0.1, 1, true);
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
