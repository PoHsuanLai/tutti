//! Delay lines and the delay nodes built on them.
//!
//! [`DelayLine`] is the bare fractional-read ring; [`DelayLineNode`] wraps it
//! into a width-generic `AudioUnit` with feedback, an explicit cross-feedback
//! routing matrix, wet/dry [`Mix`] and optional audio-rate param-input ports. The fractional read is the
//! reason this is not just a [`CircularBuffer`](crate::buffer::CircularBuffer):
//! a delay whose time is modulated must interpolate between taps or it steps
//! audibly.

use tutti_core::Arc;
use tutti_core::AtomicF32;
use tutti_core::{AudioUnit, BufferMut, BufferRef, SignalFrame};

use tutti_core::{ChannelLayout, Feedback, Mix, Param, SampleRate, Seconds};

use crate::ramp::Ramp;

/// How a fractional delay position is turned into a sample.
///
/// The cost/quality ladder for a *modulated* delay: a moving delay time lands
/// between samples, and how that gap is filled is what separates a clean chorus
/// from a gritty one. At a fixed, integral delay all three agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InterpolationMode {
    /// Round to the nearest whole sample. Cheapest, and it quantises the delay
    /// to frame boundaries — a swept delay steps audibly ("zipper"). Fine for a
    /// static echo, wrong for chorus or flanger.
    None,
    /// Linear blend between the two neighbouring samples. The default: no
    /// stepping under modulation, at the cost of slight high-frequency damping
    /// that worsens as the fraction approaches 0.5.
    #[default]
    Linear,
    /// Four-point cubic Hermite. Keeps the highs that [`Linear`](Self::Linear)
    /// damps, for roughly four reads per sample instead of two. Reach for it on
    /// audibly-swept delays; it is wasted on a static one.
    CubicHermite,
}

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

/// The block's raw scalar controls — what the next block's ramps start from.
/// `fb`/`cf` are bounded as a pair per sample, after ramping.
#[derive(Clone, Copy, PartialEq)]
struct DelayControls {
    fb: f32,
    cf: f32,
    mix: f32,
}

/// Delay with feedback, of any width: `N` audio inputs, `N` outputs, one
/// delay line and one delay time per channel.
///
/// This used to be a mono `DelayLineNode` and a `StereoDelayLineNode` whose
/// L↔R cross-feedback existed only at exactly width 2 — a special case in the
/// sample loop. The cross-feed is now an explicit **routing matrix**: entry
/// `(c, j)` is how much of channel `j`'s delayed tap recirculates into channel
/// `c`'s line, scaled by the [`cross_feedback`](Self::cross_feedback) amount.
/// Width 2 defaults to the swap matrix `[[0, 1], [1, 0]]` — exactly the old
/// stereo cross-feed, bit for bit — and every other width to no routing, as
/// before; [`with_cross_feedback_matrix`](Self::with_cross_feedback_matrix)
/// routes any width.
///
/// [`Seconds`] delay times, [`Feedback`] recirculation, cross-feedback and
/// wet/dry [`Mix`] are live [`Param`]s shared across clones, all read **once
/// per block**. A value that moved since the last block is ramped linearly
/// across the block — a delay time glides (a brief pitch bend) rather than
/// jumping (a click), and a mix change fades rather than stepping. `tick` is a
/// block of one.
///
/// The maximum delay is baked at construction and the lines are never
/// reallocated on the audio thread; a longer request is clamped rather than
/// rejected.
///
/// # Port layout
///
/// `N` audio inputs, then optional param-input ports (see
/// [`Self::with_param_inputs`]) in the order **feedback, then delay-time**. A
/// present delay-time port drives *every* channel's delay time through one
/// shared input (the natural flanger/chorus control — the lines interpolate),
/// and a present feedback port overrides the feedback atomic; both are read
/// every sample, since they are audio signals. Cross-feedback and mix stay
/// atomic-only. Absent ports cost nothing, which is the common case.
pub struct DelayLineNode {
    /// Per-channel delay lines; `len()` is the audio width. Built at
    /// construction — never resized in `tick`/`process` (RT no-alloc).
    delays: Vec<DelayLine>,
    /// Per-channel delay time.
    delay_time: Vec<Param<Seconds>>,
    feedback: Param<Feedback>,
    cross_feedback: Param<Feedback>,
    /// `N×N`, row-major: `cross_routing[c * N + j]` is the weight of channel
    /// `j`'s tap into channel `c`'s line. Every row's absolute sum is at most
    /// 1, which with the stabilised `(fb, cf)` pair keeps each line's loop gain
    /// under [`Feedback::MAX_STABLE`].
    cross_routing: Vec<f32>,
    /// Whether any routing weight is non-zero. When none is, the channels are
    /// independent and render channel-outer.
    cross_routed: bool,
    mix: Param<Mix>,
    interpolation: InterpolationMode,
    sample_rate: SampleRate,
    /// The longest delay this line can hold. Kept typed: it is a duration the
    /// setters clamp against, not scratch.
    max_delay: Seconds,
    mod_feedback: bool,
    mod_delay_time: bool,
    /// The controls the previous block ended on — where this block's ramp
    /// starts. `None` until the first block (and after `reset`), which then
    /// starts on its targets rather than ramping in from nothing.
    last: Option<DelayControls>,
    /// Per-channel delay (fractional samples) the previous block ended on.
    last_delay: Vec<f32>,
    /// Per-channel delay ramp for the block, in fractional samples. Scratch,
    /// sized at construction.
    delay_ramps: Vec<Ramp>,
    /// Per-channel tap scratch for the cross-fed loop: every channel's
    /// feedback tap is read before any line is written. Sized at construction.
    taps: Vec<f32>,
}

