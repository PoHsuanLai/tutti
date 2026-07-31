use tutti_core::Arc;
use tutti_core::AtomicF32;
use tutti_core::{dsp::DEFAULT_SAMPLE_RATE, AudioUnit, BufferMut, BufferRef, SignalFrame};

use tutti_core::{Feedback, Mix, Param, SampleRate, Seconds};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InterpolationMode {
    None,
    #[default]
    Linear,
    CubicHermite,
}

#[derive(Clone)]
pub struct DelayLine {
    pub(crate) buffer: Vec<f32>,
    write_pos: usize,
    max_delay_samples: usize,
}

impl DelayLine {
    pub fn new(max_delay_samples: usize) -> Self {
        Self {
            buffer: vec![0.0; max_delay_samples + 1],
            write_pos: 0,
            max_delay_samples,
        }
    }

    pub fn from_seconds(
        max_delay_secs: impl Into<Seconds>,
        sample_rate: impl Into<SampleRate>,
    ) -> Self {
        // `to_samples_ceil`, not a nearest-rounding cast: a line sized for
        // `max_delay` must hold *at least* that long, and rounding to nearest
        // under-allocates for half of all inputs. The multiply also stays in
        // f64 now — the old form narrowed the sample rate to f32 first.
        let samples = max_delay_secs
            .into()
            .to_samples_ceil(sample_rate.into().get());
        Self::new(samples.get())
    }

    pub fn push_sample(&mut self, sample: f32) {
        self.buffer[self.write_pos] = sample;
        self.write_pos += 1;
        if self.write_pos >= self.buffer.len() {
            self.write_pos = 0;
        }
    }

    pub fn read_sample(&self, delay_samples: f32, mode: InterpolationMode) -> f32 {
        let delay = delay_samples.clamp(0.0, self.max_delay_samples as f32);
        match mode {
            InterpolationMode::None => {
                let idx = delay.round() as usize;
                self.read_at(idx)
            }
            InterpolationMode::Linear => {
                let floor = delay.floor() as usize;
                let frac = delay - floor as f32;
                let s0 = self.read_at(floor);
                let s1 = self.read_at(floor + 1);
                s0 + frac * (s1 - s0)
            }
            InterpolationMode::CubicHermite => {
                let floor = delay.floor() as usize;
                let frac = delay - floor as f32;
                let sm1 = self.read_at(floor.saturating_sub(1));
                let s0 = self.read_at(floor);
                let s1 = self.read_at(floor + 1);
                let s2 = self.read_at(floor + 2);
                let c0 = s0;
                let c1 = 0.5 * (s1 - sm1);
                let c2 = sm1 - 2.5 * s0 + 2.0 * s1 - 0.5 * s2;
                let c3 = 0.5 * (s2 - sm1) + 1.5 * (s0 - s1);
                ((c3 * frac + c2) * frac + c1) * frac + c0
            }
        }
    }

    pub fn reset(&mut self) {
        self.buffer.fill(0.0);
        self.write_pos = 0;
    }

    #[inline]
    fn read_at(&self, delay_samples: usize) -> f32 {
        let len = self.buffer.len();
        let idx = (self.write_pos + len - 1 - delay_samples.min(self.max_delay_samples)) % len;
        self.buffer[idx]
    }
}

/// A delay position in samples, keeping the fraction.
///
/// Deliberately **not** `Seconds::to_samples`: that returns `Samples`, an
/// integer frame count, and the whole point of an interpolated read is the
/// fractional part — rounding here would quantise every delay to a frame
/// boundary and step audibly under modulation.
///
/// The multiply stays in `f64` and narrows once at the end. The old form was
/// `secs * self.sample_rate as f32`, which narrowed the *rate* first and so
/// computed the position at `f32` precision throughout.
#[inline]
fn fractional_samples(secs: Seconds, sample_rate: SampleRate) -> f32 {
    (secs.get() as f64 * sample_rate.get()) as f32
}

/// Mono delay with feedback. 1 input (audio), 1 output.
/// Delay time can be modulated via the typed parameter handles.
pub struct DelayLineNode {
    delay: DelayLine,
    delay_time: Param<Seconds>,
    feedback: Param<Feedback>,
    mix: Param<Mix>,
    interpolation: InterpolationMode,
    sample_rate: SampleRate,
    /// The longest delay this line can hold. Kept typed: it is a duration the
    /// setters clamp against, not scratch.
    max_delay: Seconds,
}

impl DelayLineNode {
    pub fn new(
        max_delay_secs: impl Into<Seconds>,
        delay_secs: impl Into<Seconds>,
        feedback: impl Into<Feedback>,
    ) -> Self {
        let max_delay = max_delay_secs.into();
        let delay_secs = delay_secs.into();
        let feedback = Feedback::new_clamped(feedback.into().get());
        Self {
            delay: DelayLine::from_seconds(max_delay.get(), DEFAULT_SAMPLE_RATE),
            delay_time: Param::new(delay_secs),
            feedback: Param::new(feedback),
            mix: Param::new(Mix::WET),
            interpolation: InterpolationMode::Linear,
            sample_rate: DEFAULT_SAMPLE_RATE,
            max_delay,
        }
    }

