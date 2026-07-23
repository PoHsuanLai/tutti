use tutti_core::Arc;
use tutti_core::AtomicF32;
use tutti_core::{
    dsp::{Real, DEFAULT_SR},
    AudioUnit, BufferMut, BufferRef, SignalFrame,
};

use tutti_core::{Db, Hz, Param, Ratio};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SvfType {
    #[default]
    LowPass,
    HighPass,
    BandPass,
    Notch,
    /// Allpass: flat magnitude, frequency-dependent phase shift.
    Allpass,
    Bell,
    LowShelf,
    HighShelf,
}

/// Pure SVF coefficient set computed from filter parameters.
#[derive(Debug, Clone, Copy)]
pub(super) struct SvfCoeffs {
    pub a1: f64,
    pub a2: f64,
    pub a3: f64,
    pub m0: f64,
    pub m1: f64,
    pub m2: f64,
}

/// Compute SVF filter coefficients from parameters (pure function, no state).
pub(super) fn compute_svf_coeffs(
    filter_type: SvfType,
    freq: f32,
    q: f32,
    gain_db: f32,
    sample_rate: f64,
) -> SvfCoeffs {
    let fc = (freq as f64).clamp(1.0, sample_rate * 0.499);
    let g = (core::f64::consts::PI * fc / sample_rate).tan();
    let k = 1.0 / (q as f64).max(0.01);

    let a1 = 1.0 / (1.0 + g * (g + k));
    let a2 = g * a1;
    let a3 = g * a2;

    let a = 10.0_f64.powf(gain_db as f64 / 40.0);

    let (m0, m1, m2) = match filter_type {
        SvfType::LowPass => (0.0, 0.0, 1.0),
        SvfType::HighPass => (1.0, -k, -1.0),
        SvfType::BandPass => (0.0, 1.0, 0.0),
        SvfType::Notch => (1.0, -k, 0.0),
        // Allpass: flat magnitude, all-pass phase. m1 = -2k (vs notch's -k).
        SvfType::Allpass => (1.0, -2.0 * k, 0.0),
        SvfType::Bell => (1.0, k * (a * a - 1.0), 0.0),
        SvfType::LowShelf => (1.0, k * (a - 1.0), a * a - 1.0),
        SvfType::HighShelf => (a * a, k * (1.0 - a) * a, 1.0 - a * a),
    };

    SvfCoeffs {
        a1,
        a2,
        a3,
        m0,
        m1,
        m2,
    }
}

/// State-variable filter (LP/HP/BP/Notch/Bell/LowShelf/HighShelf).
/// 1 input, 1 output. Frequency, Q, and gain are modulatable via AtomicF32.
///
/// `F` is the internal state precision. Defaults to `f64` for accuracy at low
/// cutoffs; use `SvfFilterNode::<f32>::new(...)` to trade precision for CPU.
///
/// State is split into [`SvfCoefficients`] (parameter-derived; shared across
/// channels in stereo / multi-channel variants) and [`SvfIntegrator`] (the
/// per-channel z-1 / z-2 delay registers).
struct SvfCoefficients<F: Real> {
    a1: F,
    a2: F,
    a3: F,
    m0: F,
    m1: F,
    m2: F,
    last_freq: f32,
    last_q: f32,
    last_gain_db: f32,
}

impl<F: Real> SvfCoefficients<F> {
    fn zeroed() -> Self {
        let zero = F::from_f64(0.0);
        Self {
            a1: zero,
            a2: zero,
            a3: zero,
            m0: zero,
            m1: zero,
            m2: zero,
            last_freq: -1.0,
            last_q: -1.0,
            last_gain_db: f32::NAN,
        }
    }

    fn invalidate(&mut self) {
        self.last_freq = -1.0;
    }

    fn store(&mut self, c: SvfCoeffs, freq: f32, q: f32, gain_db: f32) {
        self.a1 = F::from_f64(c.a1);
        self.a2 = F::from_f64(c.a2);
        self.a3 = F::from_f64(c.a3);
        self.m0 = F::from_f64(c.m0);
        self.m1 = F::from_f64(c.m1);
        self.m2 = F::from_f64(c.m2);
        self.last_freq = freq;
        self.last_q = q;
        self.last_gain_db = gain_db;
    }
}

impl<F: Real> Clone for SvfCoefficients<F> {
    fn clone(&self) -> Self {
        Self {
            a1: self.a1,
            a2: self.a2,
            a3: self.a3,
            m0: self.m0,
            m1: self.m1,
            m2: self.m2,
            last_freq: self.last_freq,
            last_q: self.last_q,
            last_gain_db: self.last_gain_db,
        }
    }
}