impl DelayLineNode {
    /// A mono delay (1 in, 1 out): a line sized for `max_delay_secs`, an
    /// initial `delay_secs` tap and `feedback` recirculation.
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
        Self::with_channels(ChannelLayout::MONO, max_delay_secs, delay_secs, feedback)
    }

    /// A stereo delay with independent left and right delay times and the
    /// L↔R cross-feed routing.
    ///
    /// Different L/R times are the classic ping-pong / widening setup.
    /// Cross-feedback starts at [`Feedback::NONE`] — set it with
    /// [`set_cross_feedback`](Self::set_cross_feedback). Otherwise as
    /// [`new`](Self::new), placeholder rate included.
    pub fn stereo(
        max_delay_secs: impl Into<Seconds>,
        delay_l_secs: impl Into<Seconds>,
        delay_r_secs: impl Into<Seconds>,
        feedback: impl Into<Feedback>,
    ) -> Self {
        let node = Self::with_channels(
            ChannelLayout::STEREO,
            max_delay_secs,
            delay_l_secs,
            feedback,
        );
        node.set_delay_time_r(delay_r_secs);
        node
    }

    /// A delay `channels` wide (clamped to at least 1): each channel gets its
    /// own line and delay time (all seeded to `delay_secs`), with
    /// self-feedback. The `feedback`/`cross_feedback`/`mix` surface is shared
    /// across the channels (one linked control).
    ///
    /// Width 2 gets the L↔R swap routing, so `with_channels(STEREO, …)` is
    /// [`stereo`](Self::stereo) with equal times; any other width starts with
    /// no cross routing (cross-feedback inert) until
    /// [`with_cross_feedback_matrix`](Self::with_cross_feedback_matrix) gives
    /// it one. Placeholder rate as [`new`](Self::new); allocates.
    pub fn with_channels(
        channels: impl Into<ChannelLayout>,
        max_delay_secs: impl Into<Seconds>,
        delay_secs: impl Into<Seconds>,
        feedback: impl Into<Feedback>,
    ) -> Self {
        let n = usize::from(channels.into().count()).max(1);
        let max_delay = max_delay_secs.into();
        let delay_secs = Seconds(delay_secs.into().get().clamp(0.0, max_delay.get()));
        let feedback = Feedback::new_clamped(feedback.into().get());
        let mut cross_routing = vec![0.0; n * n];
        if n == 2 {
            cross_routing[1] = 1.0;
            cross_routing[2] = 1.0;
        }
        Self {
            delays: (0..n)
                .map(|_| DelayLine::from_seconds(max_delay, SampleRate::DEFAULT))
                .collect(),
            delay_time: (0..n).map(|_| Param::new(delay_secs)).collect(),
            feedback: Param::new(feedback),
            cross_feedback: Param::new(Feedback::NONE),
            cross_routed: n == 2,
            cross_routing,
            mix: Param::new(Mix::WET),
            interpolation: InterpolationMode::Linear,
            sample_rate: SampleRate::DEFAULT,
            max_delay,
            mod_feedback: false,
            mod_delay_time: false,
            last: None,
            last_delay: vec![0.0; n],
            delay_ramps: vec![Ramp::new(0.0, 0.0, 1); n],
            taps: vec![0.0; n],
        }
    }

    /// A delay with optional audio-rate `feedback` / `delay_time` param-input
    /// ports, appended after the audio inputs in that order (feedback first).
    /// Each present port overrides its atomic; the atomics still hold the base.
    /// A present `delay_time` port drives every channel's delay time (shared),
    /// through the interpolating read — the flanger / chorus path.
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
    pub fn with_param_inputs(
        channels: impl Into<ChannelLayout>,
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

    /// Replaces the cross-feed routing with `matrix`, `N×N` row-major: entry
    /// `matrix[c * N + j]` is how much of channel `j`'s delayed tap feeds back
    /// into channel `c`'s line, before the
    /// [`cross_feedback`](Self::cross_feedback) amount scales it.
    ///
    /// Each row is scaled down, if needed, so its absolute sum is at most 1:
    /// with self- and cross-feedback bounded as a pair, that keeps every line's
    /// loop gain inside [`Feedback::MAX_STABLE`] whatever the matrix says. The
    /// stereo default is `[0, 1, 1, 0]`; a ring (`c` fed by `c - 1`) spins
    /// echoes around a surround field.
    ///
    /// # Panics
    ///
    /// If `matrix.len()` is not `N²` — a build-time shape error, never an
    /// audio-thread one.
    pub fn with_cross_feedback_matrix(mut self, matrix: &[f32]) -> Self {
        let n = self.width();
        assert_eq!(
            matrix.len(),
            n * n,
            "a {n}-channel delay takes a {n}x{n} cross-feedback matrix"
        );
        for (row_out, row_in) in self.cross_routing.chunks_mut(n).zip(matrix.chunks(n)) {
            let sum: f32 = row_in.iter().map(|w| w.abs()).sum();
            let scale = if sum > 1.0 { 1.0 / sum } else { 1.0 };
            for (o, &w) in row_out.iter_mut().zip(row_in) {
                *o = w * scale;
            }
        }
        self.cross_routed = self.cross_routing.iter().any(|&w| w != 0.0);
        self
    }

    /// Audio channel width (`inputs()` audio ports == `outputs()`).
    #[inline]
    fn width(&self) -> usize {
        self.delays.len()
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
    /// [`Linear`](InterpolationMode::Linear) default.
    ///
    /// Baked at construction — there is no live setter, because the mode is a
    /// build-time quality choice rather than something to automate.
    pub fn with_interpolation(mut self, mode: InterpolationMode) -> Self {
        self.interpolation = mode;
        self
    }

    /// The shared [`Seconds`] delay-time cell of channel 0 — the whole delay's
    /// on a mono node, the left one's on a stereo node.
    ///
    /// Read once per block and ramped. A present delay-time param-input port
    /// **overrides** it. Shared across clones, so a write reaches the live
    /// node.
    pub fn delay_time(&self) -> Arc<AtomicF32> {
        self.delay_time[0].as_atomic()
    }

    /// The shared [`Seconds`] delay-time cell of channel 1 (right), falling
    /// back to channel 0 on a mono node so the call is safe at any width.
    pub fn delay_time_r(&self) -> Arc<AtomicF32> {
        self.delay_time[1.min(self.width() - 1)].as_atomic()
    }

    /// The shared [`Seconds`] delay-time cell of `channel`, clamped to the last
    /// channel.
    pub fn channel_delay_time(&self, channel: usize) -> Arc<AtomicF32> {
        self.delay_time[channel.min(self.width() - 1)].as_atomic()
    }

    /// The shared [`Feedback`] cell for each channel's own recirculation.
    ///
    /// Writing the raw cell bypasses the stability clamp
    /// [`set_feedback`](Self::set_feedback) applies; it is still bounded
    /// jointly with cross-feedback at read time, since both feed the same loop.
    pub fn feedback(&self) -> Arc<AtomicF32> {
        self.feedback.as_atomic()
    }

    /// The shared cross-[`Feedback`] cell: how much of the routed taps (see
    /// [`with_cross_feedback_matrix`](Self::with_cross_feedback_matrix))
    /// recirculates. On a stereo node that is L↔R; on a width with no routing
    /// it is inert.
    pub fn cross_feedback(&self) -> Arc<AtomicF32> {
        self.cross_feedback.as_atomic()
    }

    /// The shared wet/dry [`Mix`] cell, applied to every channel alike: `0.0`
    /// is the dry input untouched, `1.0` the delayed signal alone.
    pub fn mix(&self) -> Arc<AtomicF32> {
        self.mix.as_atomic()
    }

    /// Sets the cross-[`Feedback`], clamped to the stable range.
    ///
    /// Cross- and self-feedback feed the same recirculation, so they are
    /// additionally bounded *as a pair* at read time: clamping each to the
    /// stable maximum independently still admits a combined value that runs
    /// away.
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
        self.set_channel_delay_time(0, secs);
    }

    /// Sets channel 1's (right) delay time, clamped to `0..=max_delay`.
    ///
    /// Falls back to channel 0 on a mono node, so the call is safe at any
    /// width.
    pub fn set_delay_time_r(&self, secs: impl Into<Seconds>) {
        self.set_channel_delay_time(1, secs);
    }

    /// Sets `channel`'s delay time (clamped to the last channel), clamped to
    /// `0..=max_delay`.
    pub fn set_channel_delay_time(&self, channel: usize, secs: impl Into<Seconds>) {
        self.delay_time[channel.min(self.width() - 1)].store(self.clamp_delay(secs.into()));
    }

    /// Sets each channel's self-[`Feedback`], clamped to the stable range.
    ///
    /// The clamp is what keeps a delay from self-oscillating; it is why this is
    /// the preferred path over writing [`feedback`](Self::feedback) directly.
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
}

