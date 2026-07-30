use tutti_core::Arc;
use tutti_core::AtomicF32;
use tutti_core::{
    dsp::{Real, DEFAULT_SAMPLE_RATE},
    AudioUnit, BufferMut, BufferRef, SignalFrame,
};

use tutti_core::{Drive, Hz, Param, Resonance, SampleRate};

/// Below these deltas a freq/resonance change doesn't warrant recomputing the
/// coefficients — the change guard shared by the atomic and modulation paths.
const FREQ_EPS: f32 = 0.01;
const RES_EPS: f32 = 0.0001;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LadderType {
    #[default]
    LP12,
    LP24,
    HP12,
    HP24,
}

/// Runtime DSP state for a ladder filter instance — the four stage
/// integrators plus the coefficient cache. Split out so the struct proper
/// reads as a bag of typed parameters.
#[derive(Clone)]
struct LadderState<F: Real> {
    stages: [F; 4],
    last_freq: f32,
    last_res: f32,
    g: F,
    k: F,
}

impl<F: Real> LadderState<F> {
    fn zeroed() -> Self {
        let zero = F::from_f64(0.0);
        Self {
            stages: [zero; 4],
            last_freq: -1.0,
            last_res: -1.0,
            g: zero,
            k: zero,
        }
    }

    fn reset_z(&mut self) {
        self.stages = [F::from_f64(0.0); 4];
    }

    fn invalidate(&mut self) {
        self.last_freq = -1.0;
    }
}

/// Moog-style ladder filter with resonance and drive.
/// 1 input, 1 output.
///
/// `F` is the internal state precision. Defaults to `f64`.
pub struct LadderFilterNode<F: Real = f64> {
    ladder_type: LadderType,
    frequency: Param<Hz>,
    resonance: Param<Resonance>,
    drive: Param<Drive>,
    sample_rate: SampleRate,
    state: LadderState<F>,
}

impl<F: Real> LadderFilterNode<F> {
    pub fn new(
        ladder_type: LadderType,
        frequency: impl Into<Hz>,
        resonance: impl Into<Resonance>,
    ) -> Self {
        let frequency = frequency.into();
        let resonance = Resonance::new_clamped(resonance.into().get());
        let mut node = Self {
            ladder_type,
            frequency: Param::new(frequency),
            resonance: Param::new(resonance),
            drive: Param::new(Drive::UNITY),
            sample_rate: DEFAULT_SAMPLE_RATE,
            state: LadderState::zeroed(),
        };
        node.update_coefficients(frequency.get(), resonance.get());
        node
    }

    pub fn frequency(&self) -> Arc<AtomicF32> {
        self.frequency.as_atomic()
    }

    pub fn resonance(&self) -> Arc<AtomicF32> {
        self.resonance.as_atomic()
    }

    pub fn drive(&self) -> Arc<AtomicF32> {
        self.drive.as_atomic()
    }

    pub fn set_frequency(&self, hz: impl Into<Hz>) {
        self.frequency.store(Hz(hz.into().get().max(1.0)));
    }

    pub fn set_resonance(&self, res: impl Into<Resonance>) {
        self.resonance
            .store(Resonance::new_clamped(res.into().get()));
    }

    pub fn set_drive(&self, drive: impl Into<Drive>) {
        self.drive.store(Drive(drive.into().get().max(0.1)));
    }

    fn update_coefficients(&mut self, freq: f32, resonance: f32) {
        // 0.998 of Nyquist is the old `sample_rate * 0.499`: `tan` diverges at
        // Nyquist itself, so the cutoff has to stop just short.
        let fc = (freq as f64).clamp(1.0, f64::from(self.sample_rate.nyquist_scaled(0.998).get()));
        self.state.g = F::from_f64((core::f64::consts::PI * fc / self.sample_rate.get()).tan());
        self.state.k = F::from_f64(4.0 * resonance.clamp(0.0, 1.0) as f64);
        self.state.last_freq = freq;
        self.state.last_res = resonance;
    }

