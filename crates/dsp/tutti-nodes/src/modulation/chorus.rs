//! Stereo chorus — a longer modulated delay that thickens rather than combs.

use tutti_core::Arc;
use tutti_core::AtomicF32;
use tutti_core::{AudioUnit, BufferMut, BufferRef, Feedback, Mix, SignalFrame};

use super::modulated_delay::{ModulatedDelay, ModulatedDelayConfig};

/// Stereo chorus effect. 2-in, 2-out.
///
/// A `ModulatedDelay` with chorus defaults: a 10 ms base delay and a
/// quarter-cycle L/R phase offset. The long base delay is what separates
/// chorus from flanger — the copy sits far enough behind to read as a second
/// voice thickening the sound, rather than close enough to comb-filter it.
/// The L/R offset sweeps the two channels out of step, which is what makes the
/// result feel wide.
pub struct ChorusNode {
    core: ModulatedDelay,
}

impl Default for ChorusNode {
    fn default() -> Self {
        Self::new()
    }
}

impl ChorusNode {
    /// Builds a chorus at 1 Hz rate, 5 ms sweep depth, 0.3 feedback and a 50/50
    /// [`Mix`].
    ///
    /// Allocates its delay lines, so build before the node goes live.
    ///
    /// **Starts at the placeholder [`SampleRate::DEFAULT`]**; call
    /// [`AudioUnit::set_sample_rate`] before the first `process`, which rebuilds
    /// the lines (and so reallocates). Skip it at 48 kHz and the sweep covers
    /// 8.8% less delay at 8.8% below the configured 1 Hz — shallower and lazier
    /// than asked for, and still unmistakably a chorus, which is why nothing
    /// downstream flags it.
    ///
    /// This constructor takes no rate, and [`Default`] could not take one at
    /// all — which is the concrete reason this type carries a placeholder rather
    /// than a mandatory argument. See the crate-level "born at a placeholder
    /// rate" section.
    ///
    /// [`SampleRate::DEFAULT`]: tutti_core::SampleRate::DEFAULT
    /// [`AudioUnit::set_sample_rate`]: tutti_core::AudioUnit::set_sample_rate
    pub fn new() -> Self {
        Self {
            core: ModulatedDelay::new(ModulatedDelayConfig::CHORUS, 1.0, 0.005, 0.3, 0.5),
        }
    }

    /// The shared LFO rate cell in [`Hz`](tutti_core::Hz) — how fast the delay
    /// sweeps.
    ///
    /// Chorus rates are slow, typically 0.1–2 Hz; faster reads as vibrato.
    /// Shared across clones.
    pub fn rate(&self) -> Arc<AtomicF32> {
        self.core.lfo.rate.as_atomic()
    }

    /// The shared sweep-depth cell in [`Seconds`](tutti_core::Seconds) — how far
    /// the delay time moves.
    ///
    /// **Denominated in seconds of delay, not a fraction**: this is a duration
    /// added to the base delay, unlike the phaser's unitless depth.
    pub fn depth(&self) -> Arc<AtomicF32> {
        self.core.mix.depth.as_atomic()
    }

    /// The shared [`Feedback`] cell — how much of the delayed signal
    /// recirculates.
    ///
    /// Writing the raw cell bypasses [`set_feedback`](Self::set_feedback)'s
    /// stability clamp.
    pub fn feedback(&self) -> Arc<AtomicF32> {
        self.core.mix.feedback.as_atomic()
    }

    /// The shared wet/dry [`Mix`] cell: `0.0` dry, `1.0` fully wet.
    ///
    /// Chorus depends on hearing both — at fully wet the detuned copy stands
    /// alone and the thickening disappears.
    pub fn mix(&self) -> Arc<AtomicF32> {
        self.core.mix.mix.as_atomic()
    }