/// Per-channel SVF integrator state. Stereo and multi-channel variants
/// hold one of these per channel; coefficients are shared.
#[derive(Clone)]
struct SvfIntegrator<F: Real> {
    ic1eq: F,
    ic2eq: F,
}

impl<F: Real> SvfIntegrator<F> {
    fn zeroed() -> Self {
        let zero = F::from_f64(0.0);
        Self {
            ic1eq: zero,
            ic2eq: zero,
        }
    }

    fn reset_z(&mut self) {
        let zero = F::from_f64(0.0);
        self.ic1eq = zero;
        self.ic2eq = zero;
    }

    /// Single-sample tick using the supplied coefficients.
    #[inline]
    fn tick(&mut self, coeffs: &SvfCoefficients<F>, v0: F) -> F {
        let two = F::from_f64(2.0);
        let v3 = v0 - self.ic2eq;
        let v1 = coeffs.a1 * self.ic1eq + coeffs.a2 * v3;
        let v2 = self.ic2eq + coeffs.a2 * self.ic1eq + coeffs.a3 * v3;
        self.ic1eq = two * v1 - self.ic1eq;
        self.ic2eq = two * v2 - self.ic2eq;
        coeffs.m0 * v0 + coeffs.m1 * v1 + coeffs.m2 * v2
    }
}

pub struct SvfFilterNode<F: Real = f64> {
    filter_type: SvfType,
    frequency: Param<Hz>,
    q: Param<Ratio>,
    gain_db: Param<Db>,
    sample_rate: f64,
    coeffs: SvfCoefficients<F>,
    integrator: SvfIntegrator<F>,
}

impl<F: Real> SvfFilterNode<F> {
    pub fn new(filter_type: SvfType, frequency: impl Into<Hz>, q: impl Into<Ratio>) -> Self {
        let frequency = frequency.into();
        let q = q.into();
        let mut node = Self {
            filter_type,
            frequency: Param::new(frequency),
            q: Param::new(q),
            gain_db: Param::new(Db(0.0)),
            sample_rate: DEFAULT_SR,
            coeffs: SvfCoefficients::zeroed(),
            integrator: SvfIntegrator::zeroed(),
        };
        node.update_coefficients(frequency.get(), q.get(), 0.0);
        node
    }

    pub fn with_gain_db(mut self, db: impl Into<Db>) -> Self {
        let db = db.into();
        self.gain_db = Param::new(db);
        self.update_coefficients(self.frequency.load().get(), self.q.load().get(), db.get());
        self
    }

    pub fn frequency(&self) -> Arc<AtomicF32> {
        self.frequency.as_atomic()
    }

    pub fn q(&self) -> Arc<AtomicF32> {
        self.q.as_atomic()
    }

    pub fn gain_db(&self) -> Arc<AtomicF32> {
        self.gain_db.as_atomic()
    }

    pub fn set_frequency(&self, hz: impl Into<Hz>) {
        self.frequency.store(Hz(hz.into().get().max(1.0)));
    }

    pub fn set_q(&self, q: impl Into<Ratio>) {
        self.q.store(Ratio(q.into().get().max(0.01)));
    }

    pub fn set_gain_db(&self, db: impl Into<Db>) {
        self.gain_db.store(db.into());
    }

    /// Switch filter mode. Forces a coefficient recalculation on the next sample.
    pub fn set_filter_type(&mut self, filter_type: SvfType) {
        self.filter_type = filter_type;
        self.coeffs.invalidate();
    }

    fn update_coefficients(&mut self, freq: f32, q: f32, gain_db: f32) {
        let c = compute_svf_coeffs(self.filter_type, freq, q, gain_db, self.sample_rate);
        self.coeffs.store(c, freq, q, gain_db);
    }

    #[inline]
    fn maybe_update(&mut self) {
        let freq = self.frequency.load().get();
        let q = self.q.load().get();
        let gain_db = self.gain_db.load().get();
        if (freq - self.coeffs.last_freq).abs() > 0.01
            || (q - self.coeffs.last_q).abs() > 0.0001
            || (gain_db - self.coeffs.last_gain_db).abs() > 0.01
        {
            self.update_coefficients(freq, q, gain_db);
        }
    }

    #[inline]
    fn process_one(&mut self, v0: F) -> F {
        self.integrator.tick(&self.coeffs, v0)
    }
}