impl DelayLineNode {
    /// The one render kernel behind `tick` and `process`.
    ///
    /// Every control atomic is read once, here. `fb_port` / `dt_port` are the
    /// param-port signals for the block when those ports exist; they are audio
    /// and read per sample. Everything else ramps from where the previous block
    /// ended (see [`ramp`](crate::ramp)).
    fn render(
        &mut self,
        size: usize,
        x: impl Fn(usize, usize) -> f32,
        y: impl FnMut(usize, usize, f32),
        fb_port: Option<&[f32]>,
        dt_port: Option<&[f32]>,
    ) {
        let width = self.width();
        let interp = self.interpolation;
        let sr = self.sample_rate;
        let max = self.max_delay.get();

        let target = DelayControls {
            fb: self.feedback.load().get(),
            cf: if self.cross_routed {
                self.cross_feedback.load().get()
            } else {
                0.0
            },
            mix: self.mix.load().get(),
        };
        let from = self.last.unwrap_or(target);
        let fb_r = Ramp::new(from.fb, target.fb, size);
        let cf_r = Ramp::new(from.cf, target.cf, size);
        let mix_r = Ramp::new(from.mix, target.mix, size);

        // Per-channel delay ramps from the atomics. Unprimed (first block,
        // after `reset` or a rate change), the ramp starts on the target.
        let primed = self.last.is_some();
        let mut steady = fb_port.is_none()
            && dt_port.is_none()
            && fb_r.is_flat()
            && cf_r.is_flat()
            && mix_r.is_flat();
        for c in 0..width {
            let d = fractional_samples(self.delay_time[c].load(), sr);
            let from = if primed { self.last_delay[c] } else { d };
            self.delay_ramps[c] = Ramp::new(from, d, size);
            steady &= self.delay_ramps[c].is_flat();
        }
        let coupled = self.cross_routed && (from.cf != 0.0 || target.cf != 0.0);
        let lines = Lines {
            delays: &mut self.delays,
            taps: &mut self.taps,
            routing: &self.cross_routing,
            interp,
            coupled,
        };

        if steady {
            // Nothing moves this block: every control is a constant in the
            // loop, which is the common case and the one worth keeping tight.
            let (fb, cf) = Feedback::stable_pair(target.fb, target.cf);
            let (fb, cf, mix) = (fb.get(), cf.get(), target.mix);
            let ramps = &self.delay_ramps;
            lines.run(size, x, y, |_| (fb, cf), |c, _| ramps[c].at(0), |_| mix);
        } else {
            // Self- and cross-feedback feed the same loop, so they are bounded
            // as a pair — clamping each alone still admits a combined 1.98. A
            // feedback port bypasses every constructor, so its clamp is
            // reapplied here.
            let loop_gain = |i: usize| -> (f32, f32) {
                let fb = fb_port.map_or_else(|| fb_r.at(i), |s| Feedback::new_clamped(s[i]).get());
                let (fb, cf) = Feedback::stable_pair(fb, cf_r.at(i));
                (fb.get(), cf.get())
            };
            let ramps = &self.delay_ramps;
            // A delay-time port is audio and read per sample; the atomics ramp.
            let delay_at = |c: usize, i: usize| -> f32 {
                match dt_port {
                    Some(s) => fractional_samples(Seconds(s[i].clamp(0.0, max)), sr),
                    None => ramps[c].at(i),
                }
            };
            lines.run(size, x, y, loop_gain, delay_at, |i| mix_r.at(i));
        }

        // Where the next block's ramps start.
        for c in 0..width {
            self.last_delay[c] = match dt_port {
                Some(s) => fractional_samples(Seconds(s[size - 1].clamp(0.0, max)), sr),
                None => self.delay_ramps[c].at(size - 1),
            };
        }
        self.last = Some(target);
    }
}

