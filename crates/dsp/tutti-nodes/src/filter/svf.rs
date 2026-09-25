//! State-variable filter — the crate's general-purpose filter.
//!
//! One coefficient computation serves eight responses ([`SvfType`]), so
//! switching type is as cheap as any parameter change. One width-generic node,
//! [`SvfFilterNode`]: the coefficients are shared across channels and only the
//! integrator state is per channel.

use tutti_core::Arc;
use tutti_core::AtomicF32;
use tutti_core::{AudioUnit, BufferMut, BufferRef, Real, SignalFrame};

use tutti_core::{ChannelLayout, Db, Hz, Param, SampleRate, Q};

use crate::ramp::{self, LastGood, Ramp};

/// Below these deltas a freq/Q/gain change doesn't warrant recomputing the
/// coefficients — the change guard shared by the atomic and modulation paths.
///
/// The thresholds alone would not keep a NaN out (`|NaN - last| > eps` is
/// false, so a NaN is held off only until another control moves); the cells
/// are read through `LastGood`, which does.
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
///
/// Public so a voice engine that runs the same topology across SIMD lanes
/// (`tutti-polysynth`'s voice bank) derives its coefficients from this one
/// function rather than a copy of it. The per-sample recurrence those
/// coefficients feed is `v3 = v0 - ic2; v1 = a1*ic1 + a2*v3;
/// v2 = ic2 + a2*ic1 + a3*v3; ic1 = 2*v1 - ic1; ic2 = 2*v2 - ic2;
/// y = m0*v0 + m1*v1 + m2*v2` (Simper's trapezoidal SVF).
#[derive(Debug, Clone, Copy)]
pub struct SvfCoeffs {
    /// `1 / (1 + g*(g + k))`, with `g = tan(pi * fc / sr)` and `k = 1 / Q`.
    pub a1: f64,
    /// `g * a1`.
    pub a2: f64,
    /// `g * a2`.
    pub a3: f64,
    /// Output weight of the input `v0`.
    pub m0: f64,
    /// Output weight of the band-pass state `v1`.
    pub m1: f64,
    /// Output weight of the low-pass state `v2`.
    pub m2: f64,
}

/// Compute SVF filter coefficients from parameters (pure function, no state).
///
/// The three tuning parameters are three different units, and taking them as
/// [`Hz`] / [`Q`] / [`Db`] is what keeps them apart. As bare `f32`s they are
/// adjacent and interchangeable — the call reads as a run of positional numbers,
/// `(LowPass, 1000.0, 0.707, 0.0, 44100.0)`, where transposing any two compiles
/// and detunes the filter.
pub fn compute_svf_coeffs(
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

/// Parameter-derived filter coefficients, plus the parameter values they were
/// solved at — which is what gates recomputation and where a block ramp starts.
///
/// One set serves every channel: the params are one linked control surface, so
/// widening replicates only the integrator state.
#[derive(Clone, Copy, PartialEq)]
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
    fn solve(filter_type: SvfType, freq: Hz, q: Q, gain_db: Db, sample_rate: SampleRate) -> Self {
        let c = compute_svf_coeffs(filter_type, freq, q, gain_db, sample_rate);
        Self {
            a1: F::from_f64(c.a1),
            a2: F::from_f64(c.a2),
            a3: F::from_f64(c.a3),
            m0: F::from_f64(c.m0),
            m1: F::from_f64(c.m1),
            m2: F::from_f64(c.m2),
            last_freq: freq,
            last_q: q,
            last_gain_db: gain_db,
        }
    }

    /// Marks the set stale, so the next read solves at its target outright
    /// rather than ramping from parameters that no longer mean anything (a new
    /// sample rate, a new response type).
    fn invalidate(&mut self) {
        self.last_freq = Hz(-1.0);
    }

    fn is_invalid(&self) -> bool {
        self.last_freq.get() < 0.0
    }

    /// Whether `(freq, q, gain_db)` moved past the epsilons since this set was
    /// solved. A held value costs nothing.
    #[inline]
    fn moved(&self, freq: Hz, q: Q, gain_db: Db) -> bool {
        (freq.get() - self.last_freq.get()).abs() > FREQ_EPS
            || (q.get() - self.last_q.get()).abs() > Q_EPS
            || (gain_db.get() - self.last_gain_db.get()).abs() > GAIN_EPS
    }

    /// Linear blend toward `to` by `t` in `0..=1`. The parameters it was solved
    /// at are not blended — an interpolated set is only ever used for one
    /// sample and never stored.
    #[inline]
    fn lerp(&self, to: &Self, t: F) -> Self {
        Self {
            a1: self.a1 + (to.a1 - self.a1) * t,
            a2: self.a2 + (to.a2 - self.a2) * t,
            a3: self.a3 + (to.a3 - self.a3) * t,
            m0: self.m0 + (to.m0 - self.m0) * t,
            m1: self.m1 + (to.m1 - self.m1) * t,
            m2: self.m2 + (to.m2 - self.m2) * t,
            ..*to
        }
    }
}