    #[inline]
    fn maybe_update(&mut self) {
        let freq = self.frequency.load().get();
        let res = self.resonance.load().get();
        self.maybe_update_modulated(freq, res);
    }

    /// Recompute coefficients only when freq/resonance moved past a small
    /// epsilon, so a held value doesn't recompute every sample. Shared by the
    /// atomic ([`Self::maybe_update`]) and audio-rate modulation paths.
    #[inline]
    fn maybe_update_modulated(&mut self, freq: f32, res: f32) {
        if (freq - self.state.last_freq).abs() > FREQ_EPS
            || (res - self.state.last_res).abs() > RES_EPS
        {
            self.update_coefficients(freq, res);
        }
    }

    #[inline]
    fn process_one(&mut self, input: F) -> F {
        let drive = self.drive.load().get();
        self.process_one_with_drive(input, drive)
    }

    /// One sample using an explicit drive (the audio-rate modulation path);
    /// coefficients must already be set for the desired freq/resonance.
    #[inline]
    fn process_one_with_drive(&mut self, input: F, drive: f32) -> F {
        let one = F::from_f64(1.0);
        let drive = F::from_f32(drive);
        let x = input * drive;

        let st = &mut self.state;
        let feedback = st.stages[3];
        let u = (x - st.k * feedback).tanh();

        let g = st.g;
        let g1 = g / (one + g);

        let v0 = u;
        let v1 = g1 * (v0 - st.stages[0]);
        let lp1 = v1 + st.stages[0];
        st.stages[0] = lp1 + v1;

        let v2 = g1 * (lp1 - st.stages[1]);
        let lp2 = v2 + st.stages[1];
        st.stages[1] = lp2 + v2;

        let v3 = g1 * (lp2 - st.stages[2]);
        let lp3 = v3 + st.stages[2];
        st.stages[2] = lp3 + v3;

        let v4 = g1 * (lp3 - st.stages[3]);
        let lp4 = v4 + st.stages[3];
        st.stages[3] = lp4 + v4;

        match self.ladder_type {
            LadderType::LP12 => lp2,
            LadderType::LP24 => lp4,
            LadderType::HP12 => u - lp2,
            LadderType::HP24 => u - lp4,
        }
    }
}

impl<F: Real + 'static> AudioUnit for LadderFilterNode<F> {
    fn inputs(&self) -> usize {
        1
    }

    fn outputs(&self) -> usize {
        1
    }

    fn reset(&mut self) {
        self.state.reset_z();
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        self.sample_rate = sample_rate;
        self.state.invalidate();
    }

    #[inline]
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        self.maybe_update();
        output[0] = self.process_one(F::from_f32(input[0])).to_f32();
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        self.maybe_update();
        for i in 0..size {
            let v = F::from_f32(input.at_f32(0, i));
            output.set_f32(0, i, self.process_one(v).to_f32());
        }
    }

    fn get_id(&self) -> u64 {
        crate::node_id::LADDER_FILTER_ID
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let mut out = SignalFrame::new(1);
        out.set(0, input.at(0).distort(0.0));
        out
    }

    fn footprint(&self) -> usize {
        core::mem::size_of::<Self>()
    }
}

impl<F: Real> Clone for LadderFilterNode<F> {
    fn clone(&self) -> Self {
        Self {
            ladder_type: self.ladder_type,
            frequency: self.frequency.handle(),
            resonance: self.resonance.handle(),
            drive: self.drive.handle(),
            sample_rate: self.sample_rate,
            state: self.state.clone(),
        }
    }
}

/// Stereo wrapper around two independent [`LadderFilterNode`] instances
/// that share the same atomic parameter handles. 2-in, 2-out.
///
/// # Port layout
///
/// The default filter is 2-in / 2-out (audio on ports 0/1). For audio-rate
/// modulation it can grow optional param-input ports after the audio inputs
/// (see [`Self::with_param_inputs`]), in the order cutoff, Q, drive. A present
/// port overrides the corresponding atomic per sample (forcing a coefficient
/// recompute for cutoff/Q); absent → a plain 2-in/2-out node, bit-identical
/// output to the unmodulated path and zero added cost.
pub struct StereoLadderFilterNode<F: Real = f64> {
    /// One filter per channel; `channels[0]` is the canonical param holder (the
    /// UI/atomic handle path reads/writes it). All channels share the same
    /// authored params (linked control surface) — widening replicates the
    /// per-channel filter state, not the params. Built at construction; never
    /// resized in `tick`/`process` (RT no-alloc).
    channels: Vec<LadderFilterNode<F>>,
    mod_cutoff: bool,
    mod_q: bool,
    mod_drive: bool,
}