/// The delay lines and the scratch their sample loop needs, borrowed apart
/// from the node so the per-block control closures can borrow the rest.
struct Lines<'a> {
    delays: &'a mut [DelayLine],
    taps: &'a mut [f32],
    routing: &'a [f32],
    interp: InterpolationMode,
    coupled: bool,
}

impl Lines<'_> {
    /// Render the block. `gain(i)` is the bounded `(fb, cf)` pair at sample
    /// `i`, `delay_at(c, i)` channel `c`'s delay in samples, `mix_at(i)` the
    /// wet/dry mix. Constant closures compile down to constants.
    #[inline(always)]
    fn run(
        self,
        size: usize,
        x: impl Fn(usize, usize) -> f32,
        mut y: impl FnMut(usize, usize, f32),
        gain: impl Fn(usize) -> (f32, f32),
        delay_at: impl Fn(usize, usize) -> f32,
        mix_at: impl Fn(usize) -> f32,
    ) {
        let interp = self.interp;
        if !self.coupled {
            // Independent channels: channel-outer, one line at a time.
            for (c, line) in self.delays.iter_mut().enumerate() {
                for i in 0..size {
                    let (fb, _) = gain(i);
                    let v = delay_step(line, x(c, i), delay_at(c, i), fb, mix_at(i), interp);
                    y(c, i, v);
                }
            }
            return;
        }
        // Cross-fed: every channel's feedback tap is read before any line is
        // written, so the loop is sample-outer.
        let n = self.delays.len();
        for i in 0..size {
            let (fb, cf) = gain(i);
            let mix = Mix(mix_at(i));
            for (c, line) in self.delays.iter().enumerate() {
                let d = delay_at(c, i);
                self.taps[c] = line.read_sample((d.max(1.0) - 1.0).max(0.0), interp);
            }
            for (c, line) in self.delays.iter_mut().enumerate() {
                let mut acc = x(c, i) + self.taps[c] * fb;
                for (j, &w) in self.routing[c * n..(c + 1) * n].iter().enumerate() {
                    if w != 0.0 {
                        acc += (w * cf) * self.taps[j];
                    }
                }
                line.push_sample(acc);
            }
            for (c, line) in self.delays.iter().enumerate() {
                let wet = line.read_sample(delay_at(c, i), interp);
                y(c, i, mix.blend(x(c, i), wet));
            }
        }
    }
}

