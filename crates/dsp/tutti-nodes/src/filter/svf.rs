//! State-variable filter — the crate's general-purpose filter.
//!
//! One coefficient computation serves eight responses ([`SvfType`]), so
//! switching type is as cheap as any parameter change. Mono, and a width-native
//! variant that shares coefficients across channels while keeping per-channel
//! integrator state.

use tutti_core::Arc;
use tutti_core::AtomicF32;
use tutti_core::{
    dsp::{Real, DEFAULT_SAMPLE_RATE},
    AudioUnit, BufferMut, BufferRef, SignalFrame,
};

use tutti_core::{Db, Hz, Param, SampleRate, Q};

/// Below these deltas a freq/Q/gain change doesn't warrant recomputing the
/// coefficients — the change guard shared by the atomic and modulation paths.
const FREQ_EPS: f32 = 0.01;
const Q_EPS: f32 = 0.0001;
const GAIN_EPS: f32 = 0.01;

/// Which response the state-variable filter presents.
///
/// All eight share one coefficient computation and differ only in the output
/// mix (`m0`/`m1`/`m2`), so switching type is as cheap as any other parameter
/// change. The last three ([`Bell`](Self::Bell), [`LowShelf`](Self::LowShelf),
/// [`HighShelf`](Self::HighShelf)) are the only ones that read the gain
/// parameter; the rest ignore it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SvfType {
    /// Passes below cutoff, attenuates above it at 12 dB/octave. The default.
    #[default]
    LowPass,
    /// Passes above cutoff, attenuates below it at 12 dB/octave.
    HighPass,
    /// Passes a band around cutoff; [`Q`] sets how narrow.
    BandPass,
    /// Rejects a band around cutoff and passes everything else — the inverse of
    /// [`BandPass`](Self::BandPass). High [`Q`] gives a narrow, deep notch.
    Notch,
    /// Allpass: flat magnitude, frequency-dependent phase shift.
    ///
    /// Passes every frequency at unity and changes only phase, which is what
    /// makes it the building block of a phaser.
    Allpass,
    /// Peaking/bell EQ: boosts or cuts a band around cutoff by the gain
    /// parameter, leaving the rest flat. [`Q`] sets the bandwidth.
    Bell,
    /// Shelf boosting or cutting everything *below* cutoff by the gain
    /// parameter, flat above it.
    LowShelf,
    /// Shelf boosting or cutting everything *above* cutoff by the gain
    /// parameter, flat below it.
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
    /// [`DEFAULT_SAMPLE_RATE`]** — a cutoff only means anything relative to
    /// Nyquist, and the device rate is not known yet. Call
    /// [`AudioUnit::set_sample_rate`] before the first `process`; it recomputes
    /// them. Skip it at 48 kHz and the corner sits 8.8% high (a 1 kHz low-pass
    /// cuts at 1088 Hz) — a filter that still filters, just not where it was
    /// asked to. See the crate-level "born at a placeholder rate" section.
    ///
    /// [`DEFAULT_SAMPLE_RATE`]: tutti_core::dsp::DEFAULT_SAMPLE_RATE
    /// [`AudioUnit::set_sample_rate`]: tutti_core::AudioUnit::set_sample_rate
    pub fn new(filter_type: SvfType, frequency: impl Into<Hz>, q: impl Into<Q>) -> Self {
        let frequency = frequency.into();
        let q = q.into();
        let mut node = Self {
            filter_type,
            frequency: Param::new(frequency),
            q: Param::new(q),
            gain_db: Param::new(Db(0.0)),
            sample_rate: DEFAULT_SAMPLE_RATE,
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
    /// **Starts at the placeholder [`DEFAULT_SAMPLE_RATE`]**; call
    /// [`AudioUnit::set_sample_rate`] before the first `process` or every
    /// channel's corner sits 8.8% high at 48 kHz. The coefficients are shared,
    /// so the skew is identical across the width — wrong everywhere rather than
    /// unbalanced, which is why widening does not make it any easier to hear.
    /// See the crate-level "born at a placeholder rate" section.
    ///
    /// [`DEFAULT_SAMPLE_RATE`]: tutti_core::dsp::DEFAULT_SAMPLE_RATE
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
            sample_rate: DEFAULT_SAMPLE_RATE,
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

    /// Cutoff and Q are not interchangeable, and the types are what say so.
    ///
    /// As adjacent `f32`s, `(freq, q)` and `(q, freq)` both compile and the
    /// second silently detunes the filter. Passing them as `Hz` and `Q` makes
    /// the transposition a type error — this pins the difference the swap would
    /// make, so the two are never quietly given one type again.
    #[test]
    fn cutoff_and_q_are_not_interchangeable() {
        let right = compute_svf_coeffs(SvfType::LowPass, Hz(1000.0), Q(0.707), Db(0.0), 44100.0);
        // What the transposed call computes: a 0.707 Hz cutoff at Q 1000.
        let swapped = compute_svf_coeffs(SvfType::LowPass, Hz(0.707), Q(1000.0), Db(0.0), 44100.0);
        assert!(
            (right.a2 - swapped.a2).abs() > 1e-6,
            "the swap has to be observable, or this test proves nothing"
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

    // ── Width-native (N-channel) ─────────────────────────────────────────────

    fn process_wide(node: &mut dyn AudioUnit, frames: &[Vec<f32>]) -> Vec<Vec<f32>> {
        let ch = node.inputs();
        let len = frames[0].len();
        let mut out = vec![vec![0.0f32; len]; ch];
        let mut inbuf = vec![0.0f32; ch];
        let mut outbuf = vec![0.0f32; ch];
        for i in 0..len {
            for (c, f) in frames.iter().enumerate() {
                inbuf[c] = f[i];
            }
            node.tick(&inbuf, &mut outbuf);
            for c in 0..ch {
                out[c][i] = outbuf[c];
            }
        }
        out
    }

    #[test]
    fn with_channels_2_is_bit_identical_to_new() {
        // The stereo fast path must be preserved exactly.
        let mut a = StereoSvfFilterNode::<f64>::new(SvfType::LowPass, 1200.0, 0.8);
        a.set_sample_rate(tutti_core::SampleRate(44100.0));
        let mut b = StereoSvfFilterNode::<f64>::with_channels(2, SvfType::LowPass, 1200.0, 0.8);
        b.set_sample_rate(tutti_core::SampleRate(44100.0));

        let sig = generate_sine(440.0, 44100.0, 1024);
        let (al, ar) = process_stereo(&mut a, &sig, &sig);
        let (bl, br) = process_stereo(&mut b, &sig, &sig);
        for i in 0..sig.len() {
            assert_eq!(al[i].to_bits(), bl[i].to_bits(), "L bit-diff at {i}");
            assert_eq!(ar[i].to_bits(), br[i].to_bits(), "R bit-diff at {i}");
        }
    }

    #[test]
    fn with_channels_reports_arity() {
        let f = StereoSvfFilterNode::<f64>::with_channels(6, SvfType::HighPass, 800.0, 0.707);
        assert_eq!(f.inputs(), 6);
        assert_eq!(f.outputs(), 6);
    }

    #[test]
    fn wide_channel_matches_mono_and_is_independent() {
        // Each of 6 channels must filter exactly like a mono SVF with the same
        // coeffs, and carry only its own input (no cross-channel bleed).
        let mut mono = SvfFilterNode::<f64>::new(SvfType::LowPass, 1000.0, 0.707);
        mono.set_sample_rate(tutti_core::SampleRate(44100.0));
        let mut wide =
            StereoSvfFilterNode::<f64>::with_channels(6, SvfType::LowPass, 1000.0, 0.707);
        wide.set_sample_rate(tutti_core::SampleRate(44100.0));

        // Drive only channel 4; the rest are silent.
        let sig = generate_sine(300.0, 44100.0, 2048);
        let mut frames: Vec<Vec<f32>> = (0..6).map(|_| vec![0.0f32; sig.len()]).collect();
        frames[4] = sig.clone();
        let out = process_wide(&mut wide, &frames);

        let mono_out = process_mono(&mut mono, &sig);
        for i in 0..sig.len() {
            assert!(
                (mono_out[i] - out[4][i]).abs() < 1e-5,
                "ch4 diverges from mono at {i}"
            );
        }
        for c in [0usize, 1, 2, 3, 5] {
            let e: f32 = out[c].iter().map(|s| s * s).sum();
            assert!(e < 1e-10, "ch{c} should stay silent; energy {e}");
        }
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
            2,
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
            2,
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
            2,
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
                2,
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
            2,
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

    /// Width and modulation are independent axes.
    ///
    /// Hardcoding `vec![SvfIntegrator::zeroed(); 2]` in the modulated
    /// constructor makes a 6-channel request come back *stereo*; the arity
    /// assertion is what catches it.
    #[test]
    fn a_modulated_filter_is_as_wide_as_it_was_asked_for() {
        let f = StereoSvfFilterNode::<f64>::with_param_inputs(
            6,
            SvfType::LowPass,
            1000.0,
            0.7,
            true,
            true,
        );
        assert_eq!(f.outputs(), 6, "the width is what was asked for");
        assert_eq!(f.inputs(), 8, "six audio inputs, then cutoff and Q");
        assert_eq!(
            f.cutoff_port(),
            Some(6),
            "param ports follow the audio inputs"
        );
        assert_eq!(f.q_port(), Some(7), "and keep their documented order");
    }

    /// The modulated constructor must be the unmodulated one plus flags.
    ///
    /// Written as a *duplicated struct literal* it becomes a second
    /// initialisation path that can drift from `with_channels` field by field.
    /// Ticking both is what catches a drift arity alone would miss:
    /// coefficients computed from a different gain, or an uninvalidated cache,
    /// still report 6 outputs.
    #[test]
    fn a_modulated_filter_ticks_identically_to_its_unmodulated_twin() {
        let mut plain = StereoSvfFilterNode::<f64>::with_channels(6, SvfType::LowPass, 1000.0, 0.7);
        // Both flags off — the same filter, built through the other path.
        let mut ported = StereoSvfFilterNode::<f64>::with_param_inputs(
            6,
            SvfType::LowPass,
            1000.0,
            0.7,
            false,
            false,
        );

        for i in 0..256 {
            let x = 0.6 * (i as f32 * 0.05).sin();
            let frame = [x; 6];
            let (mut a, mut b) = ([0.0f32; 6], [0.0f32; 6]);
            plain.tick(&frame, &mut a);
            ported.tick(&frame, &mut b);
            assert_eq!(a, b, "the two construction paths diverged at sample {i}");
        }
    }
}