    pub fn with_interpolation(mut self, mode: InterpolationMode) -> Self {
        self.interpolation = mode;
        self
    }

    pub fn delay_time(&self) -> Arc<AtomicF32> {
        self.delay_time.as_atomic()
    }

    pub fn feedback(&self) -> Arc<AtomicF32> {
        self.feedback.as_atomic()
    }

    pub fn mix(&self) -> Arc<AtomicF32> {
        self.mix.as_atomic()
    }

    pub fn set_delay_time(&self, secs: impl Into<Seconds>) {
        self.delay_time.store(Seconds(secs.into().get().max(0.0)));
    }

    pub fn set_feedback(&self, fb: impl Into<Feedback>) {
        self.feedback.store(Feedback::new_clamped(fb.into().get()));
    }

    pub fn set_mix(&self, mix: impl Into<Mix>) {
        self.mix.store(Mix::new_clamped(mix.into().get()));
    }

    #[inline]
    fn process_sample(&mut self, input: f32, delay_samples: f32, fb: f32, mix: Mix) -> f32 {
        let feedback_tap = self
            .delay
            .read_sample(delay_samples.max(1.0) - 1.0, self.interpolation);
        self.delay.push_sample(input + feedback_tap * fb);
        let delayed = self.delay.read_sample(delay_samples, self.interpolation);
        mix.blend(input, delayed)
    }
}

impl AudioUnit for DelayLineNode {
    fn inputs(&self) -> usize {
        1
    }

    fn outputs(&self) -> usize {
        1
    }

    fn reset(&mut self) {
        self.delay.reset();
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        self.sample_rate = sample_rate;
        self.delay = DelayLine::from_seconds(self.max_delay, sample_rate);
    }

    #[inline]
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        let delay_samples = fractional_samples(self.delay_time.load(), self.sample_rate);
        let fb = self.feedback.load().get();
        let mix = self.mix.load();
        output[0] = self.process_sample(input[0], delay_samples, fb, mix);
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        let delay_samples = fractional_samples(self.delay_time.load(), self.sample_rate);
        let fb = self.feedback.load().get();
        let mix = self.mix.load();

        for i in 0..size {
            let in_s = input.at_f32(0, i);
            output.set_f32(0, i, self.process_sample(in_s, delay_samples, fb, mix));
        }
    }

    fn set(&mut self, setting: tutti_core::dsp::Setting) {
        if let Some((param, value)) = tutti_core::unit_param::from_setting(&setting) {
            match param {
                tutti_core::UnitParam::DelayTime => self.set_delay_time(value),
                tutti_core::UnitParam::Feedback => self.set_feedback(value),
                tutti_core::UnitParam::Wet => self.set_mix(value),
                _ => {}
            }
        }
    }

    fn get_id(&self) -> u64 {
        crate::node_id::DELAY_LINE_ID
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let mut out = SignalFrame::new(1);
        // `route` wants f64, so this one never narrows: the old form went
        // f32 → f64 and threw away precision on the way through.
        let delay_samples = self.delay_time.load().get() as f64 * self.sample_rate.get();
        out.set(0, input.at(0).delay(delay_samples));
        out
    }

    fn footprint(&self) -> usize {
        core::mem::size_of::<Self>() + self.delay.buffer.len() * core::mem::size_of::<f32>()
    }
}

impl Clone for DelayLineNode {
    fn clone(&self) -> Self {
        Self {
            delay: self.delay.clone(),
            delay_time: self.delay_time.handle(),
            feedback: self.feedback.handle(),
            mix: self.mix.handle(),
            interpolation: self.interpolation,
            sample_rate: self.sample_rate,
            max_delay: self.max_delay,
        }
    }
}

/// A left/right pair of `T`. Trivial helper, but removes `_l`/`_r` field
/// duplication across stereo DSP nodes.
#[derive(Debug, Clone)]
pub struct StereoPair<T> {
    pub l: T,
    pub r: T,
}

impl<T> StereoPair<T> {
    #[inline]
    pub const fn new(l: T, r: T) -> Self {
        Self { l, r }
    }
}