/// One channel's integrator step (Cytomic/Zavalishin TPT form) over its two
/// state words `[ic1eq, ic2eq]`.
///
/// The pair is kept together per channel (array-of-structures), not split into
/// one array per word: the two updates are symmetric and their loads and
/// stores stay adjacent. Measured, splitting them cost 26% at width 6 — the
/// across-channel layout design doc 013 sketched only pays once a
/// channel-inner loop is explicitly vectorised, which this one is not.
#[inline(always)]
fn svf_step<F: Real>(c: &SvfCoefficients<F>, s: &mut [F; 2], v0: F) -> F {
    let two = F::from_f64(2.0);
    let [ic1eq, ic2eq] = *s;
    let v3 = v0 - ic2eq;
    let v1 = c.a1 * ic1eq + c.a2 * v3;
    let v2 = ic2eq + c.a2 * ic1eq + c.a3 * v3;
    *s = [two * v1 - ic1eq, two * v2 - ic2eq];
    c.m0 * v0 + c.m1 * v1 + c.m2 * v2
}

/// State-variable filter of any width: `N` audio inputs, `N` outputs, one
/// coefficient set shared across the channels and one integrator pair per
/// channel.
///
/// This used to be two types — a mono `SvfFilterNode` and a
/// `StereoSvfFilterNode` that was already `N`-wide despite its name (a leftover
/// of the extraction). They ran the same integrator; the merge keeps the mono
/// arithmetic bit-for-bit at width 1 and the old wide arithmetic at every other
/// width.
///
/// Cutoff ([`Hz`]), [`Q`] and gain ([`Db`]) are live [`Param`]s shared across
/// clones, read **once per block**. A held value costs nothing — the
/// coefficients are solved only when one moves past a small epsilon. A value
/// that *did* move is ramped across the block: the coefficients are re-solved
/// every 16 samples along a linear parameter ramp and interpolated between
/// solves, so an automated cutoff glides rather than stepping at block edges.
/// `tick` is a block of one, so it applies a change on the very next sample.
///
/// Gain applies only to [`Bell`](SvfType::Bell),
/// [`LowShelf`](SvfType::LowShelf) and [`HighShelf`](SvfType::HighShelf); the
/// other types ignore it.
///
/// `F` is the internal state precision. It defaults to `f64` for accuracy at
/// low cutoffs, where an `f32` integrator loses resolution in the feedback
/// path; `SvfFilterNode::<f32>::new(…)` trades that for CPU.
///
/// # Port layout
///
/// `N` audio inputs, then *optional param-input ports* (see
/// [`Self::with_param_inputs`]): a cutoff port in [`Hz`], then a [`Q`] port,
/// each present only if its `mod_*` flag was set. A present port **overrides**
/// the corresponding atomic. It is sampled at the solve points — every 16
/// samples and at the block's last sample — with the coefficients interpolated
/// between them, instead of a `tan` per sample. Without ports the filter is a
/// plain `N`-in/`N`-out node with zero added cost, which is the common case.
pub struct SvfFilterNode<F: Real = f64> {
    filter_type: SvfType,
    frequency: Param<Hz>,
    q: Param<Q>,
    gain_db: Param<Db>,
    sample_rate: SampleRate,
    /// The coefficients the last rendered sample ran at.
    coeffs: SvfCoefficients<F>,
    /// Integrator state `[ic1eq, ic2eq]`, one pair per channel; `len()` is the
    /// audio width. Built at construction — never resized in `tick`/`process`
    /// (RT no-alloc).
    state: Vec<[F; 2]>,
    /// The last finite cutoff / Q / gain the cells held: a non-finite write
    /// reads as unchanged, so it never reaches the solve (see [`LastGood`]).
    good: [LastGood; 3],
    /// When true, a cutoff param-input port follows the audio inputs.
    mod_cutoff: bool,
    /// When true, a Q param-input port follows the cutoff port (or the audio
    /// inputs if `mod_cutoff` is false).
    mod_q: bool,
}

