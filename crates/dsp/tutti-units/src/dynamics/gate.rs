use tutti_core::Arc;
use tutti_core::AtomicF32;
use tutti_core::{dsp::DEFAULT_SR, AudioUnit, BufferMut, BufferRef, SignalFrame};
use tutti_types::ChannelLayout;

use super::envelope::GateEnvelopeFollower;
use super::params::AttackRelease;
use super::utils::{
    amplitude_to_db, compute_gate_gain, sidechain_level_buffer, sidechain_level_slice,
};
use tutti_core::{Db, Param, Seconds};

/// Shared gate state used by the per-sample gain computation.
#[derive(Clone)]
pub(super) struct GateCore {
    pub threshold_db: Param<Db>,
    pub timing: AttackRelease,
    pub hold: Param<Seconds>,
    pub range_db: Param<Db>,

    envelope: f32,
    follower: GateEnvelopeFollower,
}

impl GateCore {
    pub fn new(
        threshold_db: impl Into<Db>,
        attack: impl Into<Seconds>,
        hold: impl Into<Seconds>,
        release: impl Into<Seconds>,
    ) -> Self {
        let threshold_db = threshold_db.into();
        let attack = attack.into();
        let hold = hold.into();
        let release = release.into();
        Self {
            threshold_db: Param::new(threshold_db),
            timing: AttackRelease::new(attack, release),
            hold: Param::new(hold),
            range_db: Param::new(Db(-80.0)),
            envelope: 0.0,
            follower: GateEnvelopeFollower::new(attack, hold, release, DEFAULT_SR),
        }
    }

    pub fn with_range(mut self, range_db: impl Into<Db>) -> Self {
        self.range_db = Param::new(Db(range_db.into().get().min(0.0)));
        self
    }

    pub fn is_open(&self) -> bool {
        self.follower.value() > 0.5
    }

    pub fn gate_level(&self) -> f32 {
        self.follower.value()
    }

    pub fn reset(&mut self) {
        self.envelope = 0.0;
        self.follower.reset();
    }

    pub fn set_sample_rate(&mut self, sample_rate: impl Into<tutti_core::SampleRate>) {
        let attack = self.timing.attack.load();
        let release = self.timing.release.load();
        self.follower
            .set_sample_rate(sample_rate, attack, self.hold.load(), release);
    }

    #[inline]
    pub fn update_coefficients(&mut self) {
        let attack = self.timing.attack.load();
        let release = self.timing.release.load();
        self.follower
            .update_coefficients(attack, self.hold.load(), release);
    }

    /// Compute gate gain (linear) for the given sidechain level, with an
    /// optional per-sample threshold override (dB). `None` reads the atomic
    /// (the fast path); `Some(db)` overrides it (the audio-rate modulation
    /// path). No extra clamp — the setter stores threshold unclamped.
    #[inline]
    pub fn compute_gain_with_threshold(
        &mut self,
        sc_level: f32,
        threshold_override: Option<f32>,
    ) -> f32 {
        let input_db = amplitude_to_db(sc_level);
        let threshold = threshold_override.unwrap_or_else(|| self.threshold_db.load().get());
        self.envelope = sc_level;
        self.follower.step(input_db.get() >= threshold);
        compute_gate_gain(self.follower.value(), self.range_db.load()).get()
    }
}

