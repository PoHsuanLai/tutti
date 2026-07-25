use tutti_core::Arc;
use tutti_core::AtomicF32;
use tutti_core::{dsp::Real, AudioUnit, BufferMut, BufferRef, SignalFrame};
use tutti_core::{Db, Hz, Q};

use super::svf::{SvfFilterNode, SvfType};

/// Enable/bypass state for an EQ band. Replaces a raw `bool` so callers
/// cannot confuse "active" with unrelated boolean flags, and so the two
/// modes have names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BandState {
    #[default]
    Active,
    Bypassed,
}

impl BandState {
    #[inline]
    pub fn is_active(self) -> bool {
        matches!(self, Self::Active)
    }

    #[inline]
    pub fn from_enabled(enabled: bool) -> Self {
        if enabled {
            Self::Active
        } else {
            Self::Bypassed
        }
    }
}

impl From<bool> for BandState {
    #[inline]
    fn from(enabled: bool) -> Self {
        Self::from_enabled(enabled)
    }
}

/// Parametric EQ band wrapping [`SvfFilterNode`] with zero-cost bypass.
/// 1 input, 1 output.
///
/// `F` is the internal state precision; defaults to `f64`.
pub struct EqBandNode<F: Real = f64> {
    svf: SvfFilterNode<F>,
    state: BandState,
}

impl<F: Real> EqBandNode<F> {
    pub fn new(
        filter_type: SvfType,
        frequency: impl Into<Hz>,
        q: impl Into<Q>,
        gain_db: impl Into<Db>,
    ) -> Self {
        Self {
            svf: SvfFilterNode::<F>::new(filter_type, frequency, q).with_gain_db(gain_db),
            state: BandState::Active,
        }
    }

    pub fn frequency(&self) -> Arc<AtomicF32> {
        self.svf.frequency()
    }

    pub fn q(&self) -> Arc<AtomicF32> {
        self.svf.q()
    }

    pub fn gain_db(&self) -> Arc<AtomicF32> {
        self.svf.gain_db()
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.state = BandState::from_enabled(enabled);
    }

    pub fn is_enabled(&self) -> bool {
        self.state.is_active()
    }

    pub fn state(&self) -> BandState {
        self.state
    }

    pub fn set_state(&mut self, state: BandState) {
        self.state = state;
    }

    pub fn set_filter_type(&mut self, filter_type: SvfType) {
        self.svf.set_filter_type(filter_type);
    }
}

impl<F: Real + 'static> AudioUnit for EqBandNode<F> {
    fn inputs(&self) -> usize {
        1
    }

    fn outputs(&self) -> usize {
        1
    }

    fn reset(&mut self) {
        self.svf.reset();
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        self.svf.set_sample_rate(sample_rate);
    }

    #[inline]
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        match self.state {
            BandState::Active => self.svf.tick(input, output),
            BandState::Bypassed => output[0] = input[0],
        }
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        match self.state {
            BandState::Active => self.svf.process(size, input, output),
            BandState::Bypassed => {
                for i in 0..size {
                    output.set_f32(0, i, input.at_f32(0, i));
                }
            }
        }
    }

    fn set(&mut self, setting: tutti_core::dsp::Setting) {
        // Delegate to the inner SVF, which owns frequency/Q/gain.
        self.svf.set(setting);
    }

    fn get_id(&self) -> u64 {
        crate::node_id::EQ_BAND_ID
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    fn route(&mut self, input: &SignalFrame, frequency: f64) -> SignalFrame {
        match self.state {
            BandState::Active => self.svf.route(input, frequency),
            BandState::Bypassed => {
                let mut out = SignalFrame::new(1);
                out.set(0, input.at(0));
                out
            }
        }
    }

    fn footprint(&self) -> usize {
        core::mem::size_of::<Self>()
    }
}

impl<F: Real> Clone for EqBandNode<F> {
    fn clone(&self) -> Self {
        Self {
            svf: self.svf.clone(),
            state: self.state,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::test_utils::{generate_sine, process_mono, rms};

    #[test]
    fn test_eq_band_f32_variant_compiles_and_runs() {
        let mut eq = EqBandNode::<f32>::new(SvfType::Bell, 1000.0, 1.0, 6.0);
        eq.set_sample_rate(tutti_core::SampleRate(44100.0));
        let input = generate_sine(1000.0, 44100.0, 1024);
        let out = process_mono(&mut eq, &input);
        assert!(
            rms(&out[256..]) > 0.0,
            "eq f32 variant should produce signal"
        );
    }

    #[test]
    fn test_eq_band_bypass() {
        let mut eq = EqBandNode::<f64>::new(SvfType::Bell, 1000.0, 1.0, 6.0);
        eq.set_sample_rate(tutti_core::SampleRate(44100.0));
        eq.set_enabled(false);

        let sine = generate_sine(1000.0, 44100.0, 1024);
        let out = process_mono(&mut eq, &sine);

        for i in 0..sine.len() {
            assert!(
                (sine[i] - out[i]).abs() < 0.0001,
                "Bypassed EQ should pass through at sample {i}"
            );
        }
    }

    #[test]
    fn test_eq_band_bell_boost() {
        let mut eq = EqBandNode::<f64>::new(SvfType::Bell, 1000.0, 1.0, 12.0);
        eq.set_sample_rate(tutti_core::SampleRate(44100.0));

        let sine = generate_sine(1000.0, 44100.0, 4096);
        let out = process_mono(&mut eq, &sine);

        let rms_in = rms(&sine[512..]);
        let rms_out = rms(&out[512..]);

        assert!(
            rms_out > rms_in,
            "Bell boost should increase level: in={rms_in}, out={rms_out}"
        );
    }

    #[test]
    fn test_eq_band_filter_type_change() {
        let mut eq = EqBandNode::<f64>::new(SvfType::LowPass, 500.0, 0.707, 0.0);
        eq.set_sample_rate(tutti_core::SampleRate(44100.0));

        let high = generate_sine(5000.0, 44100.0, 2048);
        let out_lp = process_mono(&mut eq, &high);
        eq.reset();

        eq.set_filter_type(SvfType::HighPass);
        let out_hp = process_mono(&mut eq, &high);

        let rms_lp = rms(&out_lp[512..]);
        let rms_hp = rms(&out_hp[512..]);

        assert!(
            rms_hp > rms_lp * 2.0,
            "HP should pass more high freq than LP: lp={rms_lp}, hp={rms_hp}"
        );
    }
}