impl<F: Real> SvfFilterNode<F> {
    /// A mono filter (1 in, 1 out) of `filter_type` at `frequency` cutoff and
    /// resonance `q`, with 0 dB gain.
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
        Self::with_channels(ChannelLayout::MONO, filter_type, frequency, q)
    }

    /// A filter `channels` wide (clamped to at least 1).
    ///
    /// The coefficients are channel-shared — one linked control surface across
    /// all channels — and only the per-channel integrator state is replicated,
    /// so widening costs state but not parameter work.
    ///
    /// Speaker placement is the upstream panner's job: this is a per-channel
    /// filter, not a spatial process.
    ///
    /// **Starts at the placeholder [`SampleRate::DEFAULT`]**, as [`new`](Self::new)
    /// does. The coefficients are shared, so an uncorrected rate skews every
    /// channel identically — wrong everywhere rather than unbalanced, which is
    /// why widening does not make it any easier to hear.
    ///
    /// [`SampleRate::DEFAULT`]: tutti_core::SampleRate::DEFAULT
    pub fn with_channels(
        channels: impl Into<ChannelLayout>,
        filter_type: SvfType,
        frequency: impl Into<Hz>,
        q: impl Into<Q>,
    ) -> Self {
        let frequency = frequency.into();
        let q = q.into();
        let n = usize::from(channels.into().count()).max(1);
        let zero = F::from_f64(0.0);
        Self {
            filter_type,
            frequency: Param::new(frequency),
            q: Param::new(q),
            gain_db: Param::new(Db(0.0)),
            sample_rate: SampleRate::DEFAULT,
            coeffs: SvfCoefficients::solve(filter_type, frequency, q, Db(0.0), SampleRate::DEFAULT),
            state: vec![[zero; 2]; n],
            good: [
                LastGood::new(frequency.get()),
                LastGood::new(q.get()),
                LastGood::new(0.0),
            ],
            mod_cutoff: false,
            mod_q: false,
        }
    }

    /// A filter with optional audio-rate param-input ports. `mod_cutoff` /
    /// `mod_q` add a cutoff / Q param-input port after the audio inputs
    /// (cutoff first), each overriding its atomic when present. The atomics
    /// still hold the base (they feed the upstream param-sum's base port), so
    /// the UI handle path is unchanged.
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
        channels: impl Into<ChannelLayout>,
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
        self.state.len()
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
        self.good[2] = LastGood::new(db.get());
        self.coeffs = SvfCoefficients::solve(
            self.filter_type,
            self.frequency.load(),
            self.q.load(),
            db,
            self.sample_rate,
        );
        self
    }

    /// The shared cutoff cell in [`Hz`], applied to every channel alike.
    ///
    /// Read once per block. Writing the raw cell bypasses the 1 Hz floor
    /// [`set_frequency`](Self::set_frequency) applies; the coefficient
    /// computation clamps to `1.0..=0.998 * Nyquist` regardless, since `tan`
    /// diverges at Nyquist. **A present cutoff param-input port overrides
    /// it.** Shared across clones.
    ///
    /// A non-finite value written here (NaN, ±∞) reads as *unchanged*: the
    /// filter keeps the last finite value, so it never reaches the coefficient
    /// solve or the filter state, and the next finite write takes effect.
    pub fn frequency(&self) -> Arc<AtomicF32> {
        self.frequency.as_atomic()
    }

    /// The shared resonance cell in [`Q`], applied to every channel alike.
    ///
    /// Read once per block. Writing the raw cell bypasses
    /// [`set_q`](Self::set_q)'s clamp; the coefficient computation floors the
    /// value at `0.01` to avoid a division blow-up. **A present Q param-input
    /// port overrides it.** Shared across clones.
    ///
    /// A non-finite value written here (NaN, ±∞) reads as *unchanged*: the
    /// filter keeps the last finite value, so it never reaches the coefficient
    /// solve or the filter state, and the next finite write takes effect.
    pub fn q(&self) -> Arc<AtomicF32> {
        self.q.as_atomic()
    }

    /// The shared shelf/bell gain cell in [`Db`].
    ///
    /// Always read from the atomic — there is no gain param-input port. Inert
    /// on the non-gain filter types.
    ///
    /// A non-finite value written here (NaN, ±∞) reads as *unchanged*: the
    /// filter keeps the last finite value, so it never reaches the coefficient
    /// solve or the filter state, and the next finite write takes effect.
    pub fn gain_db(&self) -> Arc<AtomicF32> {
        self.gain_db.as_atomic()
    }

    /// Sets the cutoff in [`Hz`] for every channel, floored at 1 Hz.
    ///
    /// The upper bound is applied when coefficients are computed, at `0.998` of
    /// Nyquist — so this accepts a higher value and the filter tops out there.
    /// When a cutoff param-input port is present this sets the *base* the port
    /// overrides, not what the filter runs at.
    pub fn set_frequency(&self, hz: impl Into<Hz>) {
        self.frequency.store(Hz(hz.into().get().max(1.0)));
    }

    /// Sets the resonance [`Q`] for every channel, clamped to the valid range.
    ///
    /// `0.707` is the flattest response; higher resonates at the cutoff.
    pub fn set_q(&self, q: impl Into<Q>) {
        self.q.store(Q::new_clamped(q.into().get()));
    }

    /// Sets the shelf/bell gain in [`Db`]. Inert on the non-gain filter types.
    pub fn set_gain_db(&self, db: impl Into<Db>) {
        self.gain_db.store(db.into());
    }

    /// Switches the filter response for every channel, forcing a coefficient
    /// solve on the next sample.
    ///
    /// `&mut self`, so this cannot reach a node already live in the graph —
    /// the response is a structural choice, not an automatable parameter.
    /// Integrator state is deliberately kept, so the switch is continuous
    /// rather than a click.
    pub fn set_filter_type(&mut self, filter_type: SvfType) {
        self.filter_type = filter_type;
        self.coeffs.invalidate();
    }

    /// Read the three control cells, once, with non-finite values held off.
    #[inline]
    fn read_controls(&mut self) -> (Hz, Q, Db) {
        (
            Hz(self.good[0].read(self.frequency.load().get())),
            Q(self.good[1].read(self.q.load().get())),
            Db(self.good[2].read(self.gain_db.load().get())),
        )
    }

    /// Solve at `(freq, q, gain_db)` unless the current set is already there.
    #[inline]
    fn solve_toward(
        &self,
        from: &SvfCoefficients<F>,
        freq: Hz,
        q: Q,
        gain_db: Db,
    ) -> SvfCoefficients<F> {
        if from.is_invalid() || from.moved(freq, q, gain_db) {
            SvfCoefficients::solve(self.filter_type, freq, q, gain_db, self.sample_rate)
        } else {
            *from
        }
    }

    /// Render with the current coefficients held for the whole block.
    ///
    /// The channels go through in groups of up to `ramp::LANES`, sample-inner, with
    /// the group's integrators in registers. A recursive filter is
    /// latency-bound on one channel — each sample waits for the last — so
    /// running one channel at a time (measured: 2.7× slower at width 6) wastes
    /// the independent chains the other channels offer; a group interleaves
    /// them while still reading and writing planar slices.
    fn run_held(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        let w = self.width();
        let mut first = 0;
        while first < w {
            let n = ramp::lane_group(w - first);
            match n {
                8 => self.held_group::<8>(first, size, input, output),
                7 => self.held_group::<7>(first, size, input, output),
                6 => self.held_group::<6>(first, size, input, output),
                5 => self.held_group::<5>(first, size, input, output),
                4 => self.held_group::<4>(first, size, input, output),
                3 => self.held_group::<3>(first, size, input, output),
                2 => self.held_group::<2>(first, size, input, output),
                _ => self.held_group::<1>(first, size, input, output),
            }
            first += n;
        }
    }

    #[inline]
    fn held_group<const N: usize>(
        &mut self,
        first: usize,
        size: usize,
        input: &BufferRef,
        output: &mut BufferMut,
    ) {
        let c = self.coeffs;
        let xs: [&[f32]; N] = core::array::from_fn(|k| &input.channel_f32(first + k)[..size]);
        let ys: [&mut [f32]; N] =
            core::array::from_fn(|k| &mut output.channel_f32_mut(first + k)[..size]);
        let mut st: [[F; 2]; N] = core::array::from_fn(|k| self.state[first + k]);
        for i in 0..size {
            for k in 0..N {
                ys[k][i] = svf_step(&c, &mut st[k], F::from_f32(xs[k][i])).to_f32();
            }
        }
        self.state[first..first + N].copy_from_slice(&st);
    }

    /// Render with the parameters moving inside the block. `param_at(i)` is the
    /// `(cutoff, Q, gain)` at sample `i`; it is only asked at the solve points —
    /// each segment's last sample — and the coefficients are interpolated
    /// linearly between consecutive solves. The last sample of each segment runs
    /// at its solved set exactly, so a segment of one sample (`tick`) is the
    /// per-sample solve it replaces.
    fn run_swept(
        &mut self,
        size: usize,
        input: &BufferRef,
        output: &mut BufferMut,
        param_at: impl Fn(usize) -> (Hz, Q, Db),
    ) {
        let width = self.width();
        let mut prev = self.coeffs;
        let unprimed = prev.is_invalid();
        if unprimed {
            // Nothing meaningful to ramp from — solve at the first sample.
            let (f, q, g) = param_at(0);
            prev = SvfCoefficients::solve(self.filter_type, f, q, g, self.sample_rate);
        }
        for (start, end) in ramp::segments(size, unprimed) {
            let (f, q, g) = param_at(end - 1);
            let next = self.solve_toward(&prev, f, q, g);
            let n = F::from_f64((end - start) as f64);
            for i in start..end {
                let k = if next == prev || i + 1 == end {
                    next
                } else {
                    prev.lerp(&next, F::from_f64((i - start + 1) as f64) / n)
                };
                for ch in 0..width {
                    let x = F::from_f32(input.at_f32(ch, i));
                    let y = svf_step(&k, &mut self.state[ch], x);
                    output.set_f32(ch, i, y.to_f32());
                }
            }
            prev = next;
        }
        self.coeffs = prev;
    }
}

