use tutti_core::Arc;
use tutti_core::AtomicF32;
use tutti_core::{AudioUnit, BufferMut, BufferRef, SignalFrame};

use tutti_core::{Feedback, Mix, Param, SampleRate, Seconds};

pub(crate) use crate::InterpolationMode;

/// A fractional-delay ring buffer: push a sample per frame, read back at any
/// real-valued delay.
///
/// The read position is fractional and interpolated
/// ([`InterpolationMode`]), which is what makes it usable as the core of a
/// modulated delay. Capacity is fixed at construction — [`read_sample`] clamps
/// rather than growing, so the line never allocates on the audio thread.
///
/// [`read_sample`]: Self::read_sample
#[derive(Clone)]
pub struct DelayLine {
    pub(crate) buffer: Vec<f32>,
    write_pos: usize,
    max_delay_samples: usize,
}

impl DelayLine {
    /// Allocates a line holding up to `max_delay_samples` frames of history.
    ///
    /// The backing buffer is one frame longer than the maximum delay, so that
    /// an interpolating read at exactly `max_delay_samples` still has a second
    /// tap to blend toward. Allocates — build before going live.
    pub fn new(max_delay_samples: usize) -> Self {
        Self {
            buffer: vec![0.0; max_delay_samples + 1],
            write_pos: 0,
            max_delay_samples,
        }
    }

    /// Allocates a line sized for `max_delay_secs` at `sample_rate`.
    ///
    /// The length is rounded **up**, so the line always holds at least the
    /// requested duration. Note that the size is baked in here: a node changing
    /// its sample rate has to rebuild the line, which is what
    /// `AudioUnit::set_sample_rate` does.
    pub fn from_seconds(
        max_delay_secs: impl Into<Seconds>,
        sample_rate: impl Into<SampleRate>,
    ) -> Self {
        // `to_samples_ceil`, not a nearest-rounding cast: a line sized for
        // `max_delay` must hold *at least* that long, and rounding to nearest
        // under-allocates for half of all inputs. The multiply stays in f64 —
        // narrowing the sample rate to f32 first loses precision.
        let samples = max_delay_secs
            .into()
            .to_samples_ceil(sample_rate.into().get());
        Self::new(samples.get())
    }

    /// Writes one frame at the cursor and advances, overwriting the oldest
    /// sample.
    ///
    /// Call exactly once per frame: the cursor *is* the line's clock, so a
    /// skipped or doubled push shifts every subsequent read by that much.
    pub fn push_sample(&mut self, sample: f32) {
        self.buffer[self.write_pos] = sample;
        self.write_pos += 1;
        if self.write_pos >= self.buffer.len() {
            self.write_pos = 0;
        }
    }

    /// Reads the line `delay_samples` frames back from the newest write,
    /// interpolating per `mode`.
    ///
    /// `delay_samples` is a **fractional frame count**, not [`Seconds`] — the
    /// caller converts, because keeping the fraction is the whole point of an
    /// interpolated read. `0.0` is the sample just pushed. Where the unit types
    /// stop: `Samples` is an integer count and cannot carry the fraction, and
    /// `SamplePosition` is an absolute f64 offset into a wave, where this is a
    /// per-sample f32 span back from the write head.
    ///
    /// The delay is clamped to `0.0..=max_delay_samples`, so an over-long
    /// request shortens silently rather than panicking. Does not allocate.
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