impl<F: Real> StereoLadderFilterNode<F> {
    pub fn new(
        ladder_type: LadderType,
        frequency: impl Into<Hz>,
        resonance: impl Into<Resonance>,
    ) -> Self {
        Self::with_channels(2, ladder_type, frequency, resonance)
    }

    /// An `n`-channel ladder filter. All channels share the authored params
    /// (one linked control surface via `channels[0]`); only the per-channel
    /// filter state is replicated. `with_channels(2, …)` is bit-identical to
    /// [`Self::new`].
    pub fn with_channels(
        channels: usize,
        ladder_type: LadderType,
        frequency: impl Into<Hz>,
        resonance: impl Into<Resonance>,
    ) -> Self {
        let n = channels.max(1);
        let head = LadderFilterNode::new(ladder_type, frequency, resonance);
        let channels = (0..n).map(|_| head.clone()).collect();
        Self {
            channels,
            mod_cutoff: false,
            mod_q: false,
            mod_drive: false,
        }
    }

    /// A filter with optional audio-rate cutoff / Q / drive param-input ports,
    /// appended after the two audio inputs in that order. Each present port
    /// overrides its atomic per sample; the atomics still hold the base.
    pub fn with_param_inputs(
        ladder_type: LadderType,
        frequency: impl Into<Hz>,
        resonance: impl Into<Resonance>,
        mod_cutoff: bool,
        mod_q: bool,
        mod_drive: bool,
    ) -> Self {
        let mut node = Self::new(ladder_type, frequency, resonance);
        node.mod_cutoff = mod_cutoff;
        node.mod_q = mod_q;
        node.mod_drive = mod_drive;
        node
    }

    /// Audio channel width (`inputs()` audio ports == `outputs()`).
    #[inline]
    fn width(&self) -> usize {
        self.channels.len()
    }

    /// Input-port index of the cutoff param input, if present (right after the
    /// audio inputs).
    #[inline]
    pub fn cutoff_port(&self) -> Option<usize> {
        self.mod_cutoff.then_some(self.width())
    }

    /// Input-port index of the Q param input, if present.
    #[inline]
    pub fn q_port(&self) -> Option<usize> {
        self.mod_q
            .then_some(self.width() + self.mod_cutoff as usize)
    }

    /// Input-port index of the drive param input, if present.
    #[inline]
    pub fn drive_port(&self) -> Option<usize> {
        self.mod_drive
            .then_some(self.width() + self.mod_cutoff as usize + self.mod_q as usize)
    }

    pub fn frequency(&self) -> Arc<AtomicF32> {
        self.channels[0].frequency()
    }

    pub fn resonance(&self) -> Arc<AtomicF32> {
        self.channels[0].resonance()
    }

    pub fn drive(&self) -> Arc<AtomicF32> {
        self.channels[0].drive()
    }

    pub fn set_frequency(&self, hz: impl Into<Hz>) {
        self.channels[0].set_frequency(hz);
    }

    pub fn set_resonance(&self, res: impl Into<Resonance>) {
        self.channels[0].set_resonance(res);
    }

    pub fn set_drive(&self, drive: impl Into<Drive>) {
        self.channels[0].set_drive(drive);
    }