/// Gate with external sidechain. Channel-count is runtime-configurable:
/// `channels = N` means N audio inputs + N sidechain inputs + N outputs, with
/// a single linked gate level computed from the max-abs of the sidechain channels.
///
/// - `Gate::mono(..)` — 2 inputs (audio + sidechain), 1 output.
/// - `Gate::stereo(..)` — 4 inputs (L, R, SC-L, SC-R), 2 outputs, linked gate.
/// - `Gate::with_channels(.., n)` — arbitrary N.
///
/// # Port layout & audio-rate modulation
///
/// The audio inputs (`0..ch`) come first, then the sidechain inputs
/// (`ch..2*ch`). For audio-rate threshold modulation the node can grow **one
/// optional param-input port after all audio+sidechain inputs** (see
/// [`Gate::with_param_inputs`]): if [`Gate::mod_threshold`] is set, the
/// threshold port sits at index `2*ch` (e.g. index 4 for a stereo gate) and
/// overrides the threshold atomic per sample. Absent → a plain `2*ch`-in node,
/// bit-identical output to the unmodulated path (the common case).
pub struct Gate {
    core: GateCore,
    channels: ChannelLayout,
    /// When true, a threshold param-input port (dB) follows all audio +
    /// sidechain inputs at index `2*channels` and overrides the threshold
    /// atomic per sample.
    mod_threshold: bool,
}

impl Gate {
    /// Mono + mono sidechain: 2 inputs, 1 output.
    pub fn mono(
        threshold_db: impl Into<Db>,
        attack: impl Into<Seconds>,
        hold: impl Into<Seconds>,
        release: impl Into<Seconds>,
    ) -> Self {
        Self::with_channels(threshold_db, attack, hold, release, 1)
    }

    /// Stereo + stereo sidechain: 4 inputs, 2 outputs, linked gate.
    pub fn stereo(
        threshold_db: impl Into<Db>,
        attack: impl Into<Seconds>,
        hold: impl Into<Seconds>,
        release: impl Into<Seconds>,
    ) -> Self {
        Self::with_channels(threshold_db, attack, hold, release, 2)
    }

    /// Arbitrary channel count.
    pub fn with_channels(
        threshold_db: impl Into<Db>,
        attack: impl Into<Seconds>,
        hold: impl Into<Seconds>,
        release: impl Into<Seconds>,
        channels: u8,
    ) -> Self {
        Self {
            core: GateCore::new(threshold_db, attack, hold, release),
            channels: ChannelLayout::from_count(channels.max(1) as u16),
            mod_threshold: false,
        }
    }