    /// Zeroes the line and returns the cursor to 0, dropping any tail still in
    /// flight.
    ///
    /// Does not reallocate, so it is safe on the audio thread.
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
/// The multiply stays in `f64` and narrows once at the end. Narrowing the
/// *rate* first (`secs * sample_rate as f32`) computes the whole position at
/// `f32` precision, which is coarser than the fractional read can resolve.
#[inline]
fn fractional_samples(secs: Seconds, sample_rate: SampleRate) -> f32 {
    (secs.get() as f64 * sample_rate.get()) as f32
}

/// Mono delay with feedback: 1 audio input, 1 output.
///
/// [`Seconds`] delay time, [`Feedback`] recirculation and wet/dry [`Mix`] are
/// all live [`Param`]s shared across clones, so a write reaches a node already
/// in the graph. All three are read **once per block** in `process`, so
/// automation lands at block granularity; for sample-accurate delay-time
/// modulation use [`StereoDelayLineNode`]'s param-input ports.
///
/// The maximum delay is baked at construction and the line is never
/// reallocated; a longer request is clamped at the read
/// ([`DelayLine::read_sample`]) rather than rejected.
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
    /// Builds a mono delay: a line sized for `max_delay_secs`, an initial
    /// `delay_secs` tap and `feedback` recirculation.
    ///
    /// `feedback` is clamped to the stable range — at or past unity a delay
    /// self-oscillates and grows without bound. `mix` starts fully wet
    /// ([`Mix::WET`]); set it for a parallel send.
    ///
    /// **Starts at the placeholder [`SampleRate::DEFAULT`]**: the line is sized
    /// there and rebuilt by [`AudioUnit::set_sample_rate`], which is what makes
    /// `max_delay_secs` hold at whatever rate the graph ends up running at. Call
    /// that setter before the first `process` — skip it at 48 kHz and every tap
    /// lands 8.8% short (a 500 ms echo returns at 459 ms), audibly wrong but not
    /// detectably so. See the crate-level "born at a placeholder rate" section.
    /// Allocates, and `set_sample_rate` reallocates.
    ///
    /// [`SampleRate::DEFAULT`]: tutti_core::SampleRate::DEFAULT
    /// [`AudioUnit::set_sample_rate`]: tutti_core::AudioUnit::set_sample_rate
    pub fn new(
        max_delay_secs: impl Into<Seconds>,
        delay_secs: impl Into<Seconds>,
        feedback: impl Into<Feedback>,
    ) -> Self {
        let max_delay = max_delay_secs.into();
        let delay_secs = delay_secs.into();
        let feedback = Feedback::new_clamped(feedback.into().get());
        Self {
            delay: DelayLine::from_seconds(max_delay.get(), SampleRate::DEFAULT),
            delay_time: Param::new(delay_secs),
            feedback: Param::new(feedback),
            mix: Param::new(Mix::WET),
            interpolation: InterpolationMode::Linear,
            sample_rate: SampleRate::DEFAULT,
            max_delay,
        }
    }

    /// Selects the fractional-read [`InterpolationMode`], replacing the
    /// [`Linear`](InterpolationMode::Linear) default.
    ///
    /// Baked at construction — there is no live setter, because the mode is a
    /// build-time quality choice rather than something to automate.
    pub fn with_interpolation(mut self, mode: InterpolationMode) -> Self {
        self.interpolation = mode;
        self
    }

    /// The shared [`Seconds`] delay-time cell, for driving the delay time from
    /// a modulator.
    ///
    /// Read once per block, so writes land at block granularity. Values past
    /// the constructed maximum are clamped at the read rather than refused.
    /// Shared across clones, so a write reaches the live node.
    pub fn delay_time(&self) -> Arc<AtomicF32> {
        self.delay_time.as_atomic()
    }

    /// The shared [`Feedback`] cell governing how much of the delayed signal
    /// recirculates.
    ///
    /// Writing the raw cell **bypasses the stability clamp** that
    /// [`set_feedback`](Self::set_feedback) applies: at or past unity the delay
    /// self-oscillates and grows without bound. Prefer the setter unless the
    /// writer bounds the value itself.
    pub fn feedback(&self) -> Arc<AtomicF32> {
        self.feedback.as_atomic()
    }

    /// The shared wet/dry [`Mix`] cell: `0.0` is the dry input untouched,
    /// `1.0` the delayed signal alone.
    ///
    /// Shared across clones, so a write reaches the live node.
    pub fn mix(&self) -> Arc<AtomicF32> {
        self.mix.as_atomic()
    }

    /// Sets the delay time in [`Seconds`], floored at 0.
    ///
    /// **Not** clamped to the constructed maximum — a longer request is
    /// shortened at the read instead, so it takes effect as the longest delay
    /// the line can hold.
    pub fn set_delay_time(&self, secs: impl Into<Seconds>) {
        self.delay_time.store(Seconds(secs.into().get().max(0.0)));
    }

    /// Sets the [`Feedback`] amount, clamped to the stable range.
    ///
    /// The clamp is what keeps a delay from self-oscillating; it is why this is
    /// the preferred path over writing [`feedback`](Self::feedback) directly.
    pub fn set_feedback(&self, fb: impl Into<Feedback>) {
        self.feedback.store(Feedback::new_clamped(fb.into().get()));
    }

    /// Sets the wet/dry [`Mix`], clamped to `0.0..=1.0`.
    pub fn set_mix(&self, mix: impl Into<Mix>) {
        self.mix.store(Mix::new_clamped(mix.into().get()));
    }

    /// `fb` is a [`Feedback`] beside an already-typed [`Mix`]. `input` and
    /// `delay_samples` stay raw: a sample value and a fractional read position
    /// are per-sample scratch feeding an interpolating read, not roster
    /// quantities.
    #[inline]
    fn process_sample(&mut self, input: f32, delay_samples: f32, fb: Feedback, mix: Mix) -> f32 {
        let feedback_tap = self
            .delay
            .read_sample(delay_samples.max(1.0) - 1.0, self.interpolation);
        self.delay.push_sample(input + feedback_tap * fb.get());
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
        let fb = self.feedback.load();
        let mix = self.mix.load();
        output[0] = self.process_sample(input[0], delay_samples, fb, mix);
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        let delay_samples = fractional_samples(self.delay_time.load(), self.sample_rate);
        let fb = self.feedback.load();
        let mix = self.mix.load();

        for i in 0..size {
            let in_s = input.at_f32(0, i);
            output.set_f32(0, i, self.process_sample(in_s, delay_samples, fb, mix));
        }
    }