/// One sample of one uncoupled channel: read the feedback tap, write the line,
/// read the output tap, blend.
#[inline(always)]
fn delay_step(
    line: &mut DelayLine,
    x: f32,
    d: f32,
    fb: f32,
    mix: f32,
    interp: InterpolationMode,
) -> f32 {
    let tap = line.read_sample((d.max(1.0) - 1.0).max(0.0), interp);
    line.push_sample(x + tap * fb);
    Mix(mix).blend(x, line.read_sample(d, interp))
}

impl AudioUnit for DelayLineNode {
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
        self.last = None;
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        self.sample_rate = sample_rate;
        for d in &mut self.delays {
            *d = DelayLine::from_seconds(self.max_delay, sample_rate);
        }
        // Delay positions are in samples of the old rate: start the next block
        // on its targets rather than gliding across a rate change.
        self.last = None;
    }

    #[inline]
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        // A block of one through the same kernel as `process`: the one-sample
        // ramps land on the new values at once, as the old per-sample read did.
        let fb = self.feedback_port().map(|p| &input[p..p + 1]);
        let dt = self.delay_time_port().map(|p| &input[p..p + 1]);
        self.render(1, |c, _| input[c], |c, _, v| output[c] = v, fb, dt);
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        if size == 0 {
            return;
        }
        let fb = self.feedback_port().map(|p| &input.channel_f32(p)[..size]);
        let dt = self
            .delay_time_port()
            .map(|p| &input.channel_f32(p)[..size]);
        self.render(
            size,
            |c, i| input.at_f32(c, i),
            |c, i, v| output.set_f32(c, i, v),
            fb,
            dt,
        );
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
        if self.inputs() == 1 {
            crate::node_id::DELAY_LINE_ID
        } else {
            crate::node_id::STEREO_DELAY_LINE_ID
        }
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
        let mut out = SignalFrame::new(self.width());
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