impl<F: Real + 'static> AudioUnit for SvfFilterNode<F> {
    fn inputs(&self) -> usize {
        1
    }

    fn outputs(&self) -> usize {
        1
    }

    fn reset(&mut self) {
        self.integrator.reset_z();
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        self.sample_rate = sample_rate;
        self.coeffs.invalidate();
    }

    #[inline]
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        self.maybe_update();
        output[0] = self.process_one(F::from_f32(input[0])).to_f32();
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        self.maybe_update();
        for i in 0..size {
            let v0 = F::from_f32(input.at_f32(0, i));
            output.set_f32(0, i, self.process_one(v0).to_f32());
        }
    }

    fn set(&mut self, setting: tutti_core::dsp::Setting) {
        if let Some((param, value)) = tutti_core::unit_param::from_setting(&setting) {
            match param {
                tutti_core::UnitParam::Cutoff => self.set_frequency(value),
                tutti_core::UnitParam::Q => self.set_q(value),
                tutti_core::UnitParam::GainDb => self.set_gain_db(value),
                _ => {}
            }
        }
    }

    fn get_id(&self) -> u64 {
        crate::node_id::SVF_FILTER_ID
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let mut out = SignalFrame::new(1);
        out.set(0, input.at(0).filter(0.0, |z| z));
        out
    }

    fn footprint(&self) -> usize {
        core::mem::size_of::<Self>()
    }
}

impl<F: Real> Clone for SvfFilterNode<F> {
    fn clone(&self) -> Self {
        Self {
            filter_type: self.filter_type,
            frequency: self.frequency.handle(),
            q: self.q.handle(),
            gain_db: self.gain_db.handle(),
            sample_rate: self.sample_rate,
            coeffs: self.coeffs.clone(),
            integrator: self.integrator.clone(),
        }
    }
}

// =============================================================================
// Stereo variant — 2-in / 2-out, shared coefficients, per-channel integrators.
// =============================================================================

/// Stereo state-variable filter. 2 inputs, 2 outputs.
///
/// Same params as [`SvfFilterNode`] (frequency / Q / gain), shared
/// across L and R; each channel maintains its own integrator state.
/// Coefficient recomputation happens once per parameter change, not
/// once per channel.
///
/// # Port layout
///
/// The default filter is 2-in / 2-out (stereo audio on ports 0/1). For
/// audio-rate parameter modulation it can grow *optional param-input ports*
/// after the audio inputs (see [`Self::with_param_inputs`]):
/// - if [`Self::mod_cutoff`]: a cutoff param-input port (Hz),
/// - if [`Self::mod_q`]: a Q param-input port,
///
/// in that order. A present param-input port **overrides** the corresponding
/// atomic per sample, forcing a coefficient recompute that sample (the
/// block-rate skip only applies to the unmodulated parameters). When neither
/// flag is set the filter is a plain 2-in/2-out node — bit-identical output to
/// the unmodulated path and zero added cost (the common case).
pub struct StereoSvfFilterNode<F: Real = f64> {
    filter_type: SvfType,
    frequency: Param<Hz>,
    q: Param<Ratio>,
    gain_db: Param<Db>,
    sample_rate: f64,
    coeffs: SvfCoefficients<F>,
    left: SvfIntegrator<F>,
    right: SvfIntegrator<F>,
    /// When true, a cutoff param-input port follows the audio inputs and
    /// overrides [`Self::frequency`] per sample.
    mod_cutoff: bool,
    /// When true, a Q param-input port follows the cutoff port (or the audio
    /// inputs if `mod_cutoff` is false) and overrides [`Self::q`] per sample.
    mod_q: bool,
}

impl<F: Real> StereoSvfFilterNode<F> {
    pub fn new(filter_type: SvfType, frequency: f32, q: f32) -> Self {
        let mut node = Self {
            filter_type,
            frequency: Param::new(Hz(frequency)),
            q: Param::new(Ratio(q)),
            gain_db: Param::new(Db(0.0)),
            sample_rate: DEFAULT_SR,
            coeffs: SvfCoefficients::zeroed(),
            left: SvfIntegrator::zeroed(),
            right: SvfIntegrator::zeroed(),
            mod_cutoff: false,
            mod_q: false,
        };
        node.update_coefficients(frequency, q, 0.0);
        node
    }

    /// A filter with optional audio-rate param-input ports. `mod_cutoff` /
    /// `mod_q` add a cutoff / Q param-input port after the two audio inputs
    /// (cutoff first), each overriding its atomic per sample when present. The
    /// atomics still hold the base (they feed the upstream param-sum's base
    /// port), so the UI handle path is unchanged.
    pub fn with_param_inputs(
        filter_type: SvfType,
        frequency: f32,
        q: f32,
        mod_cutoff: bool,
        mod_q: bool,
    ) -> Self {
        let mut node = Self {
            filter_type,
            frequency: Param::new(Hz(frequency)),
            q: Param::new(Ratio(q)),
            gain_db: Param::new(Db(0.0)),
            sample_rate: DEFAULT_SR,
            coeffs: SvfCoefficients::zeroed(),
            left: SvfIntegrator::zeroed(),
            right: SvfIntegrator::zeroed(),
            mod_cutoff,
            mod_q,
        };
        node.update_coefficients(frequency, q, 0.0);
        node
    }

