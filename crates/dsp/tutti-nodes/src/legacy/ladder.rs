use tutti_core::Arc;
use tutti_core::AtomicF32;
use tutti_core::{AudioUnit, BufferMut, BufferRef, Real, SignalFrame};

use tutti_core::{Drive, Hz, Param, Resonance, SampleRate};

/// Below these deltas a freq/resonance change doesn't warrant recomputing the
/// coefficients — the change guard shared by the atomic and modulation paths.
const FREQ_EPS: f32 = 0.01;
const RES_EPS: f32 = 0.0001;

pub(crate) use crate::LadderType;

/// Runtime DSP state for a ladder filter instance — the four stage
/// integrators plus the coefficient cache. Split out so the struct proper
/// reads as a bag of typed parameters.
#[derive(Clone)]
struct LadderState<F: Real> {
    stages: [F; 4],
    last_freq: Hz,
    last_res: Resonance,
    g: F,
    k: F,
}

impl<F: Real> LadderState<F> {
    fn zeroed() -> Self {
        let zero = F::from_f64(0.0);
        Self {
            stages: [zero; 4],
            last_freq: Hz(-1.0),
            last_res: Resonance(-1.0),
            g: zero,
            k: zero,
        }
    }

    fn reset_z(&mut self) {
        self.stages = [F::from_f64(0.0); 4];
    }

    fn invalidate(&mut self) {
        self.last_freq = Hz(-1.0);
    }
}

/// Moog-style four-stage ladder filter with resonance and drive: 1 input, 1
/// output.
///
/// Cutoff ([`Hz`]), [`Resonance`] and [`Drive`] are live [`Param`]s shared
/// across clones. Cutoff and resonance are read once per block and recompute
/// coefficients only when one moves past a small epsilon; drive is read per
/// sample and needs no coefficients.
///
/// **Resonance is `0.0..=1.0`, not a [`Q`](tutti_core::Q).** It scales the
/// ladder's feedback: `0.0` is no emphasis, and toward `1.0` the filter
/// resonates hard at the cutoff and approaches self-oscillation. The feedback
/// runs through a `tanh` saturator, so it grits rather than blowing up.
///
/// [`Drive`] multiplies the input into that same saturator, so it is a
/// distortion control rather than a level one: above unity it adds harmonics
/// and compresses. `1.0` is clean.
///
/// `F` is the internal state precision, defaulting to `f64` for accuracy at low
/// cutoffs.
pub struct LadderFilterNode<F: Real = f64> {
    ladder_type: LadderType,
    frequency: Param<Hz>,
    resonance: Param<Resonance>,
    drive: Param<Drive>,
    sample_rate: SampleRate,
    state: LadderState<F>,
}