impl Clone for DelayLineNode {
    fn clone(&self) -> Self {
        Self {
            delays: self.delays.clone(),
            delay_time: self.delay_time.iter().map(|p| p.handle()).collect(),
            feedback: self.feedback.handle(),
            cross_feedback: self.cross_feedback.handle(),
            cross_routing: self.cross_routing.clone(),
            cross_routed: self.cross_routed,
            mix: self.mix.handle(),
            interpolation: self.interpolation,
            sample_rate: self.sample_rate,
            max_delay: self.max_delay,
            mod_feedback: self.mod_feedback,
            mod_delay_time: self.mod_delay_time,
            last: self.last,
            last_delay: self.last_delay.clone(),
            delay_ramps: self.delay_ramps.clone(),
            taps: self.taps.clone(),
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
        let mut node = DelayLineNode::stereo(1.0, 0.005, 0.01, 0.0);
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
        let mut node = DelayLineNode::stereo(1.0, 0.01, 0.01, 0.0);
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

    // ── Width-native (N-channel) ─────────────────────────────────────────────

    #[test]
    fn delay_with_channels_2_matches_new() {
        // with_channels(2, ...) with equal times == new(...) with equal L/R.
        let mut a = DelayLineNode::stereo(1.0, 0.01, 0.01, 0.4);
        a.set_sample_rate(tutti_core::SampleRate(48_000.0));
        let mut b = DelayLineNode::with_channels(ChannelLayout::STEREO, 1.0, 0.01, 0.4);
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
        let d = DelayLineNode::with_channels(6usize, 1.0, 0.01, 0.3);
        assert_eq!(d.inputs(), 6);
        assert_eq!(d.outputs(), 6);
    }

    #[test]
    fn wide_delay_channels_are_independent_no_crossfeed() {
        // 6-channel delay: an impulse on channel 3 echoes only on channel 3,
        // and cross_feedback (a stereo-only notion) is inert.
        let sr = 1000.0;
        let mut node = DelayLineNode::with_channels(6usize, 1.0, 0.01, 0.0);
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
    fn stereo_delay_param_port_arity_and_indices() {
        // Plain constructor: no ports, audio arity untouched.
        let d = DelayLineNode::stereo(1.0, 0.01, 0.01, 0.5);
        assert_eq!(d.inputs(), 2);
        assert_eq!(d.outputs(), 2);
        assert_eq!(d.feedback_port(), None);
        assert_eq!(d.delay_time_port(), None);
        // feedback only → port 2 (delay-time absent).
        let f =
            DelayLineNode::with_param_inputs(ChannelLayout::STEREO, 1.0, 0.01, 0.5, true, false);
        assert_eq!(f.inputs(), 3);
        assert_eq!(f.feedback_port(), Some(2));
        assert_eq!(f.delay_time_port(), None);
        // delay-time only → port 2 (no feedback port before it).
        let d =
            DelayLineNode::with_param_inputs(ChannelLayout::STEREO, 1.0, 0.01, 0.5, false, true);
        assert_eq!(d.inputs(), 3);
        assert_eq!(d.feedback_port(), None);
        assert_eq!(d.delay_time_port(), Some(2));
        // both → feedback at 2, delay-time at 3.
        let b = DelayLineNode::with_param_inputs(ChannelLayout::STEREO, 1.0, 0.01, 0.5, true, true);
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
            let mut node = DelayLineNode::with_param_inputs(
                ChannelLayout::STEREO,
                1.0,
                delay_secs,
                0.0,
                true,
                false,
            );
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

        let mut plain = DelayLineNode::stereo(1.0, delay_secs, delay_secs, fb);
        plain.set_sample_rate(tutti_core::SampleRate(sr));
        let (plain_l, plain_r) = process_stereo_delay(&mut plain, &input_l, &input_r, &[]);

        // Both ports present, held at the atomic values (feedback then delay-time).
        let mut modn = DelayLineNode::with_param_inputs(
            ChannelLayout::STEREO,
            1.0,
            delay_secs,
            fb,
            true,
            true,
        );
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
    /// Building the modulated form at a fixed width 2 makes a 6-channel request
    /// come back *stereo*; the arity assertion is what catches it.
    #[test]
    fn a_modulated_delay_is_as_wide_as_it_was_asked_for() {
        let d = DelayLineNode::with_param_inputs(6usize, 1.0, 0.01, 0.5, true, true);
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

    // ── Per-block reads, routing and width ───────────────────────────────────

    fn slow_sine(len: usize) -> Vec<f32> {
        (0..len)
            .map(|i| (core::f32::consts::TAU * 3.0 * i as f32 / 48_000.0).sin())
            .collect()
    }

    /// A delay-time change made between blocks glides across the next block —
    /// a brief pitch bend — instead of jumping (a click), and the block ends on
    /// the new time.
    ///
    /// Mutation: starting the delay ramp at the target (`last_delay[c] = d`
    /// even when primed) fails `assert_ramps_in`.
    #[test]
    fn a_delay_time_change_glides_across_the_next_block() {
        use crate::test_support::change_between_blocks;
        let x = slow_sine(4_096 + 64);
        let run = change_between_blocks(
            || {
                let mut n = DelayLineNode::stereo(1.0, 0.010, 0.012, 0.0);
                n.set_sample_rate(tutti_core::SampleRate(48_000.0));
                n
            },
            |n| n.set_delay_time(0.200),
            &[&x[..4_096], &x[..4_096]],
            &[&x[4_096..], &x[4_096..]],
        );
        run.assert_ramps_in("delay time");
        let want = fractional_samples(Seconds(0.2), tutti_core::SampleRate(48_000.0));
        assert_eq!(run.ramped.last_delay, vec![want, want]);
    }

    /// A mix change fades across the next block.
    ///
    /// Mutation: `Ramp::new(target.mix, target.mix, size)` fails.
    #[test]
    fn a_mix_change_fades_across_the_next_block() {
        use crate::test_support::{change_between_blocks, noise};
        let x = noise(4, 128);
        let run = change_between_blocks(
            || {
                let mut n = DelayLineNode::with_channels(6usize, 0.1, 0.0005, 0.3);
                n.set_sample_rate(tutti_core::SampleRate(48_000.0));
                n
            },
            |n| n.set_mix(0.0),
            &(0..6).map(|_| &x[..64]).collect::<Vec<_>>(),
            &(0..6).map(|_| &x[64..]).collect::<Vec<_>>(),
        );
        run.assert_ramps_in("delay mix");
        assert_eq!(run.r[0][63], x[127], "the block ends fully dry");
    }

    /// A ring routing spins an echo around a six-channel field: channel 0's
    /// impulse recirculates into channel 1's line, and nowhere else.
    ///
    /// Mutation: transposing the routing lookup (`routing[j * n + c]`) sends the
    /// echo to channel 5 instead and fails.
    #[test]
    fn a_ring_matrix_routes_each_channel_into_the_next() {
        let n = 6usize;
        let mut ring = vec![0.0f32; n * n];
        for c in 0..n {
            ring[c * n + (c + n - 1) % n] = 1.0; // c is fed by c - 1
        }
        let mut node =
            DelayLineNode::with_channels(n, 1.0, 0.01, 0.0).with_cross_feedback_matrix(&ring);
        node.set_sample_rate(tutti_core::SampleRate(1_000.0));
        node.set_cross_feedback(0.8);

        // Impulse on channel 0; fully wet, so the output is the lines alone.
        let mut inp = [0.0f32; 6];
        let mut out = [0.0f32; 6];
        inp[0] = 1.0;
        node.tick(&inp, &mut out);
        inp[0] = 0.0;
        let mut energy = [0.0f32; 6];
        for _ in 0..25 {
            node.tick(&inp, &mut out);
            for c in 0..n {
                energy[c] += out[c] * out[c];
            }
        }
        assert!(
            energy[0] > 0.5,
            "channel 0 echoes its own impulse: {energy:?}"
        );
        assert!(energy[1] > 0.1, "and feeds it into channel 1: {energy:?}");
        for (c, e) in energy.iter().enumerate().skip(2) {
            assert!(
                *e < 1e-12,
                "channel {c} is not on the ring's first lap: {energy:?}"
            );
        }
    }

    /// Whatever the matrix says, a loop cannot run away: rows are scaled to an
    /// absolute sum of at most 1, and self/cross feedback are bounded as a pair.
    ///
    /// Mutation: skipping the row scale in `with_cross_feedback_matrix` makes
    /// the all-ones matrix grow without bound and fails.
    #[test]
    fn a_dense_matrix_at_full_feedback_stays_bounded() {
        let n = 4usize;
        let mut node = DelayLineNode::with_channels(n, 0.1, 0.002, 0.99)
            .with_cross_feedback_matrix(&vec![1.0; n * n]);
        node.set_sample_rate(tutti_core::SampleRate(8_000.0));
        node.set_cross_feedback(0.99);
        let mut out = [0.0f32; 4];
        node.tick(&[1.0; 4], &mut out);
        let mut peak = 0.0f32;
        for _ in 0..20_000 {
            node.tick(&[0.0; 4], &mut out);
            peak = out.iter().fold(peak, |p, s| p.max(s.abs()));
        }
        assert!(
            peak.is_finite() && peak < 4.0,
            "the loop ran away: peak {peak}"
        );
    }
}