/// Stereo delay with per-channel delay times and cross-feedback.
///
/// # Port layout
///
/// The default node is 2-in / 2-out (audio on ports 0/1). For audio-rate
/// modulation it can grow optional param-input ports after the audio inputs
/// (see [`Self::with_param_inputs`]), in the order **feedback, then delay-time**.
/// A present `delay_time` port drives BOTH L and R delay times through one
/// shared input (the natural flanger/chorus control — the delay line
/// interpolates), while `feedback` overrides the feedback atomic. `cross_feedback`
/// and `mix` stay atomic-only. Each present port overrides its atomic per sample;
/// absent → a plain 2-in/2-out node, bit-identical to the unmodulated path and
/// zero added cost (the common case). Unconnected fundsp Net inputs read 0.0
/// every sample, so the ports are demand-built and never always-on.
pub struct StereoDelayLineNode {
    /// Per-channel delay lines; `delays.len()` == audio width. Built at
    /// construction — never resized in `tick`/`process` (RT no-alloc).
    delays: Vec<DelayLine>,
    /// Per-channel delay time. At width 2 the `[0]`/`[1]` entries are the L/R
    /// times; wider widths carry one per channel.
    delay_time: Vec<Param<Seconds>>,
    feedback: Param<Feedback>,
    /// Stereo-only: L↔R cross-feedback. Applied only at width 2; for wider
    /// widths there is no meaningful N-way cross-feed, so each channel uses
    /// self-feedback alone (documented).
    cross_feedback: Param<Feedback>,
    mix: Param<Mix>,
    interpolation: InterpolationMode,
    sample_rate: SampleRate,
    /// The longest delay this line can hold. Kept typed: it is a duration the
    /// setters clamp against, not scratch.
    max_delay: Seconds,
    /// When true, a feedback param-input port follows the audio inputs and
    /// overrides [`Self::feedback`] per sample.
    mod_feedback: bool,
    /// When true, a delay-time param-input port follows the feedback port (or
    /// the audio inputs if `mod_feedback` is false) and overrides ALL channel
    /// delay times per sample, routed through the interpolating read.
    mod_delay_time: bool,
}

impl StereoDelayLineNode {
    pub fn new(
        max_delay_secs: impl Into<Seconds>,
        delay_l_secs: impl Into<Seconds>,
        delay_r_secs: impl Into<Seconds>,
        feedback: impl Into<Feedback>,
    ) -> Self {
        let max_delay = max_delay_secs.into();
        let delay_l_secs = delay_l_secs.into();
        let delay_r_secs = delay_r_secs.into();
        let feedback = Feedback::new_clamped(feedback.into().get());
        Self {
            delays: vec![
                DelayLine::from_seconds(max_delay.get(), DEFAULT_SAMPLE_RATE),
                DelayLine::from_seconds(max_delay.get(), DEFAULT_SAMPLE_RATE),
            ],
            delay_time: vec![Param::new(delay_l_secs), Param::new(delay_r_secs)],
            feedback: Param::new(feedback),
            cross_feedback: Param::new(Feedback::NONE),
            mix: Param::new(Mix::WET),
            interpolation: InterpolationMode::Linear,
            sample_rate: DEFAULT_SAMPLE_RATE,
            max_delay,
            mod_feedback: false,
            mod_delay_time: false,
        }
    }

    /// An `n`-channel delay: each channel gets its own delay line and delay
    /// time (all seeded to `delay_secs`), with self-feedback. Cross-feedback is
    /// a stereo-only notion and is inert above width 2. `with_channels(2, …)`
    /// with equal L/R times matches [`Self::new`]; the `mix`/`feedback` surface
    /// is shared across all channels (one linked control).
    pub fn with_channels(
        channels: usize,
        max_delay_secs: impl Into<Seconds>,
        delay_secs: impl Into<Seconds>,
        feedback: impl Into<Feedback>,
    ) -> Self {
        let n = channels.max(1);
        let max_delay = max_delay_secs.into();
        let delay_secs = delay_secs.into();
        let feedback = Feedback::new_clamped(feedback.into().get());
        Self {
            delays: (0..n)
                .map(|_| DelayLine::from_seconds(max_delay.get(), DEFAULT_SAMPLE_RATE))
                .collect(),
            delay_time: (0..n).map(|_| Param::new(delay_secs)).collect(),
            feedback: Param::new(feedback),
            cross_feedback: Param::new(Feedback::NONE),
            mix: Param::new(Mix::WET),
            interpolation: InterpolationMode::Linear,
            sample_rate: DEFAULT_SAMPLE_RATE,
            max_delay,
            mod_feedback: false,
            mod_delay_time: false,
        }
    }

    /// Audio channel width (`inputs()` audio ports == `outputs()`).
    #[inline]
    fn width(&self) -> usize {
        self.delays.len()
    }

    /// A delay with optional audio-rate `feedback` / `delay_time` param-input
    /// ports, appended after the audio inputs in that order (feedback first).
    /// Each present port overrides its atomic per sample; the atomics still hold
    /// the base. A present `delay_time` port drives every channel's delay time
    /// (shared), through the interpolating read — the flanger / chorus path.
    ///
    /// Width and modulation are **independent axes**: `channels` says how wide
    /// the delay is, the `mod_*` flags say which params it reads at audio rate.
    /// They were not independent — this constructor delegated to [`Self::new`],
    /// which is width 2 — so asking for a modulated 5.1 delay silently returned
    /// a *stereo* one, and the only symptom was a `set_source` on a param port
    /// that resolved and carried the wrong signal.
    ///
    /// The param ports follow the audio inputs, so their indices **move with the
    /// width**. Ask [`ParamPorts::param_port`](crate::ParamPorts::param_port);
    /// never assume an index.
    ///
    /// This takes one `delay_secs` for every channel, matching
    /// [`Self::with_channels`], rather than the separate L/R times it used to:
    /// per-channel authored delay has no positional meaning above width 2. The
    /// stereo case is not lost, only moved past construction — set the two
    /// apart with [`Self::set_delay_time_l`] / [`Self::set_delay_time_r`].
    pub fn with_param_inputs(
        channels: usize,
        max_delay_secs: impl Into<Seconds>,
        delay_secs: impl Into<Seconds>,
        feedback: impl Into<Feedback>,
        mod_feedback: bool,
        mod_delay_time: bool,
    ) -> Self {
        let mut node = Self::with_channels(channels, max_delay_secs, delay_secs, feedback);
        node.mod_feedback = mod_feedback;
        node.mod_delay_time = mod_delay_time;
        node
    }

