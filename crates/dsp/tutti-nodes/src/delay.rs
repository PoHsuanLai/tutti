//! Delay lines and the delay nodes built on them.
//!
//! [`DelayLine`] is the bare fractional-read ring; [`DelayLineNode`] wraps it
//! into a width-generic graph node with feedback, an explicit cross-feedback
//! routing matrix, wet/dry [`Mix`], and feedback and delay time the graph can
//! modulate per frame. The fractional read is the
//! reason this is not just a [`CircularBuffer`](crate::buffer::CircularBuffer):
//! a delay whose time is modulated must interpolate between taps or it steps
//! audibly.

use tutti_core::Arc;
use tutti_core::AtomicF32;

use tutti_core::{ChannelLayout, Feedback, Mix, Param, SampleRate, Seconds, Tail};
use tutti_graph::{Cx, IntoNode, Io, Node, NodeParts, ParamNode, ParamSet, Prepare, Shape, Status};
use tutti_types::UnitParam;

use crate::ramp::{finite_or, LastGood, Ramp};

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
    /// [`DelayLineNode`]'s `prepare` does.
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

/// A feedback delay of any width: `N` audio inputs, `N` outputs, one delay
/// line and one delay time per channel.
///
/// Cross-feedback is an explicit **routing matrix**: entry `(c, j)` is how
/// much of channel `j`'s delayed tap recirculates into channel `c`'s line,
/// scaled by the [`cross_feedback`](Self::cross_feedback) amount. Width 2
/// defaults to the swap matrix `[[0, 1], [1, 0]]` (a stereo ping-pong cross
/// feed) and every other width to no routing;
/// [`with_cross_feedback_matrix`](Self::with_cross_feedback_matrix) routes any
/// width.
///
/// [`Seconds`] delay times, [`Feedback`] recirculation, cross-feedback and
/// wet/dry [`Mix`] are live [`Param`]s shared across clones, all read **once
/// per block**. A value that moved since the last block is ramped linearly
/// across the block — a delay time glides (a brief pitch bend) rather than
/// jumping (a click), and a mix change fades rather than stepping. A block of
/// one takes a change whole.
///
/// The maximum delay is baked at construction and the lines are never
/// reallocated on the audio thread; a longer request is clamped rather than
/// rejected.
///
/// # Modulated params
///
/// `N` audio inputs, `N` outputs. **Feedback** and **delay time** are
/// modulatable by the graph, in that port order
/// ([`DELAY_PARAMS`]): a per-frame value on the param port
/// ([`Io::param`](tutti_graph::Io::param)) overrides the control, and a
/// modulated delay time drives *every* channel through one shared value (the
/// natural flanger/chorus control — the lines interpolate). An unmodulated
/// param reads its own control, which costs nothing, the common case.
/// Cross-feedback and mix are not modulatable.
///
/// # In a graph
///
/// A graph node ([`IntoNode`]): inserted, its controls are a [`ParamSet`]
/// over feedback ([`UnitParam::Feedback`]), channel 0's delay time
/// ([`UnitParam::DelayTime`], the modulation's base too) and the mix
/// ([`UnitParam::Wet`]); a fork of it starts from the values last set
/// through that set, and every other cell at its value when forked. The graph
/// prepares it at the device rate, which sizes the lines, before its first
/// block. Its tail is [`Tail::Unknown`]: a recirculating line rings for as
/// long as its feedback says, so the executor never skips it.
pub struct DelayLineNode {
    /// Per-channel delay lines; `len()` is the audio width. Built at
    /// construction and on `prepare` — never resized in `process` (RT no-alloc).
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
    /// The last finite feedback / cross-feedback / mix the cells held, and
    /// each channel's last finite delay time: a non-finite write reads as
    /// unchanged, so it never reaches a line (see [`LastGood`]). A NaN pushed
    /// into a recirculating line stays there for good.
    good: [LastGood; 3],
    good_delay: Vec<LastGood>,
}

/// The params a [`DelayLineNode`] lets the graph modulate, in port order.
pub const DELAY_PARAMS: [UnitParam; 2] = [UnitParam::Feedback, UnitParam::DelayTime];