    /// A gate with an optional audio-rate threshold param-input port, appended
    /// after all audio + sidechain inputs. When present it overrides the
    /// threshold atomic per sample; the atomic still holds the base.
    pub fn with_param_inputs(
        threshold_db: impl Into<Db>,
        attack: impl Into<Seconds>,
        hold: impl Into<Seconds>,
        release: impl Into<Seconds>,
        channels: u8,
        mod_threshold: bool,
    ) -> Self {
        let mut node = Self::with_channels(threshold_db, attack, hold, release, channels);
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

    pub fn with_range(mut self, range_db: impl Into<Db>) -> Self {
        self.core = self.core.with_range(range_db);
        self
    }

    pub fn channels(&self) -> u8 {
        self.channels.count() as u8
    }

    /// The width this gate was built for, as the engine's channel vocabulary.
    /// [`channels`](Self::channels) is the same number as a bare count, kept
    /// for callers doing port arithmetic.
    pub fn layout(&self) -> ChannelLayout {
        self.channels
    }

    pub fn threshold(&self) -> Arc<AtomicF32> {
        self.core.threshold_db.as_atomic()
    }

    pub fn attack_time(&self) -> Arc<AtomicF32> {
        self.core.timing.attack.as_atomic()
    }

    pub fn hold_time(&self) -> Arc<AtomicF32> {
        self.core.hold.as_atomic()
    }

    pub fn release_time(&self) -> Arc<AtomicF32> {
        self.core.timing.release.as_atomic()
    }

    pub fn range(&self) -> Arc<AtomicF32> {
        self.core.range_db.as_atomic()
    }

    pub fn is_open(&self) -> bool {
        self.core.is_open()
    }

    pub fn gate_level(&self) -> f32 {
        self.core.gate_level()
    }

    pub fn set_threshold(&self, db: impl Into<Db>) {
        self.core.threshold_db.store(db.into());
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
}

impl AudioUnit for Gate {
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
        // A present threshold port (at 2*ch) overrides the atomic.
        let threshold = self.threshold_port().map(|p| input[p]);
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
            let threshold = threshold_port.map(|p| input.at_f32(p, i));
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
                tutti_core::UnitParam::Attack => self.set_attack(value),
                tutti_core::UnitParam::Release => self.set_release(value),
                _ => {}
            }
        }
    }

    fn get_id(&self) -> u64 {
        if self.channels.is_mono() {
            crate::node_id::GATE_ID
        } else {
            crate::node_id::STEREO_GATE_ID
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

    fn footprint(&self) -> usize {
        core::mem::size_of::<Self>()
    }
}

impl Clone for Gate {
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
    fn test_gate_starts_closed() {
        let gate = Gate::mono(-30.0, 0.001, 0.01, 0.1);
        assert!(!gate.is_open());
        assert_eq!(gate.gate_level(), 0.0);
    }

    #[test]
    fn test_gate_opens_on_loud_sidechain() {
        let mut gate = Gate::mono(-20.0, 0.0001, 0.01, 0.1);
        gate.set_sample_rate(tutti_core::SampleRate(44100.0));

        let mut output = [0.0f32];

        for _ in 0..500 {
            gate.tick(&[0.5, 0.9], &mut output);
        }

        assert!(gate.is_open());
        assert!(output[0] > 0.3);
    }

    #[test]
    fn test_gate_closes_on_quiet_sidechain() {
        let mut gate = Gate::mono(-20.0, 0.001, 0.001, 0.001).with_range(-60.0);
        gate.set_sample_rate(tutti_core::SampleRate(44100.0));

        let mut output = [0.0f32];

        for _ in 0..500 {
            gate.tick(&[0.5, 0.9], &mut output);
        }
        assert!(gate.is_open());

        for _ in 0..2000 {
            gate.tick(&[0.5, 0.01], &mut output);
        }

        assert!(!gate.is_open());
        assert!(output[0] < 0.1);
    }

    #[test]
    fn test_gate_range_clamps_to_non_positive() {
        let gate = Gate::mono(-20.0, 0.001, 0.01, 0.1).with_range(10.0);
        assert_eq!(gate.range().load(Ordering::Acquire), 0.0);
    }

    #[test]
    fn test_gate_range_attenuates_rather_than_mutes() {
        let mut gate = Gate::mono(-20.0, 0.001, 0.001, 0.001).with_range(-12.0);
        gate.set_sample_rate(tutti_core::SampleRate(44100.0));

        let mut output = [0.0f32];

        for _ in 0..2000 {
            gate.tick(&[0.5, 0.01], &mut output);
        }

        assert!(!gate.is_open());
        assert!(
            output[0] > 0.05,
            "With -12dB range, signal should be attenuated not muted: {}",
            output[0]
        );
        assert!(output[0] < 0.5);
    }

    #[test]
    fn test_gate_reset() {
        let mut gate = Gate::mono(-20.0, 0.0001, 0.01, 0.1);
        gate.set_sample_rate(tutti_core::SampleRate(44100.0));

        let mut output = [0.0f32];
        for _ in 0..500 {
            gate.tick(&[0.5, 0.9], &mut output);
        }
        assert!(gate.is_open());

        gate.reset();
        assert!(!gate.is_open());
        assert_eq!(gate.gate_level(), 0.0);
    }

    #[test]
    fn test_gate_stereo_opens_on_loud_sidechain() {
        let mut gate = Gate::stereo(-20.0, 0.0001, 0.01, 0.1);
        gate.set_sample_rate(tutti_core::SampleRate(44100.0));

        let mut output = [0.0f32; 2];

        for _ in 0..500 {
            gate.tick(&[0.5, 0.4, 0.9, 0.9], &mut output);
        }

        assert!(gate.is_open());
        assert!(output[0] > 0.3);
        assert!(output[1] > 0.2);
    }

    #[test]
    fn test_gate_stereo_channel_count() {
        let mono = Gate::mono(-20.0, 0.001, 0.01, 0.1);
        assert_eq!(mono.channels(), 1);
        assert_eq!(mono.inputs(), 2);
        assert_eq!(mono.outputs(), 1);

        let stereo = Gate::stereo(-20.0, 0.001, 0.01, 0.1);
        assert_eq!(stereo.channels(), 2);
        assert_eq!(stereo.inputs(), 4);
        assert_eq!(stereo.outputs(), 2);
    }

    #[test]
    fn test_gate_stereo_linking() {
        let mut gate = Gate::stereo(-20.0, 0.0001, 0.01, 0.1);
        gate.set_sample_rate(tutti_core::SampleRate(44100.0));

        let mut output = [0.0f32; 2];

        for _ in 0..500 {
            gate.tick(&[0.8, 0.3, 0.9, 0.9], &mut output);
        }

        let ratio = output[1] / output[0];
        assert!(
            (ratio - 0.3 / 0.8).abs() < 0.15,
            "Both channels should have same gate gain, ratio: {}",
            ratio
        );
    }

    #[test]
    fn test_gate_get_id_distinguishes_mono_and_stereo() {
        let mono = Gate::mono(-20.0, 0.001, 0.01, 0.1);
        let stereo = Gate::stereo(-20.0, 0.001, 0.01, 0.1);
        assert_eq!(mono.get_id(), crate::node_id::GATE_ID);
        assert_eq!(stereo.get_id(), crate::node_id::STEREO_GATE_ID);
    }

    // ── Audio-rate threshold param-input port ────────────────────────────────

    #[test]
    fn gate_default_no_ports() {
        let mono = Gate::mono(-20.0, 0.001, 0.01, 0.1);
        assert_eq!(mono.inputs(), 2);
        assert_eq!(mono.threshold_port(), None);

        let stereo = Gate::stereo(-20.0, 0.001, 0.01, 0.1);
        assert_eq!(stereo.inputs(), 4);
        assert_eq!(stereo.threshold_port(), None);
    }

    #[test]
    fn gate_param_port_arity_and_indices() {
        // Mono: audio(1) + sidechain(1) = 2, threshold port at index 2.
        let mono = Gate::with_param_inputs(-20.0, 0.001, 0.01, 0.1, 1, true);
        assert_eq!(mono.inputs(), 3);
        assert_eq!(mono.threshold_port(), Some(2));
        // Stereo: audio(2) + sidechain(2) = 4, threshold port at index 4
        // (strictly AFTER the audio+sidechain inputs).
        let stereo = Gate::with_param_inputs(-20.0, 0.001, 0.01, 0.1, 2, true);
        assert_eq!(stereo.inputs(), 5);
        assert_eq!(stereo.threshold_port(), Some(4));
        // Flag false → no port, arity unchanged.
        let off = Gate::with_param_inputs(-20.0, 0.001, 0.01, 0.1, 2, false);
        assert_eq!(off.inputs(), 4);
        assert_eq!(off.threshold_port(), None);
    }

    #[test]
    fn gate_unmodulated_matches_held_constant() {
        // A modulated mono gate whose threshold port is held at the same value
        // as a plain gate's atomic must produce identical output.
        let mut plain = Gate::mono(-20.0, 0.0001, 0.01, 0.1);
        plain.set_sample_rate(tutti_core::SampleRate(44100.0));

        let mut modn = Gate::with_param_inputs(-20.0, 0.0001, 0.01, 0.1, 1, true);
        modn.set_sample_rate(tutti_core::SampleRate(44100.0));

        let mut plain_out = [0.0f32];
        let mut mod_out = [0.0f32];
        for n in 0..1000 {
            let audio = 0.5;
            // Alternate loud/quiet sidechain to exercise open + close.
            let sc = if n % 200 < 100 { 0.9 } else { 0.01 };
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