impl<F: Real> LadderFilterNode<F> {
    /// Builds a ladder filter of `ladder_type` at `frequency` cutoff and
    /// `resonance`, with unity [`Drive`].
    ///
    /// `resonance` is clamped to `0.0..=1.0`; `0.0` gives no emphasis at the
    /// cutoff and values near `1.0` approach self-oscillation.
    ///
    /// **Starts at the placeholder [`SampleRate::DEFAULT`]**: the coefficients
    /// computed here are relative to Nyquist, which is not known until the
    /// device is open. Call [`AudioUnit::set_sample_rate`] before the first
    /// `process`; it recomputes them. Skip it at 48 kHz and the corner sits
    /// 8.8% high, and the resonance peak moves with it — on a ladder that is
    /// the audible half, since the emphasis is what the ear tracks. See the
    /// crate-level "born at a placeholder rate" section.
    ///
    /// [`SampleRate::DEFAULT`]: tutti_core::SampleRate::DEFAULT
    /// [`AudioUnit::set_sample_rate`]: tutti_core::AudioUnit::set_sample_rate
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
            sample_rate: SampleRate::DEFAULT,
            state: LadderState::zeroed(),
        };
        node.update_coefficients(frequency, resonance);
        node
    }

    /// The shared cutoff cell in [`Hz`].
    ///
    /// Read once per block. The coefficient computation clamps to
    /// `1.0..=0.998 * Nyquist`, since `tan` diverges at Nyquist. Shared across
    /// clones.
    pub fn frequency(&self) -> Arc<AtomicF32> {
        self.frequency.as_atomic()
    }

    /// The shared [`Resonance`] cell, `0.0..=1.0`.
    ///
    /// Scales the ladder feedback: `0.0` no emphasis, near `1.0` approaching
    /// self-oscillation. The computation clamps to that range regardless of
    /// what is written here. Read once per block.
    pub fn resonance(&self) -> Arc<AtomicF32> {
        self.resonance.as_atomic()
    }

    /// The shared [`Drive`] cell — input gain into the `tanh` saturator.
    ///
    /// Read **per sample**, so it modulates smoothly and needs no coefficient
    /// recompute. `1.0` is clean; higher adds harmonics and compresses.
    pub fn drive(&self) -> Arc<AtomicF32> {
        self.drive.as_atomic()
    }

    /// Sets the cutoff in [`Hz`], floored at 1 Hz.
    ///
    /// The upper bound is applied when coefficients are computed, at `0.998` of
    /// Nyquist.
    pub fn set_frequency(&self, hz: impl Into<Hz>) {
        self.frequency.store(Hz(hz.into().get().max(1.0)));
    }

    /// Sets the [`Resonance`], clamped to `0.0..=1.0`.
    pub fn set_resonance(&self, res: impl Into<Resonance>) {
        self.resonance
            .store(Resonance::new_clamped(res.into().get()));
    }

    /// Sets the [`Drive`] into the saturator, floored at `0.1`.
    ///
    /// The floor keeps drive from silencing the filter: it multiplies the
    /// input, so `0.0` would mute rather than clean up.
    pub fn set_drive(&self, drive: impl Into<Drive>) {
        self.drive.store(Drive(drive.into().get().max(0.1)));
    }

    fn update_coefficients(&mut self, freq: Hz, resonance: Resonance) {
        // `tan` diverges at Nyquist itself, so the cutoff stops just short of it.
        let fc = f64::from(freq.get())
            .clamp(1.0, f64::from(self.sample_rate.nyquist_scaled(0.998).get()));
        self.state.g = F::from_f64((core::f64::consts::PI * fc / self.sample_rate.get()).tan());
        self.state.k = F::from_f64(4.0 * f64::from(resonance.get().clamp(0.0, 1.0)));
        self.state.last_freq = freq;
        self.state.last_res = resonance;
    }

    #[inline]
    fn maybe_update(&mut self) {
        let freq = self.frequency.load();
        let res = self.resonance.load();
        self.maybe_update_modulated(freq, res);
    }

    /// Recompute coefficients only when freq/resonance moved past a small
    /// epsilon, so a held value doesn't recompute every sample. Shared by the
    /// atomic ([`Self::maybe_update`]) and audio-rate modulation paths.
    #[inline]
    fn maybe_update_modulated(&mut self, freq: Hz, res: Resonance) {
        if (freq.get() - self.state.last_freq.get()).abs() > FREQ_EPS
            || (res.get() - self.state.last_res.get()).abs() > RES_EPS
        {
            self.update_coefficients(freq, res);
        }
    }

    #[inline]
    fn process_one(&mut self, input: F) -> F {
        let drive = self.drive.load();
        self.process_one_with_drive(input, drive)
    }

    /// One sample using an explicit drive (the audio-rate modulation path);
    /// coefficients must already be set for the desired freq/resonance.
    #[inline]
    fn process_one_with_drive(&mut self, input: F, drive: Drive) -> F {
        let one = F::from_f64(1.0);
        let drive = F::from_f32(drive.get());
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
/// port overrides the corresponding atomic per sample, forcing a coefficient
/// recompute for cutoff/Q; absent, the node is a plain 2-in/2-out filter with
/// zero added cost.
///
/// Note the port is named `q` for consistency with the other filters, but it
/// carries [`Resonance`] (`0.0..=1.0`), not a [`Q`](tutti_core::Q).
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
    /// Builds a stereo (width-2) ladder filter with no param-input ports.
    ///
    /// Shorthand for [`with_channels(2, …)`](Self::with_channels). Both
    /// channels share one authored parameter set and keep independent filter
    /// state.
    pub fn new(
        ladder_type: LadderType,
        frequency: impl Into<Hz>,
        resonance: impl Into<Resonance>,
    ) -> Self {
        Self::with_channels(2, ladder_type, frequency, resonance)
    }

    /// An `n`-channel ladder filter (clamped to at least 1).
    ///
    /// All channels share the authored params — one linked control surface,
    /// held canonically by channel 0 — and only the per-channel filter state is
    /// replicated.
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
    /// appended after the audio inputs in that order. Each present port
    /// overrides its atomic per sample; the atomics still hold the base.
    ///
    /// Width and modulation are **independent axes**: `channels` says how wide
    /// the filter is, the `mod_*` flags say which params it reads at audio rate.
    /// Collapsing them — building the modulated form at a fixed width 2 — turns
    /// a request for a modulated 5.1 filter into a *stereo* one, and the only
    /// symptom is a `set_source` on a param port that resolves and carries the
    /// wrong signal.
    ///
    /// The param ports follow the audio inputs, so their indices **move with the
    /// width**. Ask [`ParamPorts::param_port`](crate::ParamPorts::param_port);
    /// never assume an index.
    pub fn with_param_inputs(
        channels: usize,
        ladder_type: LadderType,
        frequency: impl Into<Hz>,
        resonance: impl Into<Resonance>,
        mod_cutoff: bool,
        mod_q: bool,
        mod_drive: bool,
    ) -> Self {
        let mut node = Self::with_channels(channels, ladder_type, frequency, resonance);
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

    /// The shared cutoff cell in [`Hz`], governing every channel.
    ///
    /// **A present cutoff param-input port overrides this per sample.** Shared
    /// across clones.
    pub fn frequency(&self) -> Arc<AtomicF32> {
        self.channels[0].frequency()
    }

    /// The shared [`Resonance`] cell (`0.0..=1.0`), governing every channel.
    ///
    /// **A present Q param-input port overrides this per sample.**
    pub fn resonance(&self) -> Arc<AtomicF32> {
        self.channels[0].resonance()
    }

    /// The shared [`Drive`] cell, governing every channel.
    ///
    /// **A present drive param-input port overrides this per sample.**
    pub fn drive(&self) -> Arc<AtomicF32> {
        self.channels[0].drive()
    }

    /// Sets the cutoff in [`Hz`] for every channel, floored at 1 Hz.
    ///
    /// With a cutoff param-input port present this sets the *base* the port
    /// overrides.
    pub fn set_frequency(&self, hz: impl Into<Hz>) {
        self.channels[0].set_frequency(hz);
    }

    /// Sets the [`Resonance`] for every channel, clamped to `0.0..=1.0`.
    pub fn set_resonance(&self, res: impl Into<Resonance>) {
        self.channels[0].set_resonance(res);
    }

    /// Sets the [`Drive`] for every channel, floored at `0.1`.
    pub fn set_drive(&self, drive: impl Into<Drive>) {
        self.channels[0].set_drive(drive);
    }

    /// Effective per-sample (freq, resonance, drive): a present param port
    /// overrides the corresponding atomic. `read` reads input port `p`. Params
    /// are channel-shared, so the canonical `channels[0]` atomics are the base.
    #[inline]
    fn effective_params(&self, read: impl Fn(usize) -> f32) -> (Hz, Resonance, Drive) {
        let head = &self.channels[0];
        let freq = self
            .cutoff_port()
            .map_or_else(|| head.frequency.load(), |p| Hz(read(p).max(1.0)));
        let res = self.q_port().map_or_else(
            || head.resonance.load(),
            |p| Resonance(read(p).clamp(0.0, 1.0)),
        );
        let drive = self
            .drive_port()
            .map_or_else(|| head.drive.load(), |p| Drive(read(p).max(0.1)));
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

    fn set(&mut self, setting: tutti_core::Setting) {
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