    /// Input-port index of the feedback param input, if present (right after
    /// the audio inputs).
    #[inline]
    pub fn feedback_port(&self) -> Option<usize> {
        self.mod_feedback.then_some(self.width())
    }

    /// Input-port index of the delay-time param input, if present (after the
    /// audio inputs and the feedback port).
    #[inline]
    pub fn delay_time_port(&self) -> Option<usize> {
        self.mod_delay_time
            .then_some(self.width() + self.mod_feedback as usize)
    }

    pub fn with_interpolation(mut self, mode: InterpolationMode) -> Self {
        self.interpolation = mode;
        self
    }

    pub fn delay_time_l(&self) -> Arc<AtomicF32> {
        self.delay_time[0].as_atomic()
    }

    pub fn delay_time_r(&self) -> Arc<AtomicF32> {
        self.delay_time[1.min(self.delay_time.len() - 1)].as_atomic()
    }

    pub fn feedback(&self) -> Arc<AtomicF32> {
        self.feedback.as_atomic()
    }

    pub fn cross_feedback(&self) -> Arc<AtomicF32> {
        self.cross_feedback.as_atomic()
    }

    pub fn mix(&self) -> Arc<AtomicF32> {
        self.mix.as_atomic()
    }

    pub fn set_cross_feedback(&self, cf: impl Into<Feedback>) {
        self.cross_feedback
            .store(Feedback::new_clamped(cf.into().get()));
    }

    /// Set all channel delay times to the same value.
    pub fn set_delay_time(&self, secs: impl Into<Seconds>) {
        let v = self.clamp_delay(secs.into());
        for dt in &self.delay_time {
            dt.store(v);
        }
    }

    pub fn set_delay_time_l(&self, secs: impl Into<Seconds>) {
        self.delay_time[0].store(self.clamp_delay(secs.into()));
    }

    pub fn set_delay_time_r(&self, secs: impl Into<Seconds>) {
        let idx = 1.min(self.delay_time.len() - 1);
        self.delay_time[idx].store(self.clamp_delay(secs.into()));
    }

    pub fn set_feedback(&self, fb: impl Into<Feedback>) {
        self.feedback.store(Feedback::new_clamped(fb.into().get()));
    }

    pub fn set_mix(&self, mix: impl Into<Mix>) {
        self.mix.store(Mix::new_clamped(mix.into().get()));
    }

    /// Constrain a requested delay into `0..=max_delay`. `Seconds` has no
    /// `clamp` of its own, so the bound is applied in the scalar space.
    #[inline]
    fn clamp_delay(&self, secs: Seconds) -> Seconds {
        Seconds(secs.get().clamp(0.0, self.max_delay.get()))
    }

    #[inline]
    fn snapshot_params(&self) -> StereoDelayParams {
        // `fb` and `cf` feed the SAME recirculation (see `process_stereo`:
        // `in_l + fb_l*fb + fb_r*cf`), so clamping each to `MAX_STABLE`
        // independently still admits a combined 1.98 and a runaway loop.
        // `stable_pair` bounds the sum, scaling both to preserve their ratio.
        let (fb, cf) =
            Feedback::stable_pair(self.feedback.load().get(), self.cross_feedback.load().get());
        StereoDelayParams {
            dl: fractional_samples(self.delay_time[0].load(), self.sample_rate),
            dr: fractional_samples(self.delay_time[1].load(), self.sample_rate),
            fb: fb.get(),
            cf: cf.get(),
            mix: self.mix.load(),
            interp: self.interpolation,
        }
    }

    /// Per-sample effective params for the modulated path (stereo cross-feed
    /// version): a present feedback / delay-time port overrides its atomic,
    /// applying the same clamp the setter uses. `cross_feedback` / `mix` are
    /// always read from their atomics. The shared delay-time port drives BOTH L
    /// and R (in samples), routed through the interpolating read. `read` reads
    /// input port `p`.
    #[inline]
    fn effective_params(&self, read: impl Fn(usize) -> f32) -> StereoDelayParams {
        let fb = self.feedback_port().map_or_else(
            || self.feedback.load(),
            // An audio-rate port bypasses every constructor, so the
            // stability bound has to be reapplied here or a modulated
            // feedback can be driven past unity.
            |p| Feedback::new_clamped(read(p)),
        );
        let (dl, dr) = match self.delay_time_port() {
            Some(p) => {
                let secs = Seconds(read(p).clamp(0.0, self.max_delay.get()));
                let samples = fractional_samples(secs, self.sample_rate);
                (samples, samples)
            }
            None => (
                fractional_samples(self.delay_time[0].load(), self.sample_rate),
                fractional_samples(self.delay_time[1].load(), self.sample_rate),
            ),
        };
        // Bound the pair, not each half — same reason as `snapshot_params`.
        let (fb, cf) = Feedback::stable_pair(fb.get(), self.cross_feedback.load().get());
        StereoDelayParams {
            dl,
            dr,
            fb: fb.get(),
            cf: cf.get(),
            mix: self.mix.load(),
            interp: self.interpolation,
        }
    }

