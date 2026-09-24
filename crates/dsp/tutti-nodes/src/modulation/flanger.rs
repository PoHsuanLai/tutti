//! Stereo flanger — a short modulated delay whose comb notches sweep.

use tutti_core::Arc;
use tutti_core::AtomicF32;
use tutti_core::{AudioUnit, BufferMut, BufferRef, Feedback, Mix, SignalFrame};

use super::modulated_delay::{ModulatedDelay, ModulatedDelayConfig};

/// Stereo flanger effect. 2-in, 2-out.
///
/// A `ModulatedDelay` with flanger defaults: a 1 ms base delay, a shallow
/// sweep, high feedback and a half-cycle L/R phase offset. The short base delay
/// is the whole difference from chorus — the copy lands close enough to
/// interfere with the original, producing the comb-filter notches whose sweep
/// is the flanger's signature. Feedback sharpens those notches into the
/// familiar metallic ring; the half-cycle offset puts the channels in
/// opposition for a wide sweep.
pub struct FlangerNode {
    core: ModulatedDelay,
}

impl Default for FlangerNode {
    fn default() -> Self {
        Self::new()
    }
}

impl FlangerNode {
    /// Builds a flanger at 0.5 Hz rate, 2 ms sweep depth, 0.7 feedback and a
    /// 50/50 [`Mix`].
    ///
    /// Allocates its delay lines, so build before the node goes live.
    ///
    /// **Starts at the placeholder [`DEFAULT_SAMPLE_RATE`]**; call
    /// [`AudioUnit::set_sample_rate`] before the first `process`, which rebuilds
    /// the lines (and so reallocates). Skip it at 48 kHz and the 2 ms sweep
    /// covers 8.8% less delay at 8.8% below the configured 0.5 Hz. A flanger is
    /// the least forgiving of the three: its notches are comb peaks set by the
    /// delay in *samples*, so shortening the sweep moves every notch up in
    /// frequency — the effect keeps working and simply sits somewhere else.
    ///
    /// This constructor takes no rate, and [`Default`] could not take one at
    /// all — which is the concrete reason this type carries a placeholder rather
    /// than a mandatory argument. See the crate-level "born at a placeholder
    /// rate" section.
    ///
    /// [`DEFAULT_SAMPLE_RATE`]: tutti_core::dsp::DEFAULT_SAMPLE_RATE
    /// [`AudioUnit::set_sample_rate`]: tutti_core::AudioUnit::set_sample_rate
    pub fn new() -> Self {
        Self {
            core: ModulatedDelay::new(ModulatedDelayConfig::FLANGER, 0.5, 0.002, 0.7, 0.5),
        }
    }

    /// The shared LFO rate cell in [`Hz`](tutti_core::Hz) — how fast the notches
    /// sweep.
    ///
    /// Flanger rates are slow, typically 0.1–1 Hz. Shared across clones.
    pub fn rate(&self) -> Arc<AtomicF32> {
        self.core.lfo.rate.as_atomic()
    }

    /// The shared sweep-depth cell in [`Seconds`](tutti_core::Seconds) — how far
    /// the delay time moves.
    ///
    /// **Denominated in seconds of delay, not a fraction.** Flanger depths are
    /// an order of magnitude shallower than chorus, which is what keeps the
    /// notches in comb range.
    pub fn depth(&self) -> Arc<AtomicF32> {
        self.core.mix.depth.as_atomic()
    }

    /// The shared [`Feedback`] cell — how much of the delayed signal
    /// recirculates.
    ///
    /// The flanger's most characterful control: higher feedback sharpens the
    /// comb notches into a resonant, metallic sweep. Writing the raw cell
    /// bypasses [`set_feedback`](Self::set_feedback)'s stability clamp.
    pub fn feedback(&self) -> Arc<AtomicF32> {
        self.core.mix.feedback.as_atomic()
    }

    /// The shared wet/dry [`Mix`] cell: `0.0` dry, `1.0` fully wet.
    ///
    /// A flanger needs both halves — the comb notches come from wet and dry
    /// interfering, so 50/50 is the deepest setting and fully wet cancels the
    /// effect.
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
    /// `0.0001..=0.01`.
    ///
    /// The 10 ms ceiling keeps the sweep inside comb-filter range; past it the
    /// effect drifts toward chorus. The floor keeps the sweep audible.
    pub fn set_depth(&self, secs: impl Into<tutti_core::Seconds>) {
        self.core
            .mix
            .depth
            .store(tutti_core::Seconds(secs.into().get().clamp(0.0001, 0.01)));
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

impl AudioUnit for FlangerNode {
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
        crate::node_id::FLANGER_ID
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    /// Zero latency: the modulated delay is the flanger's sound, not processing
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

impl Clone for FlangerNode {
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
    fn test_flanger_passthrough_dry() {
        let mut flanger = FlangerNode::new();
        flanger.set_sample_rate(tutti_core::SampleRate(44100.0));
        flanger.set_mix(0.0);

        let mut out = [0.0f32; 2];
        flanger.tick(&[0.7, -0.4], &mut out);
        assert!((out[0] - 0.7).abs() < 0.001);
        assert!((out[1] - (-0.4)).abs() < 0.001);
    }

    #[test]
    fn test_flanger_feedback_effect() {
        let mut flanger = FlangerNode::new();
        flanger.set_sample_rate(tutti_core::SampleRate(44100.0));
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
    fn test_flanger_reset() {
        let mut flanger = FlangerNode::new();
        flanger.set_sample_rate(tutti_core::SampleRate(44100.0));

        let mut out = [0.0f32; 2];
        for _ in 0..100 {
            flanger.tick(&[1.0, 1.0], &mut out);
        }
        flanger.reset();
        flanger.tick(&[0.0, 0.0], &mut out);
        assert!(out[0].abs() < 0.01);
    }
}