impl<F: Real + 'static> AudioUnit for SvfFilterNode<F> {
    fn inputs(&self) -> usize {
        self.width() + self.mod_cutoff as usize + self.mod_q as usize
    }

    fn outputs(&self) -> usize {
        self.width()
    }

    fn reset(&mut self) {
        let zero = F::from_f64(0.0);
        self.state.fill([zero; 2]);
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        self.sample_rate = sample_rate;
        self.coeffs.invalidate();
    }

    #[inline]
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        // A block of one: read every control once, solve at it if it moved.
        // A port's `max` floor also maps a NaN sample to the floor.
        let (base_freq, base_q, gain_db) = self.read_controls();
        let freq = self
            .cutoff_port()
            .map_or(base_freq, |p| Hz(input[p].max(1.0)));
        let q = self.q_port().map_or(base_q, |p| Q(input[p].max(0.01)));
        self.coeffs = self.solve_toward(&self.coeffs, freq, q, gain_db);
        let c = self.coeffs;
        for ch in 0..self.width() {
            output[ch] = svf_step(&c, &mut self.state[ch], F::from_f32(input[ch])).to_f32();
        }
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        if size == 0 {
            return;
        }
        // Every control is read here, once, for the whole block. A port's
        // `max` floor maps a NaN sample to the floor, and ±∞ is clamped by the
        // solve, so only the cells need holding off.
        let (base_freq, base_q, gain_db) = self.read_controls();
        match (self.cutoff_port(), self.q_port()) {
            (None, None) => {
                if self.coeffs.is_invalid() {
                    // A new rate or response type: solve at the target outright.
                    self.coeffs = SvfCoefficients::solve(
                        self.filter_type,
                        base_freq,
                        base_q,
                        gain_db,
                        self.sample_rate,
                    );
                }
                if !self.coeffs.moved(base_freq, base_q, gain_db) {
                    self.run_held(size, input, output);
                    return;
                }
                // Moved since the last block: glide there across this one.
                let fr = Ramp::new(self.coeffs.last_freq.get(), base_freq.get(), size);
                let qr = Ramp::new(self.coeffs.last_q.get(), base_q.get(), size);
                let gr = Ramp::new(self.coeffs.last_gain_db.get(), gain_db.get(), size);
                self.run_swept(size, input, output, |i| {
                    (Hz(fr.at(i)), Q(qr.at(i)), Db(gr.at(i)))
                });
            }
            (cp, qp) => {
                let cutoff = cp.map(|p| input.channel_f32(p));
                let res = qp.map(|p| input.channel_f32(p));
                self.run_swept(size, input, output, |i| {
                    (
                        cutoff.map_or(base_freq, |s| Hz(s[i].max(1.0))),
                        res.map_or(base_q, |s| Q(s[i].max(0.01))),
                        gain_db,
                    )
                });
            }
        }
    }

    /// Sever the param cells a clone shares (`Clone` takes `handle()`s, so
    /// the frontend and backend of a `Net` move together), keeping their
    /// current values — as `MemorySource::isolate_gain` does. After this a
    /// write to this copy's cutoff, Q or gain never reaches the live filter:
    /// an offline render does not follow the live controls, and a
    /// `Legacy::controlled` shadow never moves the live cutoff ahead of its
    /// settings ring.
    fn isolate(&mut self) {
        self.frequency = Param::new(self.frequency.load());
        self.q = Param::new(self.q.load());
        self.gain_db = Param::new(self.gain_db.load());
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
        // Keyed on the audio width, not `inputs()`: param ports are not a
        // channel, so a mono filter with a cutoff port is still the mono shape.
        // Width 1 keeps the id it always had, every other width the one the
        // wide twin carried. Only the render hash reads it.
        if self.width() == 1 {
            crate::node_id::SVF_FILTER_ID
        } else {
            crate::node_id::SVF_FILTER_ID ^ 0xDA02
        }
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
        core::mem::size_of::<Self>() + 2 * self.width() * core::mem::size_of::<F>()
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
            coeffs: self.coeffs,
            state: self.state.clone(),
            good: self.good,
            mod_cutoff: self.mod_cutoff,
            mod_q: self.mod_q,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::test_utils::{generate_sine, make_impulse, process_mono, rms};

    /// A clone shares the param cells (so a `Net`'s frontend and backend move
    /// together); an isolated clone keeps their values and shares nothing, so
    /// a write to it never reaches the original.
    ///
    /// Mutation: drop any one of the three re-seats in `isolate` → that
    /// param's write leaks to the original → fails.
    #[test]
    fn isolate_severs_the_param_cells_and_keeps_their_values() {
        let live = SvfFilterNode::<f64>::new(SvfType::LowPass, 500.0, 0.707).with_gain_db(-3.0);
        let shared = live.clone();
        shared.set_frequency(600.0);
        assert_eq!(live.frequency.load(), Hz(600.0), "a clone shares the cells");

        let mut isolated = live.clone();
        isolated.isolate();
        assert_eq!(isolated.frequency.load(), Hz(600.0), "values kept");
        assert_eq!(isolated.q.load(), Q(0.707));
        assert_eq!(isolated.gain_db.load(), Db(-3.0));
        isolated.set_frequency(2_000.0);
        isolated.set_q(3.0);
        isolated.set_gain_db(6.0);
        assert_eq!(live.frequency.load(), Hz(600.0));
        assert_eq!(live.q.load(), Q(0.707));
        assert_eq!(live.gain_db.load(), Db(-3.0));
    }

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
        let mut node = SvfFilterNode::<f64>::with_channels(
            ChannelLayout::STEREO,
            SvfType::LowPass,
            1000.0,
            0.707,
        );
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
    // Stereo width
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

        let mut stereo = SvfFilterNode::<f64>::with_channels(
            ChannelLayout::STEREO,
            SvfType::LowPass,
            1000.0,
            0.707,
        );
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
        let mut stereo = SvfFilterNode::<f64>::with_channels(
            ChannelLayout::STEREO,
            SvfType::LowPass,
            1000.0,
            0.707,
        );
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
        let mut stereo = SvfFilterNode::<f64>::with_channels(
            ChannelLayout::STEREO,
            SvfType::LowPass,
            1000.0,
            0.707,
        );
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
    fn with_channels_reports_arity() {
        let f = SvfFilterNode::<f64>::with_channels(6usize, SvfType::HighPass, 800.0, 0.707);
        assert_eq!(f.inputs(), 6);
        assert_eq!(f.outputs(), 6);
    }

    #[test]
    fn wide_channel_matches_mono_and_is_independent() {
        // Each of 6 channels must filter exactly like a mono SVF with the same
        // coeffs, and carry only its own input (no cross-channel bleed).
        let mut mono = SvfFilterNode::<f64>::new(SvfType::LowPass, 1000.0, 0.707);
        mono.set_sample_rate(tutti_core::SampleRate(44100.0));
        let mut wide = SvfFilterNode::<f64>::with_channels(6usize, SvfType::LowPass, 1000.0, 0.707);
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
    fn stereo_svf_param_port_arity_and_indices() {
        // Plain constructor: no ports, audio arity untouched.
        let d = SvfFilterNode::<f64>::with_channels(
            ChannelLayout::STEREO,
            SvfType::LowPass,
            1000.0,
            0.707,
        );
        assert_eq!(d.inputs(), 2);
        assert_eq!(d.outputs(), 2);
        assert_eq!(d.cutoff_port(), None);
        assert_eq!(d.q_port(), None);
        // cutoff only → port 2.
        let c = SvfFilterNode::<f64>::with_param_inputs(
            ChannelLayout::STEREO,
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
        let q = SvfFilterNode::<f64>::with_param_inputs(
            ChannelLayout::STEREO,
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
        let b = SvfFilterNode::<f64>::with_param_inputs(
            ChannelLayout::STEREO,
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
            let mut f = SvfFilterNode::<f64>::with_param_inputs(
                ChannelLayout::STEREO,
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

        let mut plain = SvfFilterNode::<f64>::with_channels(
            ChannelLayout::STEREO,
            SvfType::LowPass,
            1000.0,
            0.707,
        );
        plain.set_sample_rate(tutti_core::SampleRate(44100.0));
        let (plain_l, _) = process_stereo(&mut plain, &signal, &signal);

        let mut modn = SvfFilterNode::<f64>::with_param_inputs(
            ChannelLayout::STEREO,
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
        let f = SvfFilterNode::<f64>::with_param_inputs(
            6usize,
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
        let mut plain = SvfFilterNode::<f64>::with_channels(6usize, SvfType::LowPass, 1000.0, 0.7);
        // Both flags off — the same filter, built through the other path.
        let mut ported = SvfFilterNode::<f64>::with_param_inputs(
            6usize,
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

    // ── Per-block reads ──────────────────────────────────────────────────────

    /// A cutoff change made between blocks is read by the next block and
    /// ramped across it, and that block ends solved exactly at the new cutoff.
    ///
    /// (A write *during* a block cannot be staged in a single-threaded test —
    /// the control thread writes between callbacks — so "between blocks" is the
    /// observable form of "not before the next block".)
    ///
    /// Mutation: solving straight at the target and rendering the block held
    /// (a jump) fails `assert_ramps_in`; dropping the `self.coeffs = prev`
    /// that stores the block's final solve fails the coefficient check.
    #[test]
    fn a_cutoff_change_is_read_next_block_and_ramped_across_it() {
        use crate::test_support::{change_between_blocks, noise};
        let x = noise(3, 128);
        let run = change_between_blocks(
            || {
                let mut n = SvfFilterNode::<f64>::with_channels(
                    ChannelLayout::STEREO,
                    SvfType::LowPass,
                    400.0,
                    0.707,
                );
                n.set_sample_rate(tutti_core::SampleRate(48_000.0));
                n
            },
            |n| n.set_frequency(6_000.0),
            &[&x[..64], &x[..64]],
            &[&x[64..], &x[64..]],
        );
        run.assert_ramps_in("svf cutoff");
        assert!(
            run.ramped.coeffs == run.jumped.coeffs,
            "the block must end solved at the new cutoff"
        );
    }

    /// A NaN in a raw cell never reaches the filter state — not even in the
    /// block where another control moves and forces a re-solve, which is the
    /// case a threshold comparison alone lets through. The filter keeps
    /// running at the last good cutoff, and picks up the next finite write.
    ///
    /// Mutation: reading the cutoff cell raw in `read_controls` (no
    /// `LastGood`) fails the second assertion — the NaN reaches the solve and
    /// the output goes NaN for good.
    #[test]
    fn a_nan_in_a_raw_cell_never_reaches_the_state() {
        use crate::test_support::{noise, process_block};
        let x = noise(31, 64);
        let mut f = SvfFilterNode::<f64>::new(SvfType::LowPass, 800.0, 0.7);
        f.set_sample_rate(tutti_core::SampleRate(48_000.0));
        process_block(&mut f, &[&x]);
        f.frequency()
            .store(f32::NAN, std::sync::atomic::Ordering::Release);
        let held = process_block(&mut f, &[&x]);
        assert!(held[0].iter().all(|s| s.is_finite()), "a NaN alone");
        f.set_q(3.0);
        let moved = process_block(&mut f, &[&x]);
        assert!(
            moved[0].iter().all(|s| s.is_finite()),
            "a NaN with another control moving in the same block"
        );
        assert_eq!(
            f.coeffs.last_freq,
            Hz(800.0),
            "held at the last good cutoff"
        );
        f.set_frequency(2_000.0);
        process_block(&mut f, &[&x]);
        assert_eq!(
            f.coeffs.last_freq,
            Hz(2_000.0),
            "and back on a finite write"
        );
    }
}