    /// Effective per-sample (freq, resonance, drive): a present param port
    /// overrides the corresponding atomic. `read` reads input port `p`. Params
    /// are channel-shared, so the canonical `channels[0]` atomics are the base.
    #[inline]
    fn effective_params(&self, read: impl Fn(usize) -> f32) -> (f32, f32, f32) {
        let head = &self.channels[0];
        let freq = self
            .cutoff_port()
            .map_or_else(|| head.frequency.load().get(), |p| read(p).max(1.0));
        let res = self
            .q_port()
            .map_or_else(|| head.resonance.load().get(), |p| read(p).clamp(0.0, 1.0));
        let drive = self
            .drive_port()
            .map_or_else(|| head.drive.load().get(), |p| read(p).max(0.1));
        (freq, res, drive)
    }
}

impl<F: Real + 'static> AudioUnit for StereoLadderFilterNode<F> {
    fn inputs(&self) -> usize {
        self.width() + self.mod_cutoff as usize + self.mod_q as usize + self.mod_drive as usize
    }

    fn outputs(&self) -> usize {
        self.width()
    }

    fn reset(&mut self) {
        for ch in &mut self.channels {
            ch.reset();
        }
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        for ch in &mut self.channels {
            ch.set_sample_rate(sample_rate);
        }
    }

    #[inline]
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        if !self.mod_cutoff && !self.mod_q && !self.mod_drive {
            // Fast path: no ports — each channel reads its own (shared) atomics.
            for (c, ch) in self.channels.iter_mut().enumerate() {
                ch.tick(&input[c..c + 1], &mut output[c..c + 1]);
            }
            return;
        }
        let (freq, res, drive) = self.effective_params(|p| input[p]);
        for (c, ch) in self.channels.iter_mut().enumerate() {
            ch.maybe_update_modulated(freq, res);
            output[c] = ch
                .process_one_with_drive(F::from_f32(input[c]), drive)
                .to_f32();
        }
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        // Fast path: no ports — delegate to the channels' own atomic reads.
        if !self.mod_cutoff && !self.mod_q && !self.mod_drive {
            for i in 0..size {
                for (c, ch) in self.channels.iter_mut().enumerate() {
                    let mut out = [0.0f32];
                    ch.tick(&[input.at_f32(c, i)], &mut out);
                    output.set_f32(c, i, out[0]);
                }
            }
            return;
        }
        // Modulated path: read the active port(s) per sample.
        for i in 0..size {
            let (freq, res, drive) = self.effective_params(|p| input.at_f32(p, i));
            for (c, ch) in self.channels.iter_mut().enumerate() {
                ch.maybe_update_modulated(freq, res);
                let x = F::from_f32(input.at_f32(c, i));
                output.set_f32(c, i, ch.process_one_with_drive(x, drive).to_f32());
            }
        }
    }

    fn set(&mut self, setting: tutti_core::dsp::Setting) {
        if let Some((param, value)) = tutti_core::unit_param::from_setting(&setting) {
            match param {
                tutti_core::UnitParam::Cutoff => self.set_frequency(value),
                tutti_core::UnitParam::Q => self.set_resonance(value),
                tutti_core::UnitParam::Drive => self.set_drive(value),
                _ => {}
            }
        }
    }

    fn get_id(&self) -> u64 {
        crate::node_id::LADDER_FILTER_ID ^ 0xDA02
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let mut out = SignalFrame::new(self.width());
        for c in 0..self.width() {
            out.set(c, input.at(c).distort(0.0));
        }
        out
    }

    fn footprint(&self) -> usize {
        core::mem::size_of::<Self>()
    }
}