    /// Sets the LFO rate in [`Hz`](tutti_core::Hz), floored at 0.01 Hz.
    pub fn set_rate(&self, hz: impl Into<tutti_core::Hz>) {
        self.core
            .lfo
            .rate
            .store(tutti_core::Hz(hz.into().get().max(0.01)));
    }

    /// Sets the sweep depth in [`Seconds`](tutti_core::Seconds), clamped to
    /// `0.0..=0.04`.
    ///
    /// The 40 ms ceiling bounds the sweep to what the delay line holds. Deeper
    /// sweeps detune more audibly.
    pub fn set_depth(&self, secs: impl Into<tutti_core::Seconds>) {
        self.core
            .mix
            .depth
            .store(tutti_core::Seconds(secs.into().get().clamp(0.0, 0.04)));
    }

    /// Sets the [`Feedback`], clamped to the stable range.
    pub fn set_feedback(&self, fb: impl Into<Feedback>) {
        self.core
            .mix
            .feedback
            .store(Feedback::new_clamped(fb.into().get()));
    }

    /// Sets the wet/dry [`Mix`], clamped to `0.0..=1.0`.
    pub fn set_mix(&self, mix: impl Into<Mix>) {
        self.core.mix.mix.store(Mix::new_clamped(mix.into().get()));
    }
}

impl AudioUnit for ChorusNode {
    fn inputs(&self) -> usize {
        2
    }
    fn outputs(&self) -> usize {
        2
    }

    fn reset(&mut self) {
        self.core.reset();
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        self.core.set_sample_rate(sample_rate);
    }

    #[inline]
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        self.core.process_sample(input[0], input[1], output);
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        let mut out = [0.0f32; 2];
        for i in 0..size {
            self.core
                .process_sample(input.at_f32(0, i), input.at_f32(1, i), &mut out);
            output.set_f32(0, i, out[0]);
            output.set_f32(1, i, out[1]);
        }
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
        crate::node_id::CHORUS_ID
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    /// Zero latency: the modulated delay is the chorus's sound, not processing
    /// latency, and PDC would otherwise delay every other path by the base
    /// delay (design doc 013, D1; the full argument is on
    /// [`DelayLineNode`](crate::DelayLineNode)'s `route`). `distort`, because a
    /// swept delay has no fixed frequency response.
    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let mut out = SignalFrame::new(2);
        out.set(0, input.at(0).distort(0.0));
        out.set(1, input.at(1).distort(0.0));
        out
    }

    fn footprint(&self) -> usize {
        core::mem::size_of::<Self>() + self.core.footprint()
    }
}

impl Clone for ChorusNode {
    fn clone(&self) -> Self {
        Self {
            core: self.core.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chorus_passthrough_dry() {
        let mut chorus = ChorusNode::new();
        chorus.set_sample_rate(tutti_core::SampleRate(44100.0));
        chorus.set_mix(0.0);

        let mut out = [0.0f32; 2];
        chorus.tick(&[0.5, -0.3], &mut out);
        assert!((out[0] - 0.5).abs() < 0.001);
        assert!((out[1] - (-0.3)).abs() < 0.001);
    }

    #[test]
    fn test_chorus_stereo_difference() {
        let mut chorus = ChorusNode::new();
        chorus.set_sample_rate(tutti_core::SampleRate(44100.0));
        chorus.set_mix(1.0);

        let mut out = [0.0f32; 2];
        let mut l_sum = 0.0f64;
        let mut r_sum = 0.0f64;
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
    fn test_chorus_reset() {
        let mut chorus = ChorusNode::new();
        chorus.set_sample_rate(tutti_core::SampleRate(44100.0));

        let mut out = [0.0f32; 2];
        for _ in 0..100 {
            chorus.tick(&[1.0, 1.0], &mut out);
        }
        chorus.reset();
        chorus.tick(&[0.0, 0.0], &mut out);
        assert!(
            out[0].abs() < 0.01,
            "After reset, output should be near zero"
        );
    }
}