    /// The stereo (width-2) sample step: L↔R cross-feedback. Reads/writes
    /// `delays[0]` (L) and `delays[1]` (R). Bit-identical to the original.
    #[inline]
    fn process_sample(&mut self, in_l: f32, in_r: f32, p: &StereoDelayParams) -> (f32, f32) {
        let fb_l = self.delays[0].read_sample((p.dl.max(1.0) - 1.0).max(0.0), p.interp);
        let fb_r = self.delays[1].read_sample((p.dr.max(1.0) - 1.0).max(0.0), p.interp);

        self.delays[0].push_sample(in_l + fb_l * p.fb + fb_r * p.cf);
        self.delays[1].push_sample(in_r + fb_r * p.fb + fb_l * p.cf);

        let del_l = self.delays[0].read_sample(p.dl, p.interp);
        let del_r = self.delays[1].read_sample(p.dr, p.interp);

        (p.mix.blend(in_l, del_l), p.mix.blend(in_r, del_r))
    }

    /// The width > 2 sample step for one channel `c`: independent delay line
    /// with self-feedback only (no cross-feed — a stereo-only notion).
    #[inline]
    fn process_sample_channel(
        &mut self,
        c: usize,
        input: f32,
        d_samples: f32,
        fb: f32,
        mix: Mix,
        interp: InterpolationMode,
    ) -> f32 {
        let fb_sample = self.delays[c].read_sample((d_samples.max(1.0) - 1.0).max(0.0), interp);
        self.delays[c].push_sample(input + fb_sample * fb);
        let delayed = self.delays[c].read_sample(d_samples, interp);
        mix.blend(input, delayed)
    }

    /// Effective (delay-samples, feedback, mix) for a wide channel `c`, honoring
    /// a present feedback / delay-time port (the delay-time port drives every
    /// channel). `read` reads input port `p`.
    #[inline]
    fn wide_channel_params(&self, c: usize, read: impl Fn(usize) -> f32) -> (f32, f32, Mix) {
        let fb = self.feedback_port().map_or_else(
            || self.feedback.load(),
            // An audio-rate port bypasses every constructor, so the
            // stability bound has to be reapplied here or a modulated
            // feedback can be driven past unity.
            |p| Feedback::new_clamped(read(p)),
        );
        let d_samples = match self.delay_time_port() {
            Some(p) => fractional_samples(
                Seconds(read(p).clamp(0.0, self.max_delay.get())),
                self.sample_rate,
            ),
            None => fractional_samples(self.delay_time[c].load(), self.sample_rate),
        };
        (d_samples, fb.get(), self.mix.load())
    }
}

struct StereoDelayParams {
    dl: f32,
    dr: f32,
    fb: f32,
    cf: f32,
    mix: Mix,
    interp: InterpolationMode,
}

impl AudioUnit for StereoDelayLineNode {
    fn inputs(&self) -> usize {
        self.width() + self.mod_feedback as usize + self.mod_delay_time as usize
    }

    fn outputs(&self) -> usize {
        self.width()
    }

    fn reset(&mut self) {
        for d in &mut self.delays {
            d.reset();
        }
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        self.sample_rate = sample_rate;
        for d in &mut self.delays {
            *d = DelayLine::from_seconds(self.max_delay, sample_rate);
        }
    }