    /// Input-port index of the cutoff param input, if present (right after the
    /// two audio inputs).
    #[inline]
    pub fn cutoff_port(&self) -> Option<usize> {
        self.mod_cutoff.then_some(2)
    }

    /// Input-port index of the Q param input, if present (after the audio
    /// inputs and the cutoff port).
    #[inline]
    pub fn q_port(&self) -> Option<usize> {
        self.mod_q.then_some(2 + self.mod_cutoff as usize)
    }

    pub fn with_gain_db(mut self, db: f32) -> Self {
        self.gain_db = Param::new(Db(db));
        self.update_coefficients(self.frequency.load().0, self.q.load().0, db);
        self
    }

    pub fn frequency(&self) -> Arc<AtomicF32> {
        self.frequency.as_atomic()
    }

    pub fn q(&self) -> Arc<AtomicF32> {
        self.q.as_atomic()
    }

    pub fn gain_db(&self) -> Arc<AtomicF32> {
        self.gain_db.as_atomic()
    }

    pub fn set_frequency(&self, hz: f32) {
        self.frequency.store(Hz(hz.max(1.0)));
    }

    pub fn set_q(&self, q: f32) {
        self.q.store(Ratio(q.max(0.01)));
    }

    pub fn set_gain_db(&self, db: f32) {
        self.gain_db.store(Db(db));
    }

    pub fn set_filter_type(&mut self, filter_type: SvfType) {
        self.filter_type = filter_type;
        self.coeffs.invalidate();
    }

    fn update_coefficients(&mut self, freq: f32, q: f32, gain_db: f32) {
        let c = compute_svf_coeffs(self.filter_type, freq, q, gain_db, self.sample_rate);
        self.coeffs.store(c, freq, q, gain_db);
    }

    #[inline]
    fn maybe_update(&mut self) {
        let freq = self.frequency.load().0;
        let q = self.q.load().0;
        let gain_db = self.gain_db.load().0;
        if (freq - self.coeffs.last_freq).abs() > 0.01
            || (q - self.coeffs.last_q).abs() > 0.0001
            || (gain_db - self.coeffs.last_gain_db).abs() > 0.01
        {
            self.update_coefficients(freq, q, gain_db);
        }
    }

    /// Recompute coefficients from per-sample effective freq/Q (audio-rate
    /// modulation path). Reuses the same `maybe_update`-style change guard so a
    /// held modulation value doesn't recompute every sample needlessly.
    #[inline]
    fn maybe_update_modulated(&mut self, freq: f32, q: f32) {
        let gain_db = self.gain_db.load().0;
        if (freq - self.coeffs.last_freq).abs() > 0.01
            || (q - self.coeffs.last_q).abs() > 0.0001
            || (gain_db - self.coeffs.last_gain_db).abs() > 0.01
        {
            self.update_coefficients(freq, q, gain_db);
        }
    }
}