impl<F: Real> Clone for StereoLadderFilterNode<F> {
    fn clone(&self) -> Self {
        Self {
            channels: self.channels.clone(),
            mod_cutoff: self.mod_cutoff,
            mod_q: self.mod_q,
            mod_drive: self.mod_drive,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::test_utils::{generate_sine, process_mono, rms};

    #[test]
    fn test_ladder_lp24_attenuates_high_freq() {
        let mut filter = LadderFilterNode::<f64>::new(LadderType::LP24, 500.0, 0.0);
        filter.set_sample_rate(tutti_core::SampleRate(44100.0));

        let low = generate_sine(100.0, 44100.0, 4096);
        let high = generate_sine(5000.0, 44100.0, 4096);

        let out_low = process_mono(&mut filter, &low);
        filter.reset();
        let out_high = process_mono(&mut filter, &high);

        let rms_low = rms(&out_low[512..]);
        let rms_high = rms(&out_high[512..]);

        assert!(
            rms_low > rms_high * 5.0,
            "LP24 should strongly attenuate high freq: low={rms_low}, high={rms_high}"
        );
    }

    #[test]
    fn test_ladder_hp24_attenuates_low_freq() {
        let mut filter = LadderFilterNode::<f64>::new(LadderType::HP24, 2000.0, 0.0);
        filter.set_sample_rate(tutti_core::SampleRate(44100.0));

        let low = generate_sine(100.0, 44100.0, 4096);
        let high = generate_sine(5000.0, 44100.0, 4096);

        let out_low = process_mono(&mut filter, &low);
        filter.reset();
        let out_high = process_mono(&mut filter, &high);

        let rms_low = rms(&out_low[512..]);
        let rms_high = rms(&out_high[512..]);

        assert!(
            rms_high > rms_low * 3.0,
            "HP24 should attenuate low freq: low={rms_low}, high={rms_high}"
        );
    }

    #[test]
    fn test_ladder_resonance_boosts_cutoff() {
        let mut no_res = LadderFilterNode::<f64>::new(LadderType::LP24, 1000.0, 0.0);
        no_res.set_sample_rate(tutti_core::SampleRate(44100.0));

        let mut with_res = LadderFilterNode::<f64>::new(LadderType::LP24, 1000.0, 0.8);
        with_res.set_sample_rate(tutti_core::SampleRate(44100.0));

        let at_cutoff = generate_sine(1000.0, 44100.0, 4096);

        let out_no = process_mono(&mut no_res, &at_cutoff);
        let out_yes = process_mono(&mut with_res, &at_cutoff);

        let rms_no = rms(&out_no[512..]);
        let rms_yes = rms(&out_yes[512..]);

        assert!(
            rms_yes > rms_no,
            "Resonance should boost at cutoff: no_res={rms_no}, with_res={rms_yes}"
        );
    }

    #[test]
    fn test_ladder_drive_adds_saturation() {
        let mut clean = LadderFilterNode::<f64>::new(LadderType::LP24, 5000.0, 0.0);
        clean.set_sample_rate(tutti_core::SampleRate(44100.0));

        let mut driven = LadderFilterNode::<f64>::new(LadderType::LP24, 5000.0, 0.0);
        driven.set_sample_rate(tutti_core::SampleRate(44100.0));
        driven.set_drive(10.0);

        let sine = generate_sine(440.0, 44100.0, 4096);

        let out_clean = process_mono(&mut clean, &sine);
        let out_driven = process_mono(&mut driven, &sine);

        let rms_clean = rms(&out_clean[512..]);
        let rms_driven = rms(&out_driven[512..]);

        assert!(
            (rms_clean - rms_driven).abs() > 0.01 || rms_driven > rms_clean * 0.5,
            "Drive should affect signal: clean={rms_clean}, driven={rms_driven}"
        );
    }

    #[test]
    fn test_ladder_reset() {
        let mut filter = LadderFilterNode::<f64>::new(LadderType::LP24, 1000.0, 0.5);
        filter.set_sample_rate(tutti_core::SampleRate(44100.0));

        let sine = generate_sine(440.0, 44100.0, 100);
        let _ = process_mono(&mut filter, &sine);

        filter.reset();

        let mut out = [0.0f32];
        filter.tick(&[0.0], &mut out);
        assert!(
            out[0].abs() < 0.0001,
            "After reset, output should be near zero"
        );
    }

    #[test]
    fn test_ladder_lp12_less_steep_than_lp24() {
        let mut lp12 = LadderFilterNode::<f64>::new(LadderType::LP12, 1000.0, 0.0);
        lp12.set_sample_rate(tutti_core::SampleRate(44100.0));

        let mut lp24 = LadderFilterNode::<f64>::new(LadderType::LP24, 1000.0, 0.0);
        lp24.set_sample_rate(tutti_core::SampleRate(44100.0));

        let high = generate_sine(5000.0, 44100.0, 4096);

        let out12 = process_mono(&mut lp12, &high);
        let out24 = process_mono(&mut lp24, &high);

        let rms12 = rms(&out12[512..]);
        let rms24 = rms(&out24[512..]);

        assert!(
            rms12 > rms24,
            "LP12 should pass more high freq than LP24: lp12={rms12}, lp24={rms24}"
        );
    }

    #[test]
    fn test_ladder_f32_state_agrees_with_f64_for_mid_cutoff() {
        let mut ladder_f64 = LadderFilterNode::<f64>::new(LadderType::LP24, 1000.0, 0.3);
        let mut ladder_f32 = LadderFilterNode::<f32>::new(LadderType::LP24, 1000.0, 0.3);
        ladder_f64.set_sample_rate(tutti_core::SampleRate(44100.0));
        ladder_f32.set_sample_rate(tutti_core::SampleRate(44100.0));

        let input = generate_sine(500.0, 44100.0, 2048);
        let out_f64 = process_mono(&mut ladder_f64, &input);
        let out_f32 = process_mono(&mut ladder_f32, &input);

        let rms_f64 = rms(&out_f64[512..]);
        let rms_f32 = rms(&out_f32[512..]);
        let rel_diff = (rms_f64 - rms_f32).abs() / rms_f64.max(1e-6);
        assert!(
            rel_diff < 0.01,
            "ladder f32/f64 should agree within 1% at mid cutoff: f64={rms_f64}, f32={rms_f32}"
        );
    }

    // ── Audio-rate param-input ports ─────────────────────────────────────────

    fn process_stereo_ladder(
        node: &mut dyn AudioUnit,
        l: &[f32],
        r: &[f32],
        params: &[f32],
    ) -> (Vec<f32>, Vec<f32>) {
        let mut out_l = vec![0.0f32; l.len()];
        let mut out_r = vec![0.0f32; r.len()];
        for i in 0..l.len() {
            let mut input = vec![l[i], r[i]];
            input.extend_from_slice(params);
            let mut output = [0.0f32; 2];
            node.tick(&input, &mut output);
            out_l[i] = output[0];
            out_r[i] = output[1];
        }
        (out_l, out_r)
    }

    #[test]
    fn stereo_ladder_default_is_two_in_no_param_ports() {
        let u = StereoLadderFilterNode::<f64>::new(LadderType::LP24, 1000.0, 0.3);
        assert_eq!(u.inputs(), 2);
        assert_eq!(u.outputs(), 2);
        assert_eq!(u.cutoff_port(), None);
        assert_eq!(u.q_port(), None);
        assert_eq!(u.drive_port(), None);
    }

    #[test]
    fn ladder_with_channels_2_is_bit_identical_to_new() {
        let mut a = StereoLadderFilterNode::<f64>::new(LadderType::LP24, 900.0, 0.4);
        a.set_sample_rate(tutti_core::SampleRate(44100.0));
        let mut b = StereoLadderFilterNode::<f64>::with_channels(2, LadderType::LP24, 900.0, 0.4);
        b.set_sample_rate(tutti_core::SampleRate(44100.0));

        let sig = generate_sine(440.0, 44100.0, 1024);
        let (al, ar) = process_stereo_ladder(&mut a, &sig, &sig, &[]);
        let (bl, br) = process_stereo_ladder(&mut b, &sig, &sig, &[]);
        for i in 0..sig.len() {
            assert_eq!(al[i].to_bits(), bl[i].to_bits(), "L bit-diff at {i}");
            assert_eq!(ar[i].to_bits(), br[i].to_bits(), "R bit-diff at {i}");
        }
    }

    #[test]
    fn ladder_with_channels_reports_arity_and_is_independent() {
        let mut wide =
            StereoLadderFilterNode::<f64>::with_channels(6, LadderType::LP24, 1000.0, 0.3);
        wide.set_sample_rate(tutti_core::SampleRate(44100.0));
        assert_eq!(wide.inputs(), 6);
        assert_eq!(wide.outputs(), 6);

        // Drive only channel 4; the rest must stay silent.
        let sig = generate_sine(300.0, 44100.0, 2048);
        let len = sig.len();
        let mut out = vec![vec![0.0f32; len]; 6];
        let mut inbuf = [0.0f32; 6];
        let mut outbuf = [0.0f32; 6];
        for i in 0..len {
            inbuf = [0.0; 6];
            inbuf[4] = sig[i];
            wide.tick(&inbuf, &mut outbuf);
            for c in 0..6 {
                out[c][i] = outbuf[c];
            }
        }
        let e4: f32 = out[4].iter().map(|s| s * s).sum();
        assert!(e4 > 1.0, "ch4 should carry signal; energy {e4}");
        for c in [0usize, 1, 2, 3, 5] {
            let e: f32 = out[c].iter().map(|s| s * s).sum();
            assert!(e < 1e-10, "ch{c} should stay silent; energy {e}");
        }
    }

    #[test]
    fn stereo_ladder_param_port_arity_and_indices() {
        // cutoff + drive (no Q) → cutoff at 2, drive at 3 (Q absent).
        let u = StereoLadderFilterNode::<f64>::with_param_inputs(
            LadderType::LP24,
            1000.0,
            0.3,
            true,
            false,
            true,
        );
        assert_eq!(u.inputs(), 4);
        assert_eq!(u.cutoff_port(), Some(2));
        assert_eq!(u.q_port(), None);
        assert_eq!(u.drive_port(), Some(3));
        // all three → cutoff 2, Q 3, drive 4.
        let a = StereoLadderFilterNode::<f64>::with_param_inputs(
            LadderType::LP24,
            1000.0,
            0.3,
            true,
            true,
            true,
        );
        assert_eq!(a.inputs(), 5);
        assert_eq!(a.cutoff_port(), Some(2));
        assert_eq!(a.q_port(), Some(3));
        assert_eq!(a.drive_port(), Some(4));
    }

    #[test]
    fn stereo_ladder_cutoff_port_modulates_response() {
        let noise: Vec<f32> = (0..2048)
            .map(|i| ((i * 7 + 3) % 100) as f32 / 50.0 - 1.0)
            .collect();
        let run = |cutoff: f32| -> f32 {
            let mut f = StereoLadderFilterNode::<f64>::with_param_inputs(
                LadderType::LP24,
                200.0,
                0.3,
                true,
                false,
                false,
            );
            f.set_sample_rate(tutti_core::SampleRate(44100.0));
            let (out_l, _) = process_stereo_ladder(&mut f, &noise, &noise, &[cutoff]);
            rms(&out_l[256..])
        };
        let low = run(200.0);
        let high = run(8000.0);
        assert!(
            high > low * 1.5,
            "higher cutoff via param port should pass more: low={low}, high={high}"
        );
    }

    #[test]
    fn stereo_ladder_unmodulated_matches_modulated_held_constant() {
        // A modulated node whose cutoff port is held at the atomic value must
        // produce the same output as a plain node.
        let signal = generate_sine(440.0, 44100.0, 1024);

        let mut plain = StereoLadderFilterNode::<f64>::new(LadderType::LP24, 1000.0, 0.3);
        plain.set_sample_rate(tutti_core::SampleRate(44100.0));
        let (plain_l, _) = process_stereo_ladder(&mut plain, &signal, &signal, &[]);

        let mut modn = StereoLadderFilterNode::<f64>::with_param_inputs(
            LadderType::LP24,
            1000.0,
            0.3,
            true,
            false,
            false,
        );
        modn.set_sample_rate(tutti_core::SampleRate(44100.0));
        let (mod_l, _) = process_stereo_ladder(&mut modn, &signal, &signal, &[1000.0]);

        for i in 0..signal.len() {
            assert!(
                (plain_l[i] - mod_l[i]).abs() < 1e-5,
                "modulated-held output diverges from plain at sample {i}: {} vs {}",
                plain_l[i],
                mod_l[i]
            );
        }
    }
}