    #[inline]
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        if self.width() == 2 {
            // Stereo cross-feed path — bit-identical to the original unit.
            let params = if !self.mod_feedback && !self.mod_delay_time {
                self.snapshot_params()
            } else {
                self.effective_params(|p| input[p])
            };
            let (out_l, out_r) = self.process_sample(input[0], input[1], &params);
            output[0] = out_l;
            output[1] = out_r;
            return;
        }
        // Wide path: independent per-channel delay, self-feedback only.
        for c in 0..self.width() {
            let (d, fb, mix) = self.wide_channel_params(c, |p| input[p]);
            output[c] = self.process_sample_channel(c, input[c], d, fb, mix, self.interpolation);
        }
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        if self.width() == 2 {
            // Fast path: no param ports — snapshot once per block, bit-identical.
            if !self.mod_feedback && !self.mod_delay_time {
                let params = self.snapshot_params();
                for i in 0..size {
                    let (out_l, out_r) =
                        self.process_sample(input.at_f32(0, i), input.at_f32(1, i), &params);
                    output.set_f32(0, i, out_l);
                    output.set_f32(1, i, out_r);
                }
                return;
            }
            // Modulated path: read the active port(s) per sample.
            for i in 0..size {
                let params = self.effective_params(|p| input.at_f32(p, i));
                let (out_l, out_r) =
                    self.process_sample(input.at_f32(0, i), input.at_f32(1, i), &params);
                output.set_f32(0, i, out_l);
                output.set_f32(1, i, out_r);
            }
            return;
        }
        // Wide path: independent per-channel delay, self-feedback only.
        let interp = self.interpolation;
        for i in 0..size {
            for c in 0..self.width() {
                let (d, fb, mix) = self.wide_channel_params(c, |p| input.at_f32(p, i));
                let out = self.process_sample_channel(c, input.at_f32(c, i), d, fb, mix, interp);
                output.set_f32(c, i, out);
            }
        }
    }

    fn set(&mut self, setting: tutti_core::dsp::Setting) {
        if let Some((param, value)) = tutti_core::unit_param::from_setting(&setting) {
            match param {
                tutti_core::UnitParam::DelayTime => self.set_delay_time(value),
                tutti_core::UnitParam::Feedback => self.set_feedback(value),
                tutti_core::UnitParam::Wet => self.set_mix(value),
                _ => {}
            }
        }
    }

    fn get_id(&self) -> u64 {
        crate::node_id::STEREO_DELAY_LINE_ID
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
            // f64 throughout, as in `DelayLineNode::route` above.
            let d = self.delay_time[c].load().get() as f64 * self.sample_rate.get();
            out.set(c, input.at(c).delay(d));
        }
        out
    }

    fn footprint(&self) -> usize {
        core::mem::size_of::<Self>()
            + self
                .delays
                .iter()
                .map(|d| d.buffer.len() * core::mem::size_of::<f32>())
                .sum::<usize>()
    }
}

