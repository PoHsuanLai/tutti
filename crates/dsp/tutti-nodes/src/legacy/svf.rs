use tutti_core::Arc;
use tutti_core::AtomicF32;
use tutti_core::{AudioUnit, BufferMut, BufferRef, Real, SignalFrame};

use tutti_core::{Db, Hz, Param, SampleRate, Q};

/// Below these deltas a freq/Q/gain change doesn't warrant recomputing the
/// coefficients — the change guard shared by the atomic and modulation paths.
const FREQ_EPS: f32 = 0.01;
const Q_EPS: f32 = 0.0001;
const GAIN_EPS: f32 = 0.01;

pub(crate) use crate::SvfType;

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
///
/// The three tuning parameters are three different units, and taking them as
/// [`Hz`] / [`Q`] / [`Db`] is what keeps them apart. As bare `f32`s they are
/// adjacent and interchangeable — the call reads as a run of positional numbers,
/// `(LowPass, 1000.0, 0.707, 0.0, 44100.0)`, where transposing any two compiles
/// and detunes the filter.
pub(super) fn compute_svf_coeffs(
    filter_type: SvfType,
    freq: impl Into<Hz>,
    q: impl Into<Q>,
    gain_db: impl Into<Db>,
    sample_rate: impl Into<SampleRate>,
) -> SvfCoeffs {
    let (freq, q, gain_db) = (freq.into().get(), q.into().get(), gain_db.into().get());
    let sample_rate = sample_rate.into();
    // `tan` diverges at Nyquist itself, so the cutoff stops just short of it.
    let fc = (freq as f64).clamp(1.0, f64::from(sample_rate.nyquist_scaled(0.998).get()));
    let g = (core::f64::consts::PI * fc / sample_rate.get()).tan();
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

/// Parameter-derived filter coefficients, plus the last-seen parameter values
/// that gate recomputation.
///
/// Shared across channels in the multi-channel variants — the params are one
/// linked control surface, so widening replicates only `SvfIntegrator`.
#[derive(Clone)]
struct SvfCoefficients<F: Real> {
    a1: F,
    a2: F,
    a3: F,
    m0: F,
    m1: F,
    m2: F,
    last_freq: Hz,
    last_q: Q,
    last_gain_db: Db,
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
            last_freq: Hz(-1.0),
            last_q: Q(-1.0),
            last_gain_db: Db(f32::NAN),
        }
    }

    fn invalidate(&mut self) {
        self.last_freq = Hz(-1.0);
    }

    fn store(&mut self, c: SvfCoeffs, freq: Hz, q: Q, gain_db: Db) {
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

/// Mono state-variable filter: 1 input, 1 output.
///
/// Cutoff ([`Hz`]), [`Q`] and gain ([`Db`]) are live [`Param`]s shared across
/// clones. All three are read **once per block** and the coefficients are
/// recomputed only when one moves past a small epsilon, so a held value costs
/// nothing. For sample-accurate cutoff modulation use
/// [`StereoSvfFilterNode`]'s param-input ports.
///
/// Gain applies only to [`Bell`](SvfType::Bell),
/// [`LowShelf`](SvfType::LowShelf) and [`HighShelf`](SvfType::HighShelf); the
/// other types ignore it.
///
/// `F` is the internal state precision. It defaults to `f64` for accuracy at
/// low cutoffs, where an `f32` integrator loses resolution in the feedback
/// path; `SvfFilterNode::<f32>::new(…)` trades that for CPU.
pub struct SvfFilterNode<F: Real = f64> {
    filter_type: SvfType,
    frequency: Param<Hz>,
    q: Param<Q>,
    gain_db: Param<Db>,
    sample_rate: SampleRate,
    coeffs: SvfCoefficients<F>,
    integrator: SvfIntegrator<F>,
}

impl<F: Real> SvfFilterNode<F> {
    /// Builds a filter of `filter_type` at `frequency` cutoff and resonance
    /// `q`, with 0 dB gain.
    ///
    /// `q` around `0.707` is the flattest (Butterworth) response; higher values
    /// resonate at the cutoff, and a band-pass or notch narrows as it rises.
    ///
    /// Coefficients are computed here, but **against the placeholder
    /// [`SampleRate::DEFAULT`]** — a cutoff only means anything relative to
    /// Nyquist, and the device rate is not known yet. Call
    /// [`AudioUnit::set_sample_rate`] before the first `process`; it recomputes
    /// them. Skip it at 48 kHz and the corner sits 8.8% high (a 1 kHz low-pass
    /// cuts at 1088 Hz) — a filter that still filters, just not where it was
    /// asked to. See the crate-level "born at a placeholder rate" section.
    ///
    /// [`SampleRate::DEFAULT`]: tutti_core::SampleRate::DEFAULT
    /// [`AudioUnit::set_sample_rate`]: tutti_core::AudioUnit::set_sample_rate
    pub fn new(filter_type: SvfType, frequency: impl Into<Hz>, q: impl Into<Q>) -> Self {
        let frequency = frequency.into();
        let q = q.into();
        let mut node = Self {
            filter_type,
            frequency: Param::new(frequency),
            q: Param::new(q),
            gain_db: Param::new(Db(0.0)),
            sample_rate: SampleRate::DEFAULT,
            coeffs: SvfCoefficients::zeroed(),
            integrator: SvfIntegrator::zeroed(),
        };
        node.update_coefficients(frequency, q, Db(0.0));
        node
    }

    /// Sets the shelf/bell gain in [`Db`], recomputing coefficients
    /// immediately.
    ///
    /// Only [`Bell`](SvfType::Bell), [`LowShelf`](SvfType::LowShelf) and
    /// [`HighShelf`](SvfType::HighShelf) read it; on the other types it is
    /// stored and ignored. Positive boosts, negative cuts, `0.0` is flat.
    pub fn with_gain_db(mut self, db: impl Into<Db>) -> Self {
        let db = db.into();
        self.gain_db = Param::new(db);
        self.update_coefficients(self.frequency.load(), self.q.load(), db);
        self
    }

    /// The shared cutoff cell in [`Hz`], for driving cutoff from a modulator.
    ///
    /// Read once per block. Writing the raw cell bypasses the 1 Hz floor
    /// [`set_frequency`](Self::set_frequency) applies; the coefficient
    /// computation clamps to `1.0..=0.998 * Nyquist` regardless, since `tan`
    /// diverges at Nyquist. Shared across clones.
    pub fn frequency(&self) -> Arc<AtomicF32> {
        self.frequency.as_atomic()
    }

    /// The shared resonance cell in [`Q`].
    ///
    /// Read once per block. Writing the raw cell bypasses
    /// [`set_q`](Self::set_q)'s clamp; the coefficient computation floors the
    /// value at `0.01` to avoid a division blow-up. Shared across clones.
    pub fn q(&self) -> Arc<AtomicF32> {
        self.q.as_atomic()
    }

    /// The shared shelf/bell gain cell in [`Db`]. Inert on the non-gain filter
    /// types.
    pub fn gain_db(&self) -> Arc<AtomicF32> {
        self.gain_db.as_atomic()
    }

    /// Sets the cutoff in [`Hz`], floored at 1 Hz.
    ///
    /// The upper bound is applied when coefficients are computed, at `0.998` of
    /// Nyquist — so this accepts a higher value and the filter tops out there.
    pub fn set_frequency(&self, hz: impl Into<Hz>) {
        self.frequency.store(Hz(hz.into().get().max(1.0)));
    }

    /// Sets the resonance [`Q`], clamped to the type's valid range.
    ///
    /// `0.707` is the flattest response; higher resonates at the cutoff.
    pub fn set_q(&self, q: impl Into<Q>) {
        self.q.store(Q::new_clamped(q.into().get()));
    }

    /// Sets the shelf/bell gain in [`Db`]. Inert on the non-gain filter types.
    pub fn set_gain_db(&self, db: impl Into<Db>) {
        self.gain_db.store(db.into());
    }

    /// Switches the filter response, forcing a coefficient recompute on the
    /// next sample.
    ///
    /// `&mut self`, so this cannot reach a node already live in the graph —
    /// the response is a structural choice, not an automatable parameter.
    /// Integrator state is deliberately kept, so the switch is continuous
    /// rather than a click.
    pub fn set_filter_type(&mut self, filter_type: SvfType) {
        self.filter_type = filter_type;
        self.coeffs.invalidate();
    }

    fn update_coefficients(&mut self, freq: Hz, q: Q, gain_db: Db) {
        let c = compute_svf_coeffs(self.filter_type, freq, q, gain_db, self.sample_rate);
        self.coeffs.store(c, freq, q, gain_db);
    }

    #[inline]
    fn maybe_update(&mut self) {
        let freq = self.frequency.load();
        let q = self.q.load();
        let gain_db = self.gain_db.load();
        if (freq.get() - self.coeffs.last_freq.get()).abs() > FREQ_EPS
            || (q.get() - self.coeffs.last_q.get()).abs() > Q_EPS
            || (gain_db.get() - self.coeffs.last_gain_db.get()).abs() > GAIN_EPS
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

    fn set(&mut self, setting: tutti_core::Setting) {
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
/// after the audio inputs (see [`Self::with_param_inputs`]): a cutoff port in
/// [`Hz`], then a [`Q`] port, each present only if its `mod_*` flag was set.
///
/// A present param-input port **overrides** the corresponding atomic per
/// sample, forcing a coefficient recompute that sample — the block-rate skip
/// only applies to the unmodulated parameters, so a modulated filter is
/// materially more expensive. When neither flag is set the filter is a plain
/// 2-in/2-out node with zero added cost, which is the common case.
pub struct StereoSvfFilterNode<F: Real = f64> {
    filter_type: SvfType,
    frequency: Param<Hz>,
    q: Param<Q>,
    gain_db: Param<Db>,
    sample_rate: SampleRate,
    coeffs: SvfCoefficients<F>,
    /// Per-channel integrator state; `channels.len()` == audio width. The
    /// coefficients above are channel-shared (one linked control surface), so
    /// widening only replicates the integrator state, not the params. Built at
    /// construction — never resized in `tick`/`process` (RT no-alloc).
    channels: Vec<SvfIntegrator<F>>,
    /// When true, a cutoff param-input port follows the audio inputs and
    /// overrides the `frequency` atomic per sample.
    mod_cutoff: bool,
    /// When true, a Q param-input port follows the cutoff port (or the audio
    /// inputs if `mod_cutoff` is false) and overrides the `q` atomic per sample.
    mod_q: bool,
}

impl<F: Real> StereoSvfFilterNode<F> {
    /// Builds a stereo (width-2) filter with no param-input ports.
    ///
    /// Shorthand for [`with_channels(2, …)`](Self::with_channels). The two
    /// channels share one coefficient set and keep independent integrator
    /// state, so they filter identically without bleeding into each other.
    pub fn new(filter_type: SvfType, frequency: impl Into<Hz>, q: impl Into<Q>) -> Self {
        Self::with_channels(2, filter_type, frequency, q)
    }

    /// An `n`-channel filter (clamped to at least 1).
    ///
    /// The coefficients are channel-shared — one linked control surface across
    /// all channels — and only the per-channel integrator state is replicated,
    /// so widening costs state but not parameter work.
    ///
    /// Speaker placement is the upstream panner's job: this is a per-channel
    /// filter, not a spatial process.
    ///
    /// **Starts at the placeholder [`SampleRate::DEFAULT`]**; call
    /// [`AudioUnit::set_sample_rate`] before the first `process` or every
    /// channel's corner sits 8.8% high at 48 kHz. The coefficients are shared,
    /// so the skew is identical across the width — wrong everywhere rather than
    /// unbalanced, which is why widening does not make it any easier to hear.
    /// See the crate-level "born at a placeholder rate" section.
    ///
    /// [`SampleRate::DEFAULT`]: tutti_core::SampleRate::DEFAULT
    /// [`AudioUnit::set_sample_rate`]: tutti_core::AudioUnit::set_sample_rate
    pub fn with_channels(
        channels: usize,
        filter_type: SvfType,
        frequency: impl Into<Hz>,
        q: impl Into<Q>,
    ) -> Self {
        let frequency = frequency.into();
        let q = q.into();
        let n = channels.max(1);
        let mut node = Self {
            filter_type,
            frequency: Param::new(frequency),
            q: Param::new(q),
            gain_db: Param::new(Db(0.0)),
            sample_rate: SampleRate::DEFAULT,
            coeffs: SvfCoefficients::zeroed(),
            channels: vec![SvfIntegrator::zeroed(); n],
            mod_cutoff: false,
            mod_q: false,
        };
        node.update_coefficients(frequency, q, Db(0.0));
        node
    }

    /// A filter with optional audio-rate param-input ports. `mod_cutoff` /
    /// `mod_q` add a cutoff / Q param-input port after the audio inputs
    /// (cutoff first), each overriding its atomic per sample when present. The
    /// atomics still hold the base (they feed the upstream param-sum's base
    /// port), so the UI handle path is unchanged.
    ///
    /// Width and modulation are **independent axes**: `channels` says how wide
    /// the filter is, the `mod_*` flags say which params it reads at audio rate.
    /// Collapsing them — hardcoding width 2 in the modulated form — turns a
    /// request for a modulated 5.1 filter into a *stereo* one, and the only
    /// symptom is a `set_source` on a param port that resolves and carries the
    /// wrong signal.
    ///
    /// The param ports follow the audio inputs, so their indices **move with the
    /// width**. Ask [`ParamPorts::param_port`](crate::ParamPorts::param_port);
    /// never assume an index.
    pub fn with_param_inputs(
        channels: usize,
        filter_type: SvfType,
        frequency: impl Into<Hz>,
        q: impl Into<Q>,
        mod_cutoff: bool,
        mod_q: bool,
    ) -> Self {
        let mut node = Self::with_channels(channels, filter_type, frequency, q);
        node.mod_cutoff = mod_cutoff;
        node.mod_q = mod_q;
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

    /// Input-port index of the Q param input, if present (after the audio
    /// inputs and the cutoff port).
    #[inline]
    pub fn q_port(&self) -> Option<usize> {
        self.mod_q
            .then_some(self.width() + self.mod_cutoff as usize)
    }

    /// Sets the shelf/bell gain in [`Db`], recomputing coefficients
    /// immediately. Read only by the gain-bearing filter types.
    pub fn with_gain_db(mut self, db: impl Into<Db>) -> Self {
        let db = db.into();
        self.gain_db = Param::new(db);
        self.update_coefficients(self.frequency.load(), self.q.load(), db);
        self
    }

    /// The shared cutoff cell in [`Hz`], applied to every channel alike.
    ///
    /// **A present cutoff param-input port overrides this per sample**; without
    /// one it is read once per block. Shared across clones.
    pub fn frequency(&self) -> Arc<AtomicF32> {
        self.frequency.as_atomic()
    }

    /// The shared resonance cell in [`Q`], applied to every channel alike.
    ///
    /// **A present Q param-input port overrides this per sample**; without one
    /// it is read once per block. Shared across clones.
    pub fn q(&self) -> Arc<AtomicF32> {
        self.q.as_atomic()
    }

    /// The shared shelf/bell gain cell in [`Db`].
    ///
    /// Always read from the atomic — there is no gain param-input port. Inert
    /// on the non-gain filter types.
    pub fn gain_db(&self) -> Arc<AtomicF32> {
        self.gain_db.as_atomic()
    }

    /// Sets the cutoff in [`Hz`] for every channel, floored at 1 Hz.
    ///
    /// When a cutoff param-input port is present this sets the *base* the port
    /// overrides, not what the filter runs at.
    pub fn set_frequency(&self, hz: impl Into<Hz>) {
        self.frequency.store(Hz(hz.into().get().max(1.0)));
    }

    /// Sets the resonance [`Q`] for every channel, clamped to the valid range.
    ///
    /// When a Q param-input port is present this sets the *base* the port
    /// overrides.
    pub fn set_q(&self, q: impl Into<Q>) {
        self.q.store(Q::new_clamped(q.into().get()));
    }

    /// Sets the shelf/bell gain in [`Db`]. Inert on the non-gain filter types.
    pub fn set_gain_db(&self, db: impl Into<Db>) {
        self.gain_db.store(db.into());
    }

    /// Switches the filter response for every channel, forcing a coefficient
    /// recompute on the next sample.
    ///
    /// `&mut self`, so it cannot reach a node already live in the graph.
    /// Integrator state is kept, so the switch is continuous rather than a
    /// click.
    pub fn set_filter_type(&mut self, filter_type: SvfType) {
        self.filter_type = filter_type;
        self.coeffs.invalidate();
    }

    fn update_coefficients(&mut self, freq: Hz, q: Q, gain_db: Db) {
        let c = compute_svf_coeffs(self.filter_type, freq, q, gain_db, self.sample_rate);
        self.coeffs.store(c, freq, q, gain_db);
    }

    #[inline]
    fn maybe_update(&mut self) {
        let freq = self.frequency.load();
        let q = self.q.load();
        self.maybe_update_modulated(freq, q);
    }

    /// Recompute coefficients only when freq/Q/gain moved past a small epsilon,
    /// so a held value doesn't recompute every sample. Shared by the atomic
    /// ([`Self::maybe_update`]) and audio-rate modulation paths; gain always
    /// comes from its atomic.
    #[inline]
    fn maybe_update_modulated(&mut self, freq: Hz, q: Q) {
        let gain_db = self.gain_db.load();
        if (freq.get() - self.coeffs.last_freq.get()).abs() > FREQ_EPS
            || (q.get() - self.coeffs.last_q.get()).abs() > Q_EPS
            || (gain_db.get() - self.coeffs.last_gain_db.get()).abs() > GAIN_EPS
        {
            self.update_coefficients(freq, q, gain_db);
        }
    }
}

impl<F: Real + 'static> AudioUnit for StereoSvfFilterNode<F> {
    fn inputs(&self) -> usize {
        self.width() + self.mod_cutoff as usize + self.mod_q as usize
    }

    fn outputs(&self) -> usize {
        self.width()
    }

    fn reset(&mut self) {
        for ch in &mut self.channels {
            ch.reset_z();
        }
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
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
                let freq = cp.map_or_else(|| self.frequency.load(), |p| Hz(input[p].max(1.0)));
                let q = qp.map_or_else(|| self.q.load(), |p| Q(input[p].max(0.01)));
                self.maybe_update_modulated(freq, q);
            }
        }
        // Per-channel integrator, channel-shared coeffs.
        for (c, ch) in self.channels.iter_mut().enumerate() {
            output[c] = ch.tick(&self.coeffs, F::from_f32(input[c])).to_f32();
        }
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        let cutoff_port = self.cutoff_port();
        let q_port = self.q_port();
        // Fast path: no param ports — one block-rate coeff update.
        if cutoff_port.is_none() && q_port.is_none() {
            self.maybe_update();
            for i in 0..size {
                for (c, ch) in self.channels.iter_mut().enumerate() {
                    let x = F::from_f32(input.at_f32(c, i));
                    output.set_f32(c, i, ch.tick(&self.coeffs, x).to_f32());
                }
            }
            return;
        }
        // Modulated path: read the active port(s) per sample and recompute the
        // (channel-shared) coeffs before ticking every channel.
        let base_freq = self.frequency.load();
        let base_q = self.q.load();
        for i in 0..size {
            let freq = cutoff_port.map_or(base_freq, |p| Hz(input.at_f32(p, i).max(1.0)));
            let q = q_port.map_or(base_q, |p| Q(input.at_f32(p, i).max(0.01)));
            self.maybe_update_modulated(freq, q);
            for (c, ch) in self.channels.iter_mut().enumerate() {
                let x = F::from_f32(input.at_f32(c, i));
                output.set_f32(c, i, ch.tick(&self.coeffs, x).to_f32());
            }
        }
    }

    fn set(&mut self, setting: tutti_core::Setting) {
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
        let mut out = SignalFrame::new(self.width());
        for c in 0..self.width() {
            out.set(c, input.at(c).filter(0.0, |z| z));
        }
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
            channels: self.channels.clone(),
            mod_cutoff: self.mod_cutoff,
            mod_q: self.mod_q,
        }
    }
}