impl<F: Real + 'static> AudioUnit for StereoSvfFilterNode<F> {
    fn inputs(&self) -> usize {
        2 + self.mod_cutoff as usize + self.mod_q as usize
    }

    fn outputs(&self) -> usize {
        2
    }

    fn reset(&mut self) {
        self.left.reset_z();
        self.right.reset_z();
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        self.sample_rate = sample_rate;
        self.coeffs.invalidate();
    }

    #[inline]
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        // Effective cutoff/Q: a present param-input port overrides the atomic
        // (the atomic carries the base, fed upstream into the param sum).
        match (self.cutoff_port(), self.q_port()) {
            (None, None) => self.maybe_update(),
            (cp, qp) => {
                let freq = cp.map_or_else(|| self.frequency.load().0, |p| input[p].max(1.0));
                let q = qp.map_or_else(|| self.q.load().0, |p| input[p].max(0.01));
                self.maybe_update_modulated(freq, q);
            }
        }
        let l = self.left.tick(&self.coeffs, F::from_f32(input[0])).to_f32();
        let r = self
            .right
            .tick(&self.coeffs, F::from_f32(input[1]))
            .to_f32();
        output[0] = l;
        output[1] = r;
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        let cutoff_port = self.cutoff_port();
        let q_port = self.q_port();
        // Fast path: no param ports — block-rate coeff update, bit-identical to
        // before.
        if cutoff_port.is_none() && q_port.is_none() {
            self.maybe_update();
            for i in 0..size {
                let l_in = F::from_f32(input.at_f32(0, i));
                let r_in = F::from_f32(input.at_f32(1, i));
                let l = self.left.tick(&self.coeffs, l_in).to_f32();
                let r = self.right.tick(&self.coeffs, r_in).to_f32();
                output.set_f32(0, i, l);
                output.set_f32(1, i, r);
            }
            return;
        }
        // Modulated path: read the active port(s) per sample and recompute the
        // (channel-shared) coeffs before ticking both channels.
        let base_freq = self.frequency.load().0;
        let base_q = self.q.load().0;
        for i in 0..size {
            let freq = cutoff_port.map_or(base_freq, |p| input.at_f32(p, i).max(1.0));
            let q = q_port.map_or(base_q, |p| input.at_f32(p, i).max(0.01));
            self.maybe_update_modulated(freq, q);
            let l_in = F::from_f32(input.at_f32(0, i));
            let r_in = F::from_f32(input.at_f32(1, i));
            let l = self.left.tick(&self.coeffs, l_in).to_f32();
            let r = self.right.tick(&self.coeffs, r_in).to_f32();
            output.set_f32(0, i, l);
            output.set_f32(1, i, r);
        }
    }

    fn set(&mut self, setting: tutti_core::dsp::Setting) {
        if let Some((param, value)) = tutti_core::unit_param::from_setting(&setting) {
            match param {
                tutti_core::UnitParam::Cutoff => self.set_frequency(value),
                tutti_core::UnitParam::Q => self.set_q(value),
                tutti_core::UnitParam::GainDb => self.set_gain_db(value),
                _ => {} // not a param this unit owns — ignore
            }
        }
    }

    fn get_id(&self) -> u64 {
        // Distinct from mono SVF — same family, different shape.
        crate::node_id::SVF_FILTER_ID ^ 0xDA02
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let mut out = SignalFrame::new(2);
        out.set(0, input.at(0).filter(0.0, |z| z));
        out.set(1, input.at(1).filter(0.0, |z| z));
        out
    }

    fn footprint(&self) -> usize {
        core::mem::size_of::<Self>()
    }
}