    fn set(&mut self, setting: tutti_core::Setting) {
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

    /// Zero latency, whatever the delay time: the echo is the *effect*, not
    /// processing latency.
    ///
    /// `AudioUnit::latency` is derived from `route`, and PDC compensates
    /// whatever it reports by delaying every other path. This used to report
    /// `input.delay(delay_time)`, so a 500 ms echo insert pushed the whole rest
    /// of the mix 500 ms late to "line up" with an echo that is supposed to be
    /// late (design doc 013, D1). The dry half of the blend is undelayed, so the
    /// output's earliest energy leaves with the input.
    ///
    /// `distort` rather than a pass-through: a recirculating, modulatable delay
    /// has no fixed frequency response to report, and the blend with `mix` means
    /// a constant input does not come out unchanged either.
    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let mut out = SignalFrame::new(1);
        out.set(0, input.at(0).distort(0.0));
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
    /// The left-channel value.
    pub l: T,
    /// The right-channel value.
    pub r: T,
}

impl<T> StereoPair<T> {
    /// Pairs `l` and `r` in channel order.
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
    /// Builds a stereo delay with independent left and right delay times.
    ///
    /// Different L/R times are the classic ping-pong / widening setup. Both
    /// lines are sized for `max_delay_secs`; `feedback` is clamped to the
    /// stable range and applies to each channel's own recirculation.
    /// Cross-feedback starts at [`Feedback::NONE`] — set it with
    /// [`set_cross_feedback`](Self::set_cross_feedback) — and `mix` fully wet.
    ///
    /// **Starts at the placeholder [`SampleRate::DEFAULT`]**; call
    /// [`AudioUnit::set_sample_rate`] before the first `process` or both taps
    /// land 8.8% short at 48 kHz — plausible-sounding, wrong audio rather than
    /// an error. See the crate-level "born at a placeholder rate" section.
    /// Allocates, and `set_sample_rate` reallocates.
    ///
    /// [`SampleRate::DEFAULT`]: tutti_core::SampleRate::DEFAULT
    /// [`AudioUnit::set_sample_rate`]: tutti_core::AudioUnit::set_sample_rate
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
                DelayLine::from_seconds(max_delay.get(), SampleRate::DEFAULT),
                DelayLine::from_seconds(max_delay.get(), SampleRate::DEFAULT),
            ],
            delay_time: vec![Param::new(delay_l_secs), Param::new(delay_r_secs)],
            feedback: Param::new(feedback),
            cross_feedback: Param::new(Feedback::NONE),
            mix: Param::new(Mix::WET),
            interpolation: InterpolationMode::Linear,
            sample_rate: SampleRate::DEFAULT,
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
    ///
    /// **Starts at the placeholder [`SampleRate::DEFAULT`]** like
    /// [`Self::new`]: every line is sized there, so call
    /// [`AudioUnit::set_sample_rate`] before the first `process` or all `n`
    /// taps land 8.8% short at 48 kHz. See the crate-level "born at a
    /// placeholder rate" section. Allocates, and `set_sample_rate` reallocates.
    ///
    /// [`SampleRate::DEFAULT`]: tutti_core::SampleRate::DEFAULT
    /// [`AudioUnit::set_sample_rate`]: tutti_core::AudioUnit::set_sample_rate
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
                .map(|_| DelayLine::from_seconds(max_delay.get(), SampleRate::DEFAULT))
                .collect(),
            delay_time: (0..n).map(|_| Param::new(delay_secs)).collect(),
            feedback: Param::new(feedback),
            cross_feedback: Param::new(Feedback::NONE),
            mix: Param::new(Mix::WET),
            interpolation: InterpolationMode::Linear,
            sample_rate: SampleRate::DEFAULT,
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
    /// Collapsing them — building the modulated form at a fixed width 2 — turns
    /// a request for a modulated 5.1 delay into a *stereo* one, and the only
    /// symptom is a `set_source` on a param port that resolves and carries the
    /// wrong signal.
    ///
    /// The param ports follow the audio inputs, so their indices **move with the
    /// width**. Ask [`ParamPorts::param_port`](crate::ParamPorts::param_port);
    /// never assume an index.
    ///
    /// One `delay_secs` covers every channel, matching
    /// [`Self::with_channels`]: per-channel authored delay has no positional
    /// meaning above width 2. Set a stereo pair apart after construction with
    /// [`Self::set_delay_time_l`] / [`Self::set_delay_time_r`].
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