impl DelayLineNode {
    /// A mono delay (1 in, 1 out): a line sized for `max_delay_secs`, an
    /// initial `delay_secs` tap and `feedback` recirculation.
    ///
    /// `feedback` is clamped to the stable range — at or past unity a delay
    /// self-oscillates and grows without bound. `mix` starts fully wet
    /// ([`Mix::WET`]); set it for a parallel send.
    ///
    /// The lines are sized at the rate [`Node::prepare`] hands it, which is
    /// what makes `max_delay_secs` hold at whatever rate the graph runs at.
    /// Allocates, and `prepare` reallocates.
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
    /// it one. Sized at `prepare` as [`new`](Self::new); allocates.
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
            last: None,
            last_delay: vec![0.0; n],
            delay_ramps: vec![Ramp::new(0.0, 0.0, 1); n],
            taps: vec![0.0; n],
            good: [
                LastGood::new(feedback.get()),
                LastGood::new(0.0),
                LastGood::new(Mix::WET.get()),
            ],
            good_delay: vec![LastGood::new(delay_secs.get()); n],
        }
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
    /// If `matrix.len()` is not `N²`, or any entry is not finite — build-time
    /// errors, never audio-thread ones. A NaN or infinite weight would survive
    /// the row scaling (`inf / inf` is NaN) and poison the loop it feeds
    /// forever, so it is refused here rather than rendered.
    pub fn with_cross_feedback_matrix(mut self, matrix: &[f32]) -> Self {
        let n = self.width();
        assert_eq!(
            matrix.len(),
            n * n,
            "a {n}-channel delay takes a {n}x{n} cross-feedback matrix"
        );
        assert!(
            matrix.iter().all(|w| w.is_finite()),
            "cross-feedback weights must be finite: {matrix:?}"
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
    /// Read once per block and ramped. A delay time the graph feeds
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
    /// The render kernel behind `process`.
    ///
    /// Every control atomic is read once, here. `fb_port` / `dt_port` are the
    /// graph's per-frame feedback and delay time for the block when it
    /// modulates them (the node's param ports); they are read per sample.
    /// Everything else ramps from where the previous block ended (see
    /// [`ramp`](crate::ramp)).
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
            fb: self.good[0].read(self.feedback.load().get()),
            cf: if self.cross_routed {
                self.good[1].read(self.cross_feedback.load().get())
            } else {
                0.0
            },
            mix: self.good[2].read(self.mix.load().get()),
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
            let secs = self.good_delay[c].read(self.delay_time[c].load().get());
            let d = fractional_samples(Seconds(secs), sr);
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
                let fb = fb_port.map_or_else(
                    || fb_r.at(i),
                    // `clamp` passes NaN: a non-finite sample falls back to the
                    // block's feedback instead of entering the loop.
                    |s| Feedback::new_clamped(finite_or(s[i], fb_r.at(i))).get(),
                );
                let (fb, cf) = Feedback::stable_pair(fb, cf_r.at(i));
                (fb.get(), cf.get())
            };
            let ramps = &self.delay_ramps;
            // A delay-time port is audio and read per sample; the atomics ramp.
            let delay_at = |c: usize, i: usize| -> f32 {
                match dt_port {
                    Some(s) if s[i].is_finite() => {
                        fractional_samples(Seconds(s[i].clamp(0.0, max)), sr)
                    }
                    _ => ramps[c].at(i),
                }
            };
            lines.run(size, x, y, loop_gain, delay_at, |i| mix_r.at(i));
        }

        // Where the next block's ramps start.
        for c in 0..width {
            self.last_delay[c] = match dt_port {
                Some(s) if s[size - 1].is_finite() => {
                    fractional_samples(Seconds(s[size - 1].clamp(0.0, max)), sr)
                }
                _ => self.delay_ramps[c].at(size - 1),
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

impl Node for DelayLineNode {
    /// `N` in, `N` out, feedback and delay time as param ports.
    ///
    /// Zero latency, whatever the delay time: the echo is the *effect*, not
    /// processing latency. PDC compensates whatever a node declares by
    /// delaying every other path, so reporting the delay time would push the
    /// whole rest of the mix late to "line up" with an echo that is supposed
    /// to be late. The dry half of the blend is undelayed, so the output's earliest
    /// energy leaves with the input.
    fn shape(&self) -> Shape {
        let width = ChannelLayout::from_count(self.width() as u16);
        Shape::audio(width, width)
            .with_params(&DELAY_PARAMS)
            .with_tail(Tail::Unknown)
    }

    fn prepare(&mut self, p: &Prepare) {
        self.sample_rate = p.sample_rate();
        for d in &mut self.delays {
            *d = DelayLine::from_seconds(self.max_delay, self.sample_rate);
        }
        // Delay positions are in samples of the previous rate: start the next block
        // on its targets rather than gliding across a rate change.
        self.last = None;
    }

    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let size = io.frames();
        if size == 0 {
            return Status::Modified;
        }
        let (fb, dt) = (io.param(0).frames(), io.param(1).frames());
        let (inputs, mut outputs) = io.split();
        self.render(
            size,
            |c, i| inputs.get(c)[i],
            |c, i, v| outputs.get(c)[i] = v,
            fb,
            dt,
        );
        Status::Modified
    }

    fn reset(&mut self) {
        for d in &mut self.delays {
            d.reset();
        }
        self.last = None;
    }

    fn param_base(&self, k: usize) -> Option<f32> {
        // Delay time's base is the first channel's: a modulated delay time
        // drives every channel through one shared value.
        match k {
            0 => Some(self.feedback.load().get()),
            1 => Some(self.delay_time[0].load().get()),
            _ => None,
        }
    }
}

impl ParamNode for DelayLineNode {
    /// Feedback, channel 0's delay time and the mix. The other channels'
    /// times and the cross-feedback have no address: set them through the
    /// node's own handles.
    fn param_set(&self) -> ParamSet {
        ParamSet::builder()
            .param(UnitParam::Feedback, self.feedback.as_atomic())
            .param(UnitParam::DelayTime, self.delay_time[0].as_atomic())
            .param(UnitParam::Wet, self.mix.as_atomic())
            .build()
    }

    /// A clone with every control cell detached (at its value now), so a
    /// write to either never reaches the other, and its lines cleared.
    fn fork_fresh(&self) -> Self {
        let mut fork = self.clone();
        fork.delay_time.iter_mut().for_each(Param::detach);
        fork.feedback.detach();
        fork.cross_feedback.detach();
        fork.mix.detach();
        Node::reset(&mut fork);
        fork
    }
}

/// Inserted with its [`ParamSet`] as its controls and a fork that starts
/// from the values last set through it ([`tutti_graph::param_parts`]).
impl IntoNode for DelayLineNode {
    type Controls = ParamSet;

    fn into_parts(self) -> NodeParts<ParamSet> {
        tutti_graph::param_parts(self)
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
            last: self.last,
            last_delay: self.last_delay.clone(),
            delay_ramps: self.delay_ramps.clone(),
            taps: self.taps.clone(),
            good: self.good,
            good_delay: self.good_delay.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_graph::contract::{assert_param_fork, drive, prepared};

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

    /// `node`, prepared at `sr` for blocks of up to 1024 frames.
    fn at(node: DelayLineNode, sr: f64) -> DelayLineNode {
        prepared(node, SampleRate(sr), 1024)
    }

    /// `node` over `inputs` one frame per `process` call (a change lands
    /// whole on its frame), `params[k][i]` on
    /// param port `k` at frame `i` when `Some`.
    fn frames(
        node: &mut DelayLineNode,
        inputs: &[&[f32]],
        params: &[Option<&[f32]>],
    ) -> Vec<Vec<f32>> {
        let rate = node.sample_rate;
        let n = inputs[0].len();
        let mut out = vec![Vec::with_capacity(n); inputs.len()];
        for i in 0..n {
            let ins: Vec<&[f32]> = inputs.iter().map(|c| &c[i..=i]).collect();
            let ps: Vec<Option<&[f32]>> = params.iter().map(|p| p.map(|v| &v[i..=i])).collect();
            for (o, v) in out.iter_mut().zip(drive(node, rate, &ins, &ps)) {
                o.push(v[0]);
            }
        }
        out
    }

    /// An impulse of `len` frames on each of `width` channels where `hot`.
    fn impulses(width: usize, len: usize, hot: &[usize]) -> Vec<Vec<f32>> {
        (0..width)
            .map(|c| {
                let mut v = vec![0.0; len];
                if hot.contains(&c) {
                    v[0] = 1.0;
                }
                v
            })
            .collect()
    }

    fn refs(v: &[Vec<f32>]) -> Vec<&[f32]> {
        v.iter().map(|c| &c[..]).collect()
    }

    /// A fork starts from the values last set through the node's
    /// `ParamSet` and shares no cell with it (see
    /// `tutti_graph::contract::assert_param_fork`).
    ///
    /// Mutation (run): drop the `mix` detach in `fork_fresh` → "a live write
    /// reached the fork" for `Wet`. Leave `Wet` out of `param_set` → the
    /// address list below fails.
    #[test]
    fn a_fork_starts_from_the_authored_values_and_shares_nothing() {
        let node = DelayLineNode::stereo(1.0, 0.05, 0.07, 0.4);
        assert_eq!(
            node.param_set().params().collect::<Vec<_>>(),
            [UnitParam::Feedback, UnitParam::DelayTime, UnitParam::Wet]
        );
        assert_param_fork(node);
    }

    /// The cells a fork detaches beyond its `ParamSet`'s: channel 1's delay
    /// time and the cross-feedback keep their values at the fork and follow
    /// no live write.
    ///
    /// Mutation (run): drop the `delay_time` detach in `fork_fresh` → the
    /// fork's right time follows the live write → fails.
    #[test]
    fn a_fork_detaches_the_unaddressed_cells_too() {
        let node = DelayLineNode::stereo(1.0, 0.05, 0.07, 0.4);
        node.set_cross_feedback(0.2);
        let fork = node.fork_fresh();
        node.set_delay_time_r(0.3);
        node.set_cross_feedback(0.6);
        assert_eq!(fork.delay_time[1].load(), Seconds(0.07));
        assert_eq!(fork.cross_feedback.load(), Feedback(0.2));
    }

    #[test]
    fn test_delay_node_passthrough_no_feedback() {
        let mut node = at(DelayLineNode::new(1.0, 0.0, 0.0), 44_100.0);
        node.set_mix(0.0);
        let out = frames(&mut node, &[&[0.5]], &[]);
        assert!(
            (out[0][0] - 0.5).abs() < 0.001,
            "Dry-only should pass through input"
        );
    }

    #[test]
    fn test_delay_node_echoes() {
        let delay_secs = 0.01; // 10 samples at 1 kHz
        let mut node = at(DelayLineNode::new(1.0, delay_secs, 0.5), 1_000.0);
        let x = impulses(1, 11, &[0]);
        let out = frames(&mut node, &refs(&x), &[]);
        // At sample 10, we should see the delayed signal
        assert!(
            out[0][10].abs() > 0.3,
            "Should hear echo at delay time, got {}",
            out[0][10]
        );
    }

    #[test]
    fn test_stereo_delay_independent_channels() {
        let mut node = at(DelayLineNode::stereo(1.0, 0.005, 0.01, 0.0), 1_000.0);
        let x = impulses(2, 11, &[0, 1]);
        let out = frames(&mut node, &refs(&x), &[]);
        // After 5 samples the left echoes; after 10, the right.
        let (left_5, right_10) = (out[0][5], out[1][10]);
        assert!(left_5.abs() > 0.5, "Left echo at 5 samples: {left_5}");
        assert!(right_10.abs() > 0.5, "Right echo at 10 samples: {right_10}");
    }

    #[test]
    fn test_stereo_delay_cross_feedback() {
        let mut node = at(DelayLineNode::stereo(1.0, 0.01, 0.01, 0.0), 1_000.0);
        node.set_cross_feedback(0.5);
        // Impulse on the left only.
        let x = impulses(2, 21, &[0]);
        let out = frames(&mut node, &refs(&x), &[]);
        // After a delay period or two the right has picked up the left's
        // cross-feedback.
        assert!(
            out[1][10].abs() > 0.01 || out[1][20].abs() > 0.01,
            "Cross-feedback should produce signal in right channel"
        );
    }

    // ── Width-native (N-channel) ─────────────────────────────────────────────

    #[test]
    fn delay_with_channels_2_matches_new() {
        // with_channels(2, ...) with equal times == stereo(...) with equal L/R.
        let mut a = at(DelayLineNode::stereo(1.0, 0.01, 0.01, 0.4), 48_000.0);
        let mut b = at(
            DelayLineNode::with_channels(ChannelLayout::STEREO, 1.0, 0.01, 0.4),
            48_000.0,
        );
        let x = impulses(2, 2000, &[0, 1]);
        let (oa, ob) = (
            frames(&mut a, &refs(&x), &[]),
            frames(&mut b, &refs(&x), &[]),
        );
        for i in 0..2000 {
            assert_eq!(oa[0][i].to_bits(), ob[0][i].to_bits(), "L bit-diff at {i}");
            assert_eq!(oa[1][i].to_bits(), ob[1][i].to_bits(), "R bit-diff at {i}");
        }
    }

    #[test]
    fn delay_with_channels_reports_arity() {
        let d = DelayLineNode::with_channels(6usize, 1.0, 0.01, 0.3);
        let shape = d.shape();
        assert_eq!(shape.audio_in.count(), 6);
        assert_eq!(shape.audio_out.count(), 6);
    }

    #[test]
    fn wide_delay_channels_are_independent_no_crossfeed() {
        // 6-channel delay: an impulse on channel 3 echoes only on channel 3,
        // and cross_feedback (a stereo-only notion) is inert.
        let mut node = at(
            DelayLineNode::with_channels(6usize, 1.0, 0.01, 0.0),
            1_000.0,
        );
        node.set_cross_feedback(0.9); // must have NO effect above width 2

        let x = impulses(6, 13, &[3]);
        let out = frames(&mut node, &refs(&x), &[]);
        // Frames 1..=12 cover the 10-sample delay (0.01 s at 1 kHz).
        let ch3_echo = out[3][1..].iter().fold(0.0f32, |m, s| m.max(s.abs()));
        let other_energy: f32 = [0usize, 1, 2, 4, 5]
            .iter()
            .map(|&c| out[c][1..].iter().map(|s| s * s).sum::<f32>())
            .sum();
        assert!(ch3_echo > 0.5, "ch3 should echo; peak {ch3_echo}");
        assert!(
            other_energy < 1e-10,
            "no cross-feed above width 2; other-channel energy {other_energy}"
        );
    }

    // ── Modulated params (the graph's param ports) ───────────────────────────

    /// A stereo delay frame by frame with `params` (feedback, delay time:
    /// `Some` held at a value, `None` unmodulated) on its param ports.
    fn stereo_delay_fed(
        node: &mut DelayLineNode,
        l: &[f32],
        r: &[f32],
        params: [Option<f32>; 2],
    ) -> (Vec<f32>, Vec<f32>) {
        let held: Vec<Option<Vec<f32>>> =
            params.iter().map(|p| p.map(|v| vec![v; l.len()])).collect();
        let ps: Vec<Option<&[f32]>> = held.iter().map(|p| p.as_deref()).collect();
        let mut out = frames(node, &[l, r], &ps);
        let r = out.pop().expect("two outputs");
        (out.pop().expect("two outputs"), r)
    }

    /// The node declares feedback then delay time as its params, and never
    /// changes the arity: a modulatable delay is as wide as it was built, in
    /// and out.
    ///
    /// Mutation (run): swap `DELAY_PARAMS`' order → the first assertion
    /// fails (and `feedback_feed_modulates` reads a delay time as feedback).
    #[test]
    fn the_feed_declares_feedback_then_delay_time() {
        let d = DelayLineNode::stereo(1.0, 0.01, 0.01, 0.5);
        let shape = d.shape();
        assert_eq!(
            shape.params.as_slice(),
            &[UnitParam::Feedback, UnitParam::DelayTime][..]
        );
        assert_eq!((shape.audio_in.count(), shape.audio_out.count()), (2, 2));
        let wide = DelayLineNode::with_channels(6usize, 1.0, 0.01, 0.5).shape();
        assert_eq!((wide.audio_in.count(), wide.audio_out.count()), (6, 6));
        assert_eq!(d.param_base(0), Some(0.5), "feedback's base is its control");
        assert_eq!(
            d.param_base(1),
            Some(0.01),
            "delay time's base is channel 0's"
        );
    }

    #[test]
    fn feedback_feed_modulates() {
        // An impulse through the delay: a higher fed feedback produces a
        // longer / louder tail than a low one. Proves the param port drives
        // the feedback per sample.
        let delay_secs = 0.01; // 10 samples
        let n = 200;

        let run = |fb: f32| -> f32 {
            let mut node = at(
                DelayLineNode::stereo(1.0, delay_secs, delay_secs, 0.0),
                1_000.0,
            );
            let x = impulses(2, n, &[0, 1]);
            let (out_l, _) = stereo_delay_fed(&mut node, &x[0], &x[1], [Some(fb), None]);
            // Late-buffer energy (well past the first echo) — feedback governs
            // how much survives.
            out_l[50..].iter().map(|s| s * s).sum()
        };

        let low = run(0.1);
        let high = run(0.9);
        assert!(
            high > low * 2.0,
            "higher fed feedback should yield a longer/louder tail: low={low}, high={high}"
        );
    }

    #[test]
    fn unmodulated_matches_held_constant() {
        // A delay whose param ports hold its params at the atomic values must
        // produce the same output as one reading its controls — the fed path
        // is a faithful superset.
        let delay_secs = 0.01;
        let fb = 0.5;
        let n = 256;

        let mut input_l = vec![0.0f32; n];
        let mut input_r = vec![0.0f32; n];
        for i in 0..n {
            input_l[i] = ((i * 7 + 3) % 100) as f32 / 50.0 - 1.0;
            input_r[i] = ((i * 5 + 1) % 100) as f32 / 50.0 - 1.0;
        }

        let mut plain = at(
            DelayLineNode::stereo(1.0, delay_secs, delay_secs, fb),
            1_000.0,
        );
        let (plain_l, plain_r) = stereo_delay_fed(&mut plain, &input_l, &input_r, [None, None]);

        // Both params fed, held at the atomic values.
        let mut modn = at(
            DelayLineNode::stereo(1.0, delay_secs, delay_secs, fb),
            1_000.0,
        );
        let (mod_l, mod_r) =
            stereo_delay_fed(&mut modn, &input_l, &input_r, [Some(fb), Some(delay_secs)]);

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
        use crate::test_support::change_between_node_blocks;
        let x = slow_sine(4_096 + 64);
        let run = change_between_node_blocks(
            || DelayLineNode::stereo(1.0, 0.010, 0.012, 0.0),
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
        use crate::test_support::{change_between_node_blocks, noise};
        let x = noise(4, 128);
        let run = change_between_node_blocks(
            || DelayLineNode::with_channels(6usize, 0.1, 0.0005, 0.3),
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
        let mut node = at(
            DelayLineNode::with_channels(n, 1.0, 0.01, 0.0).with_cross_feedback_matrix(&ring),
            1_000.0,
        );
        node.set_cross_feedback(0.8);

        // Impulse on channel 0; fully wet, so the output is the lines alone.
        let x = impulses(n, 26, &[0]);
        let out = frames(&mut node, &refs(&x), &[]);
        let mut energy = [0.0f32; 6];
        for c in 0..n {
            energy[c] = out[c][1..].iter().map(|s| s * s).sum();
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
        let mut node = at(
            DelayLineNode::with_channels(n, 0.1, 0.002, 0.99)
                .with_cross_feedback_matrix(&vec![1.0; n * n]),
            8_000.0,
        );
        node.set_cross_feedback(0.99);
        let x = impulses(n, 20_001, &[0, 1, 2, 3]);
        let out = frames(&mut node, &refs(&x), &[]);
        let peak = out
            .iter()
            .flat_map(|c| &c[1..])
            .fold(0.0f32, |p, s| p.max(s.abs()));
        assert!(
            peak.is_finite() && peak < 4.0,
            "the loop ran away: peak {peak}"
        );
    }

    /// A non-finite routing weight is refused at build time.
    ///
    /// Mutation: dropping the finiteness assert lets this build (the node
    /// would then render NaN forever) and fails the `should_panic`.
    #[test]
    #[should_panic(expected = "must be finite")]
    fn a_non_finite_cross_feedback_weight_is_refused() {
        let _ = DelayLineNode::with_channels(ChannelLayout::STEREO, 0.1, 0.01, 0.3)
            .with_cross_feedback_matrix(&[0.0, f32::NAN, 1.0, 0.0]);
    }
}