impl<F: Real> Clone for StereoSvfFilterNode<F> {
    fn clone(&self) -> Self {
        Self {
            filter_type: self.filter_type,
            frequency: self.frequency.handle(),
            q: self.q.handle(),
            gain_db: self.gain_db.handle(),
            sample_rate: self.sample_rate,
            coeffs: self.coeffs.clone(),
            left: self.left.clone(),
            right: self.right.clone(),
            mod_cutoff: self.mod_cutoff,
            mod_q: self.mod_q,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::test_utils::{generate_sine, make_impulse, process_mono, rms};

    #[test]
    fn test_svf_lowpass_attenuates_high_freq() {
        let mut filter = SvfFilterNode::<f64>::new(SvfType::LowPass, 500.0, 0.707);
        filter.set_sample_rate(tutti_core::SampleRate(44100.0));

        let low = generate_sine(100.0, 44100.0, 4096);
        let high = generate_sine(5000.0, 44100.0, 4096);

        let out_low = process_mono(&mut filter, &low);
        filter.reset();
        let out_high = process_mono(&mut filter, &high);

        let rms_low = rms(&out_low[512..]);
        let rms_high = rms(&out_high[512..]);

        assert!(
            rms_low > rms_high * 3.0,
            "LP should pass low freq ({rms_low}) much more than high ({rms_high})"
        );
    }

    #[test]
    fn test_svf_highpass_attenuates_low_freq() {
        let mut filter = SvfFilterNode::<f64>::new(SvfType::HighPass, 2000.0, 0.707);
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
            "HP should pass high freq ({rms_high}) much more than low ({rms_low})"
        );
    }

    #[test]
    fn test_svf_bandpass() {
        let mut filter = SvfFilterNode::<f64>::new(SvfType::BandPass, 1000.0, 5.0);
        filter.set_sample_rate(tutti_core::SampleRate(44100.0));

        let on_freq = generate_sine(1000.0, 44100.0, 4096);
        let off_freq = generate_sine(100.0, 44100.0, 4096);

        let out_on = process_mono(&mut filter, &on_freq);
        filter.reset();
        let out_off = process_mono(&mut filter, &off_freq);

        let rms_on = rms(&out_on[512..]);
        let rms_off = rms(&out_off[512..]);

        assert!(
            rms_on > rms_off * 2.0,
            "BP should favor center freq ({rms_on}) over off-freq ({rms_off})"
        );
    }

    #[test]
    fn test_svf_notch() {
        let mut filter = SvfFilterNode::<f64>::new(SvfType::Notch, 1000.0, 5.0);
        filter.set_sample_rate(tutti_core::SampleRate(44100.0));

        let on_freq = generate_sine(1000.0, 44100.0, 4096);
        let off_freq = generate_sine(5000.0, 44100.0, 4096);

        let out_on = process_mono(&mut filter, &on_freq);
        filter.reset();
        let out_off = process_mono(&mut filter, &off_freq);

        let rms_on = rms(&out_on[512..]);
        let rms_off = rms(&out_off[512..]);

        assert!(
            rms_off > rms_on * 2.0,
            "Notch should reject center ({rms_on}) vs off-freq ({rms_off})"
        );
    }

    #[test]
    fn test_svf_allpass_flat_magnitude() {
        // An allpass passes all frequencies at ~unity magnitude (it only shifts
        // phase). RMS out should track RMS in at both a low and a high tone,
        // unlike a notch which would attenuate near its centre.
        let run = |hz: f32| -> (f32, f32) {
            let mut f = SvfFilterNode::<f64>::new(SvfType::Allpass, 1000.0, 0.707);
            f.set_sample_rate(tutti_core::SampleRate(44100.0));
            let sig = generate_sine(hz, 44100.0, 8192);
            let out = process_mono(&mut f, &sig);
            (rms(&sig[1024..]), rms(&out[1024..]))
        };
        for hz in [200.0, 1000.0, 5000.0] {
            let (rin, rout) = run(hz);
            let rel = (rout - rin).abs() / rin.max(1e-6);
            assert!(
                rel < 0.05,
                "allpass should preserve magnitude at {hz} Hz: in={rin}, out={rout} (rel {rel})"
            );
        }
    }

    #[test]
    fn test_svf_impulse_response_finite() {
        let mut filter = SvfFilterNode::<f64>::new(SvfType::LowPass, 1000.0, 0.707);
        filter.set_sample_rate(tutti_core::SampleRate(44100.0));

        let impulse = make_impulse(1024);
        let ir = process_mono(&mut filter, &impulse);

        let first_half_energy: f32 = ir[..512].iter().map(|s| s * s).sum();
        let second_half_energy: f32 = ir[512..].iter().map(|s| s * s).sum();
        assert!(
            first_half_energy > second_half_energy,
            "IR should decay: first_half={first_half_energy}, second_half={second_half_energy}"
        );
    }

    #[test]
    fn test_svf_reset() {
        let mut filter = SvfFilterNode::<f64>::new(SvfType::LowPass, 1000.0, 0.707);
        filter.set_sample_rate(tutti_core::SampleRate(44100.0));

        let sine = generate_sine(100.0, 44100.0, 100);
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
    fn test_svf_set_via_unit_param() {
        // The generic param path: a UnitParam Setting flows through AudioUnit::set
        // and lands in the same atomic the bespoke setter writes.
        use tutti_core::unit_param;
        use tutti_core::{AudioUnit, UnitParam};
        let mut node = StereoSvfFilterNode::<f64>::new(SvfType::LowPass, 1000.0, 0.707);
        node.set(unit_param::setting(UnitParam::Cutoff, 5000.0));
        node.set(unit_param::setting(UnitParam::Q, 2.5));
        assert!(
            (node.frequency().load(std::sync::atomic::Ordering::Acquire) - 5000.0).abs() < 1e-3
        );
        assert!((node.q().load(std::sync::atomic::Ordering::Acquire) - 2.5).abs() < 1e-3);
        // An unknown-to-this-unit param is a silent no-op (no panic).
        node.set(unit_param::setting(UnitParam::Wet, 0.5));
    }

    #[test]
    fn test_svf_parameter_modulation() {
        let mut filter = SvfFilterNode::<f64>::new(SvfType::LowPass, 500.0, 0.707);
        filter.set_sample_rate(tutti_core::SampleRate(44100.0));

        let noise: Vec<f32> = (0..1000)
            .map(|i| ((i * 7 + 3) % 100) as f32 / 50.0 - 1.0)
            .collect();

        let out1 = process_mono(&mut filter, &noise);
        filter.reset();

        filter.set_frequency(5000.0);
        let out2 = process_mono(&mut filter, &noise);

        let rms1 = rms(&out1[100..]);
        let rms2 = rms(&out2[100..]);

        assert!(
            rms2 > rms1,
            "Higher cutoff should pass more signal: low_cutoff={rms1}, high_cutoff={rms2}"
        );
    }

    #[test]
    fn test_svf_f32_state_matches_f64_for_mid_cutoff() {
        let mut filter_f64 = SvfFilterNode::<f64>::new(SvfType::LowPass, 1000.0, 0.707);
        let mut filter_f32 = SvfFilterNode::<f32>::new(SvfType::LowPass, 1000.0, 0.707);
        filter_f64.set_sample_rate(tutti_core::SampleRate(44100.0));
        filter_f32.set_sample_rate(tutti_core::SampleRate(44100.0));

        let input = generate_sine(500.0, 44100.0, 2048);
        let out_f64 = process_mono(&mut filter_f64, &input);
        let out_f32 = process_mono(&mut filter_f32, &input);

        let rms_f64 = rms(&out_f64[512..]);
        let rms_f32 = rms(&out_f32[512..]);
        let rel_diff = (rms_f64 - rms_f32).abs() / rms_f64.max(1e-6);
        assert!(
            rel_diff < 0.01,
            "f32 and f64 SVF should agree within 1% at mid cutoff: f64={rms_f64}, f32={rms_f32}"
        );
    }

    #[test]
    fn test_svf_coeffs_lowpass_m_values() {
        let c = compute_svf_coeffs(SvfType::LowPass, 1000.0, 0.707, 0.0, 44100.0);
        assert!((c.m0 - 0.0).abs() < 1e-10);
        assert!((c.m1 - 0.0).abs() < 1e-10);
        assert!((c.m2 - 1.0).abs() < 1e-10);
        assert!(c.a1 > 0.0 && c.a1 < 1.0);
    }

    #[test]
    fn test_svf_coeffs_highpass_m_values() {
        let c = compute_svf_coeffs(SvfType::HighPass, 1000.0, 0.707, 0.0, 44100.0);
        assert!((c.m0 - 1.0).abs() < 1e-10);
        assert!((c.m2 - (-1.0)).abs() < 1e-10);
    }

    #[test]
    fn test_svf_coeffs_bell_zero_gain_is_unity() {
        let c = compute_svf_coeffs(SvfType::Bell, 1000.0, 1.0, 0.0, 44100.0);
        assert!(
            (c.m1).abs() < 1e-10,
            "Bell with 0dB gain should have m1≈0, got {}",
            c.m1
        );
    }

    #[test]
    fn test_svf_coeffs_deterministic() {
        let c1 = compute_svf_coeffs(SvfType::LowPass, 500.0, 0.707, 0.0, 44100.0);
        let c2 = compute_svf_coeffs(SvfType::LowPass, 500.0, 0.707, 0.0, 44100.0);
        assert_eq!(c1.a1, c2.a1);
        assert_eq!(c1.a2, c2.a2);
        assert_eq!(c1.m0, c2.m0);
    }

    // =========================================================================
    // Stereo SVF
    // =========================================================================

    fn process_stereo(node: &mut dyn AudioUnit, l: &[f32], r: &[f32]) -> (Vec<f32>, Vec<f32>) {
        assert_eq!(l.len(), r.len());
        let mut out_l = vec![0.0f32; l.len()];
        let mut out_r = vec![0.0f32; r.len()];
        for i in 0..l.len() {
            let input = [l[i], r[i]];
            let mut output = [0.0f32; 2];
            node.tick(&input, &mut output);
            out_l[i] = output[0];
            out_r[i] = output[1];
        }
        (out_l, out_r)
    }

    #[test]
    fn test_stereo_svf_matches_mono_per_channel() {
        // Identical signal on L+R must match what a mono SVF would produce.
        let mut mono = SvfFilterNode::<f64>::new(SvfType::LowPass, 1000.0, 0.707);
        mono.set_sample_rate(tutti_core::SampleRate(44100.0));

        let mut stereo = StereoSvfFilterNode::<f64>::new(SvfType::LowPass, 1000.0, 0.707);
        stereo.set_sample_rate(tutti_core::SampleRate(44100.0));

        let signal = generate_sine(440.0, 44100.0, 2048);
        let mono_out = process_mono(&mut mono, &signal);
        let (l_out, r_out) = process_stereo(&mut stereo, &signal, &signal);

        for i in 0..signal.len() {
            assert!(
                (mono_out[i] - l_out[i]).abs() < 1e-5,
                "L channel diverges from mono at sample {i}"
            );
            assert!(
                (mono_out[i] - r_out[i]).abs() < 1e-5,
                "R channel diverges from mono at sample {i}"
            );
        }
    }

    #[test]
    fn test_stereo_svf_per_channel_independence() {
        // Different signals on L vs R should not bleed across channels.
        let mut stereo = StereoSvfFilterNode::<f64>::new(SvfType::LowPass, 1000.0, 0.707);
        stereo.set_sample_rate(tutti_core::SampleRate(44100.0));

        let l_in = generate_sine(200.0, 44100.0, 2048);
        let r_in = vec![0.0f32; 2048];
        let (l_out, r_out) = process_stereo(&mut stereo, &l_in, &r_in);

        let r_energy: f32 = r_out.iter().map(|s| s * s).sum();
        let l_energy: f32 = l_out.iter().map(|s| s * s).sum();
        assert!(
            r_energy < 1e-10,
            "R should stay silent when only L has input; got energy {r_energy}"
        );
        assert!(l_energy > 1.0, "L should carry meaningful signal");
    }

    #[test]
    fn test_stereo_svf_reset_clears_both_channels() {
        let mut stereo = StereoSvfFilterNode::<f64>::new(SvfType::LowPass, 1000.0, 0.707);
        stereo.set_sample_rate(tutti_core::SampleRate(44100.0));

        let signal = generate_sine(100.0, 44100.0, 200);
        let _ = process_stereo(&mut stereo, &signal, &signal);
        stereo.reset();

        let mut out = [0.0f32; 2];
        stereo.tick(&[0.0, 0.0], &mut out);
        assert!(out[0].abs() < 1e-4 && out[1].abs() < 1e-4);
    }

    // ── Audio-rate param-input ports ─────────────────────────────────────────

    #[test]
    fn stereo_svf_default_is_two_in_no_param_ports() {
        let u = StereoSvfFilterNode::<f64>::new(SvfType::LowPass, 1000.0, 0.707);
        assert_eq!(u.inputs(), 2);
        assert_eq!(u.outputs(), 2);
        assert_eq!(u.cutoff_port(), None);
        assert_eq!(u.q_port(), None);
    }

    #[test]
    fn stereo_svf_param_port_arity_and_indices() {
        // cutoff only → port 2.
        let c = StereoSvfFilterNode::<f64>::with_param_inputs(
            SvfType::LowPass,
            1000.0,
            0.7,
            true,
            false,
        );
        assert_eq!(c.inputs(), 3);
        assert_eq!(c.cutoff_port(), Some(2));
        assert_eq!(c.q_port(), None);
        // Q only → port 2 (no cutoff port before it).
        let q = StereoSvfFilterNode::<f64>::with_param_inputs(
            SvfType::LowPass,
            1000.0,
            0.7,
            false,
            true,
        );
        assert_eq!(q.inputs(), 3);
        assert_eq!(q.cutoff_port(), None);
        assert_eq!(q.q_port(), Some(2));
        // both → cutoff at 2, Q at 3.
        let b = StereoSvfFilterNode::<f64>::with_param_inputs(
            SvfType::LowPass,
            1000.0,
            0.7,
            true,
            true,
        );
        assert_eq!(b.inputs(), 4);
        assert_eq!(b.cutoff_port(), Some(2));
        assert_eq!(b.q_port(), Some(3));
    }

    #[test]
    fn stereo_svf_cutoff_port_modulates_response() {
        // Same noise through a low-pass: a cutoff port held high should pass
        // more energy than the same node with the port held low. Proves the
        // port actually drives the coefficients per buffer.
        let noise: Vec<f32> = (0..2048)
            .map(|i| ((i * 7 + 3) % 100) as f32 / 50.0 - 1.0)
            .collect();

        let run = |cutoff: f32| -> f32 {
            let mut f = StereoSvfFilterNode::<f64>::with_param_inputs(
                SvfType::LowPass,
                200.0,
                0.707,
                true,
                false,
            );
            f.set_sample_rate(tutti_core::SampleRate(44100.0));
            // 3-in/2-out: feed the cutoff on port 2.
            let mut out_l = vec![0.0f32; noise.len()];
            for i in 0..noise.len() {
                let input = [noise[i], noise[i], cutoff];
                let mut out = [0.0f32; 2];
                f.tick(&input, &mut out);
                out_l[i] = out[0];
            }
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
    fn stereo_svf_unmodulated_process_matches_modulated_held_constant() {
        // A modulated node whose cutoff port is held at the same value as an
        // unmodulated node's atomic must produce bit-identical output — the
        // modulated path is a faithful superset.
        let signal = generate_sine(440.0, 44100.0, 1024);

        let mut plain = StereoSvfFilterNode::<f64>::new(SvfType::LowPass, 1000.0, 0.707);
        plain.set_sample_rate(tutti_core::SampleRate(44100.0));
        let (plain_l, _) = process_stereo(&mut plain, &signal, &signal);

        let mut modn = StereoSvfFilterNode::<f64>::with_param_inputs(
            SvfType::LowPass,
            1000.0,
            0.707,
            true,
            false,
        );
        modn.set_sample_rate(tutti_core::SampleRate(44100.0));
        let mut mod_l = vec![0.0f32; signal.len()];
        for i in 0..signal.len() {
            let input = [signal[i], signal[i], 1000.0]; // cutoff held at the atomic value
            let mut out = [0.0f32; 2];
            modn.tick(&input, &mut out);
            mod_l[i] = out[0];
        }
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