    /// Selects the fractional-read [`InterpolationMode`], replacing the
    /// [`Linear`](InterpolationMode::Linear) default. Baked at construction.
    pub fn with_interpolation(mut self, mode: InterpolationMode) -> Self {
        self.interpolation = mode;
        self
    }

    /// The shared [`Seconds`] delay-time cell for channel 0 (left).
    ///
    /// A present delay-time param-input port **overrides this per sample** —
    /// see [`with_param_inputs`](Self::with_param_inputs). Otherwise read once
    /// per block. Shared across clones.
    pub fn delay_time_l(&self) -> Arc<AtomicF32> {
        self.delay_time[0].as_atomic()
    }

    /// The shared [`Seconds`] delay-time cell for channel 1 (right).
    ///
    /// On a mono-width node this is channel 0's cell, so the call is safe at
    /// any width. Overridden per sample by a present delay-time port.
    pub fn delay_time_r(&self) -> Arc<AtomicF32> {
        self.delay_time[1.min(self.delay_time.len() - 1)].as_atomic()
    }

    /// The shared [`Feedback`] cell for each channel's own recirculation.
    ///
    /// Writing the raw cell bypasses the stability clamp
    /// [`set_feedback`](Self::set_feedback) applies. It is also bounded jointly
    /// with cross-feedback at read time, since both feed the same loop.
    pub fn feedback(&self) -> Arc<AtomicF32> {
        self.feedback.as_atomic()
    }

    /// The shared L↔R cross-[`Feedback`] cell: how much of each channel's delay
    /// recirculates into the *other*.
    ///
    /// **Stereo only.** Above width 2 there is no meaningful N-way cross-feed,
    /// so this is inert and each channel uses self-feedback alone.
    pub fn cross_feedback(&self) -> Arc<AtomicF32> {
        self.cross_feedback.as_atomic()
    }

    /// The shared wet/dry [`Mix`] cell, applied to every channel alike.
    pub fn mix(&self) -> Arc<AtomicF32> {
        self.mix.as_atomic()
    }

    /// Sets the L↔R cross-[`Feedback`], clamped to the stable range.
    ///
    /// Inert above width 2. Cross- and self-feedback feed the same
    /// recirculation, so they are additionally bounded *as a pair* at read
    /// time: clamping each to the stable maximum independently still admits a
    /// combined value that runs away.
    pub fn set_cross_feedback(&self, cf: impl Into<Feedback>) {
        self.cross_feedback
            .store(Feedback::new_clamped(cf.into().get()));
    }

    /// Sets every channel's delay time to the same value, clamped to
    /// `0..=max_delay`.
    ///
    /// The linked control — use it when the channels should track. For a
    /// ping-pong spread set the two apart with
    /// [`set_delay_time_l`](Self::set_delay_time_l) /
    /// [`set_delay_time_r`](Self::set_delay_time_r).
    pub fn set_delay_time(&self, secs: impl Into<Seconds>) {
        let v = self.clamp_delay(secs.into());
        for dt in &self.delay_time {
            dt.store(v);
        }
    }

    /// Sets channel 0's (left) delay time, clamped to `0..=max_delay`.
    pub fn set_delay_time_l(&self, secs: impl Into<Seconds>) {
        self.delay_time[0].store(self.clamp_delay(secs.into()));
    }

    /// Sets channel 1's (right) delay time, clamped to `0..=max_delay`.
    ///
    /// Falls back to channel 0 on a mono-width node, so the call is safe at any
    /// width.
    pub fn set_delay_time_r(&self, secs: impl Into<Seconds>) {
        let idx = 1.min(self.delay_time.len() - 1);
        self.delay_time[idx].store(self.clamp_delay(secs.into()));
    }

    /// Sets each channel's self-[`Feedback`], clamped to the stable range.
    ///
    /// Bounded jointly with cross-feedback at read time — see
    /// [`set_cross_feedback`](Self::set_cross_feedback).
    pub fn set_feedback(&self, fb: impl Into<Feedback>) {
        self.feedback.store(Feedback::new_clamped(fb.into().get()));
    }

    /// Sets the wet/dry [`Mix`] for every channel, clamped to `0.0..=1.0`.
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
    /// `delays[0]` (L) and `delays[1]` (R).
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
            // Stereo cross-feed path.
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
            // Fast path: no param ports — snapshot once per block.
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

    fn set(&mut self, setting: tutti_core::Setting) {
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
        // Zero latency per channel — see `DelayLineNode::route` for why a
        // musical delay must not report its delay time to PDC.
        for c in 0..self.width() {
            out.set(c, input.at(c).distort(0.0));
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