impl Clone for StereoDelayLineNode {
    fn clone(&self) -> Self {
        Self {
            delays: self.delays.clone(),
            delay_time: self.delay_time.iter().map(|p| p.handle()).collect(),
            feedback: self.feedback.handle(),
            cross_feedback: self.cross_feedback.handle(),
            mix: self.mix.handle(),
            interpolation: self.interpolation,
            sample_rate: self.sample_rate,
            max_delay: self.max_delay,
            mod_feedback: self.mod_feedback,
            mod_delay_time: self.mod_delay_time,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_delay_line_basic() {
        let mut dl = DelayLine::new(10);
        dl.push_sample(1.0);
        dl.push_sample(0.0);
        dl.push_sample(0.0);

        let val = dl.read_sample(2.0, InterpolationMode::None);
        assert!((val - 1.0).abs() < 0.001, "Expected 1.0, got {val}");
    }

    #[test]
    fn test_delay_line_linear_interpolation() {
        let mut dl = DelayLine::new(10);
        dl.push_sample(0.0);
        dl.push_sample(1.0);
        dl.push_sample(0.0);

        let val = dl.read_sample(1.5, InterpolationMode::Linear);
        assert!((val - 0.5).abs() < 0.001, "Expected 0.5, got {val}");
    }

    #[test]
    fn test_delay_line_cubic_interpolation() {
        let mut dl = DelayLine::new(10);
        for i in 0..5 {
            dl.push_sample(i as f32);
        }
        let val = dl.read_sample(1.5, InterpolationMode::CubicHermite);
        // Between samples 3 and 2 (delay 1 = sample 3, delay 2 = sample 2)
        assert!(
            val > 2.0 && val < 4.0,
            "Cubic value {val} should be between recent samples"
        );
    }

    #[test]
    fn test_delay_line_reset() {
        let mut dl = DelayLine::new(10);
        dl.push_sample(1.0);
        dl.push_sample(1.0);
        dl.reset();
        let val = dl.read_sample(1.0, InterpolationMode::None);
        assert!((val).abs() < 0.001, "After reset, should read 0.0");
    }

    #[test]
    fn test_delay_node_passthrough_no_feedback() {
        let mut node = DelayLineNode::new(1.0, 0.0, 0.0);
        node.set_sample_rate(tutti_core::SampleRate(44100.0));
        node.set_mix(0.0);

        let mut output = [0.0f32];
        node.tick(&[0.5], &mut output);
        assert!(
            (output[0] - 0.5).abs() < 0.001,
            "Dry-only should pass through input"
        );
    }

    #[test]
    fn test_delay_node_echoes() {
        let sr = 1000.0;
        let delay_secs = 0.01; // 10 samples
        let mut node = DelayLineNode::new(1.0, delay_secs, 0.5);
        node.set_sample_rate(tutti_core::SampleRate(sr));

        // Send impulse
        let mut output = [0.0f32];
        node.tick(&[1.0], &mut output);

        // Advance past delay
        for _ in 0..9 {
            node.tick(&[0.0], &mut output);
        }

        // At sample 10, we should see the delayed signal
        node.tick(&[0.0], &mut output);
        assert!(
            output[0].abs() > 0.3,
            "Should hear echo at delay time, got {}",
            output[0]
        );
    }

    #[test]
    fn test_stereo_delay_independent_channels() {
        let sr = 1000.0;
        let mut node = StereoDelayLineNode::new(1.0, 0.005, 0.01, 0.0);
        node.set_sample_rate(tutti_core::SampleRate(sr));

        let mut out = [0.0f32; 2];
        node.tick(&[1.0, 1.0], &mut out);

        // After 5 samples, left should echo; right should not yet
        for _ in 0..4 {
            node.tick(&[0.0, 0.0], &mut out);
        }
        node.tick(&[0.0, 0.0], &mut out);
        let left_5 = out[0];

        // After 10 samples total, right should echo
        for _ in 0..4 {
            node.tick(&[0.0, 0.0], &mut out);
        }
        node.tick(&[0.0, 0.0], &mut out);
        let right_10 = out[1];

        assert!(left_5.abs() > 0.5, "Left echo at 5 samples: {left_5}");
        assert!(right_10.abs() > 0.5, "Right echo at 10 samples: {right_10}");
    }

    #[test]
    fn test_stereo_delay_cross_feedback() {
        let sr = 1000.0;
        let mut node = StereoDelayLineNode::new(1.0, 0.01, 0.01, 0.0);
        node.set_sample_rate(tutti_core::SampleRate(sr));
        node.set_cross_feedback(0.5);

        // Send impulse only on left
        let mut out = [0.0f32; 2];
        node.tick(&[1.0, 0.0], &mut out);

        // After delay, right channel should have cross-fed signal
        for _ in 0..9 {
            node.tick(&[0.0, 0.0], &mut out);
        }
        node.tick(&[0.0, 0.0], &mut out);
        let right_at_delay = out[1];

        // After another delay period, right should have picked up left's cross-feedback
        for _ in 0..9 {
            node.tick(&[0.0, 0.0], &mut out);
        }
        node.tick(&[0.0, 0.0], &mut out);

        assert!(
            right_at_delay.abs() > 0.01 || out[1].abs() > 0.01,
            "Cross-feedback should produce signal in right channel"
        );
    }

    #[test]
    fn test_delay_node_footprint() {
        let node = DelayLineNode::new(2.0, 0.5, 0.5);
        assert!(node.footprint() > core::mem::size_of::<DelayLineNode>());
    }

    // ── Width-native (N-channel) ─────────────────────────────────────────────

    #[test]
    fn delay_with_channels_2_matches_new() {
        // with_channels(2, ...) with equal times == new(...) with equal L/R.
        let mut a = StereoDelayLineNode::new(1.0, 0.01, 0.01, 0.4);
        a.set_sample_rate(tutti_core::SampleRate(48_000.0));
        let mut b = StereoDelayLineNode::with_channels(2, 1.0, 0.01, 0.4);
        b.set_sample_rate(tutti_core::SampleRate(48_000.0));

        let mut oa = [0.0f32; 2];
        let mut ob = [0.0f32; 2];
        for i in 0..2000 {
            let x = if i == 0 { 1.0 } else { 0.0 };
            a.tick(&[x, x], &mut oa);
            b.tick(&[x, x], &mut ob);
            assert_eq!(oa[0].to_bits(), ob[0].to_bits(), "L bit-diff at {i}");
            assert_eq!(oa[1].to_bits(), ob[1].to_bits(), "R bit-diff at {i}");
        }
    }

    #[test]
    fn delay_with_channels_reports_arity() {
        let d = StereoDelayLineNode::with_channels(6, 1.0, 0.01, 0.3);
        assert_eq!(d.inputs(), 6);
        assert_eq!(d.outputs(), 6);
    }

    #[test]
    fn wide_delay_channels_are_independent_no_crossfeed() {
        // 6-channel delay: an impulse on channel 3 echoes only on channel 3,
        // and cross_feedback (a stereo-only notion) is inert.
        let sr = 1000.0;
        let mut node = StereoDelayLineNode::with_channels(6, 1.0, 0.01, 0.0);
        node.set_sample_rate(tutti_core::SampleRate(sr));
        node.set_cross_feedback(0.9); // must have NO effect above width 2

        let mut inbuf = [0.0f32; 6];
        let mut outbuf = [0.0f32; 6];
        inbuf[3] = 1.0;
        node.tick(&inbuf, &mut outbuf);
        inbuf[3] = 0.0;

        // Advance to the 10-sample delay (0.01s @ 1000Hz).
        let mut ch3_echo = 0.0f32;
        let mut other_energy = 0.0f32;
        for _ in 0..12 {
            node.tick(&inbuf, &mut outbuf);
            ch3_echo = ch3_echo.max(outbuf[3].abs());
            for c in [0usize, 1, 2, 4, 5] {
                other_energy += outbuf[c] * outbuf[c];
            }
        }
        assert!(ch3_echo > 0.5, "ch3 should echo; peak {ch3_echo}");
        assert!(
            other_energy < 1e-10,
            "no cross-feed above width 2; other-channel energy {other_energy}"
        );
    }

    // ── Audio-rate param-input ports ─────────────────────────────────────────

    /// Tick a stereo delay sample by sample, appending `params` (the param-port
    /// values) after the two audio inputs each sample.
    fn process_stereo_delay(
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
    fn stereo_delay_default_is_two_in_no_ports() {
        let u = StereoDelayLineNode::new(1.0, 0.01, 0.01, 0.5);
        assert_eq!(u.inputs(), 2);
        assert_eq!(u.outputs(), 2);
        assert_eq!(u.feedback_port(), None);
        assert_eq!(u.delay_time_port(), None);
    }

    #[test]
    fn stereo_delay_param_port_arity_and_indices() {
        // feedback only → port 2 (delay-time absent).
        let f = StereoDelayLineNode::with_param_inputs(2, 1.0, 0.01, 0.5, true, false);
        assert_eq!(f.inputs(), 3);
        assert_eq!(f.feedback_port(), Some(2));
        assert_eq!(f.delay_time_port(), None);
        // delay-time only → port 2 (no feedback port before it).
        let d = StereoDelayLineNode::with_param_inputs(2, 1.0, 0.01, 0.5, false, true);
        assert_eq!(d.inputs(), 3);
        assert_eq!(d.feedback_port(), None);
        assert_eq!(d.delay_time_port(), Some(2));
        // both → feedback at 2, delay-time at 3.
        let b = StereoDelayLineNode::with_param_inputs(2, 1.0, 0.01, 0.5, true, true);
        assert_eq!(b.inputs(), 4);
        assert_eq!(b.feedback_port(), Some(2));
        assert_eq!(b.delay_time_port(), Some(3));
    }

    #[test]
    fn feedback_port_modulates() {
        // An impulse through the delay: a higher feedback via the port produces
        // a longer / louder tail than a low one. Proves the port drives the
        // feedback per sample.
        let sr = 1000.0;
        let delay_secs = 0.01; // 10 samples
        let n = 200;

        let run = |fb: f32| -> f32 {
            let mut node =
                StereoDelayLineNode::with_param_inputs(2, 1.0, delay_secs, 0.0, true, false);
            node.set_sample_rate(tutti_core::SampleRate(sr));
            let mut l = vec![0.0f32; n];
            let mut r = vec![0.0f32; n];
            l[0] = 1.0;
            r[0] = 1.0;
            // feedback held on port 2.
            let (out_l, _) = process_stereo_delay(&mut node, &l, &r, &[fb]);
            // Late-buffer energy (well past the first echo) — feedback governs
            // how much survives.
            out_l[50..].iter().map(|s| s * s).sum()
        };

        let low = run(0.1);
        let high = run(0.9);
        assert!(
            high > low * 2.0,
            "higher feedback via param port should yield a longer/louder tail: low={low}, high={high}"
        );
    }

    #[test]
    fn unmodulated_matches_held_constant() {
        // A modulated node whose ports are held at the atomic values must
        // produce the same output as a plain node — the modulated path is a
        // faithful superset.
        let sr = 1000.0;
        let delay_secs = 0.01;
        let fb = 0.5;
        let n = 256;

        let mut input_l = vec![0.0f32; n];
        let mut input_r = vec![0.0f32; n];
        for i in 0..n {
            input_l[i] = ((i * 7 + 3) % 100) as f32 / 50.0 - 1.0;
            input_r[i] = ((i * 5 + 1) % 100) as f32 / 50.0 - 1.0;
        }

        let mut plain = StereoDelayLineNode::new(1.0, delay_secs, delay_secs, fb);
        plain.set_sample_rate(tutti_core::SampleRate(sr));
        let (plain_l, plain_r) = process_stereo_delay(&mut plain, &input_l, &input_r, &[]);

        // Both ports present, held at the atomic values (feedback then delay-time).
        let mut modn = StereoDelayLineNode::with_param_inputs(2, 1.0, delay_secs, fb, true, true);
        modn.set_sample_rate(tutti_core::SampleRate(sr));
        let (mod_l, mod_r) = process_stereo_delay(&mut modn, &input_l, &input_r, &[fb, delay_secs]);

        for i in 0..n {
            assert!(
                (plain_l[i] - mod_l[i]).abs() < 1e-5,
                "L modulated-held diverges from plain at sample {i}: {} vs {}",
                plain_l[i],
                mod_l[i]
            );
            assert!(
                (plain_r[i] - mod_r[i]).abs() < 1e-5,
                "R modulated-held diverges from plain at sample {i}: {} vs {}",
                plain_r[i],
                mod_r[i]
            );
        }
    }

    /// Width and modulation are independent axes.
    ///
    /// The regression for the bug this constructor had: it delegated to
    /// `Self::new`, which is width 2, so a modulated 6-channel delay came back
    /// *stereo*. The arity assertion fails against that version.
    #[test]
    fn a_modulated_delay_is_as_wide_as_it_was_asked_for() {
        let d = StereoDelayLineNode::with_param_inputs(6, 1.0, 0.01, 0.5, true, true);
        assert_eq!(d.outputs(), 6, "the width is what was asked for");
        assert_eq!(
            d.inputs(),
            8,
            "six audio inputs, then feedback and delay-time"
        );
        assert_eq!(
            d.feedback_port(),
            Some(6),
            "param ports follow the audio inputs, so their indices move with the width"
        );
        assert_eq!(
            d.delay_time_port(),
            Some(7),
            "and keep their documented order"
        );
    }
}
