//! [`DelayFamily`]: the feedback delay as a [`Family`] (design 024 section 3, row 11).
//!
//! A delay of any width: one [`DelayLine`] per channel, each recirculating its own tap
//! (the feedback) and, when the family has a [`Routing`], the taps of the others (the
//! cross-feedback, a stereo ping-pong's). The output is the delayed signal alone, the
//! **wet** signal: the blend with the dry input is the graph's mix.
//!
//! One frame of an uncoupled channel is [`delay_step`]: the feedback
//! tap is read at `max(d, 1) - 1` before the push, the line takes `x + fb * tap`, and the
//! output is read at `d` after it, so the loop's period is the output delay `d` and the
//! impulse response is `g^k` at `(k + 1) d` frames (an integer `d`). A coupled frame
//! reads every channel's tap before any line is written.
//!
//! **Time** is channel 0's control (and the one a lane drives: lane 0 of the [`Feed`],
//! in seconds, drives every channel); the channels past the first follow it
//! ([`ChannelTimes::linked`]) or hold a time of their own ([`ChannelTimes::each`]).
//! **Feedback** is lane 1. Both are read **per frame** when the graph drives them
//! (design 024 section 4.5, "Where cadence stops"): a delay time moved at audio rate is a
//! flanger, and a lane frame that is not finite falls back to the member's. A value that
//! is a cell lands whole on the stream grid ([`Family::CADENCE`]) and the recurrence
//! glides it: the **time** at most [`DELAY_SLEW`] frames of delay per frame (a moving
//! delay is a pitch bend, so its speed is bounded and not its duration), the feedback and
//! the cross-feedback over [`GAIN_GLIDE`]. A lane frame is the
//! trajectory itself, and is not glided.
//!
//! The delay has **no latency**: the echo is the effect, not processing delay, and PDC
//! must not delay the rest of the mix to line up with an echo that is supposed to be
//! late. Its tail is the echoes still to come, `time * (1 + trips)` ([`System::tail`]).
//!
//! The memory is the family's own [`Domain`](crate::Domain), the [`MaxDelay`]: a line holds that long,
//! and the member's time is clamped into it ([`Grows`]).

use core::ops::Range;

use tutti_types::{BusTopology, Db, Feedback, Hz, SampleRate, Samples, Seconds};

use super::max_delay::MaxDelay;
use super::routing::{MAX_WIDTH, Routing, RoutingError};
use super::times::{ChannelTimes, ChannelTimesError};
use crate::control::COEFF_STREAM;
use crate::{
    Approach, Cadence, Capacity, Channels, Complex64, DelayLine, Effort, Family, Feed, GAIN_GLIDE,
    Glide, Grows, InterpolationMode, Lti, Reach, Recurrence, Ring, Setup, Start, Steps, System,
    cubic_hermite, delay_step, fractional_samples,
};

/// The fastest a delay time glides: 1/8 of a frame of delay per frame.
///
/// A moving delay is a pitch shift: a delay that grows by `s` frames per frame lowers
/// the pitch by the factor `1 / (1 + s)`, so the glide's speed bounds the bend, and a
/// time that jumps is a pitch bend as long as the jump is far. At 1/8 the bend is at
/// most 12.5%, about 2 semitones (`12 log2(1.125) = 2.04`), however far the time moves:
/// a jump of 0.5 s at 48 kHz glides for 192,000 frames (4 s). (`Steps::Rate` takes an
/// `f32`: where the unit types stop.)
pub const DELAY_SLEW: f32 = 0.125;

/// The most channels a delay has: a bus's.
pub const MAX_DELAY_CHANNELS: usize = MAX_WIDTH;

/// The controls of a [`DelayFamily`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DelayControls {
    /// The delay of channel 0, and of every channel that follows it
    /// ([`ChannelTimes::linked`]): clamped into `0..=max_delay`. Not finite reads as the
    /// default's.
    pub time: Seconds,
    /// How much of a channel's own delayed tap recirculates into its line. Bounded, with
    /// the cross-feedback, as a pair
    /// ([`Feedback::stable_pair`]), so the loop gain stays under
    /// [`Feedback::MAX_STABLE`]. Not finite reads as the default's.
    pub feedback: Feedback,
    /// How much of the routed taps ([`Routing`]) recirculates: a stereo ping-pong's
    /// cross-feed. Inert on a delay with no routing. Not finite reads as the default's.
    pub cross_feedback: Feedback,
}

impl DelayControls {
    /// No delay, no recirculation: what a non-finite control falls back to.
    pub const DEFAULT: Self = Self {
        time: Seconds(0.0),
        feedback: Feedback::NONE,
        cross_feedback: Feedback::NONE,
    };
}

impl Default for DelayControls {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// `x` if it is finite, else `default`.
fn finite_or(x: f32, default: f32) -> f32 {
    if x.is_finite() { x } else { default }
}

/// The l1 norm of an interpolator's taps, which bounds its gain at every frequency:
/// 1 for the whole sample and the linear blend (a convex combination), and 1.25 for the
/// cubic Hermite, whose taps are `(-1/16, 9/16, 9/16, -1/16)` at the half.
fn interpolation_gain(mode: InterpolationMode) -> f64 {
    match mode {
        InterpolationMode::CubicHermite => 1.25,
        _ => 1.0,
    }
}

/// What one fractional read of a line is, as taps: `(frames back from the newest write,
/// weight)`, the sum read by [`DelayLine::read_sample`] at `delay` (clamped to `cap`).
/// The index arithmetic is the line's own, so a model built from these has the line's
/// behaviour at the edges too (a cubic read at delay below 1 reads one sample twice).
pub(crate) fn read_taps(
    delay: f32,
    cap: usize,
    mode: InterpolationMode,
) -> ([(usize, f32); 4], usize) {
    let delay = delay.clamp(0.0, cap as f32);
    let mut taps = [(0, 0.0); 4];
    match mode {
        InterpolationMode::None => {
            taps[0] = ((delay.round() as usize).min(cap), 1.0);
            (taps, 1)
        }
        InterpolationMode::Linear => {
            let floor = delay.floor() as usize;
            let frac = delay - floor as f32;
            taps[0] = (floor.min(cap), 1.0 - frac);
            taps[1] = ((floor + 1).min(cap), frac);
            (taps, 2)
        }
        InterpolationMode::CubicHermite => {
            let floor = delay.floor() as usize;
            let frac = delay - floor as f32;
            let at = [floor.saturating_sub(1), floor, floor + 1, floor + 2];
            for (k, slot) in taps.iter_mut().enumerate() {
                let mut e = [0.0f32; 4];
                e[k] = 1.0;
                // Linear in the four taps: the weight of tap `k` is the read of its basis.
                *slot = (at[k].min(cap), cubic_hermite(e[0], e[1], e[2], e[3], frac));
            }
            (taps, 4)
        }
    }
}

/// A delay at one set of controls and one rate: each channel's delay in frames, the
/// loop's gains and the routing.
///
/// The description the recurrence is held to. It is **linear and time invariant** at a
/// held delay ([`Lti`]), and MIMO: `H = O (I - A T)^-1`, with `O` and `T` the diagonal
/// output and tap interpolators of the channels (the line's own taps, so exact at every
/// delay, the edges included), and `A = g I + c R` the loop (`g` the feedback, `c` the
/// cross-feedback and `R` the [`Routing`]). A delay with no cross-feedback is the
/// diagonal case, `O / (1 - g T)` per channel, and for an integer delay of `d >= 1`
/// frames that is `z^-d / (1 - g z^-d)`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DelayMember {
    rate: SampleRate,
    interpolation: InterpolationMode,
    width: usize,
    /// Each channel's delay, in frames (fractional).
    delays: [f32; MAX_DELAY_CHANNELS],
    /// The line's capacity in frames: the delay is clamped into it.
    cap: usize,
    /// The feedback and cross-feedback as asked (finite), before the pair is bounded.
    feedback: f32,
    cross: f32,
    routing: Routing,
}

impl DelayMember {
    fn of(family: &DelayFamily, c: &DelayControls, rate: SampleRate) -> Self {
        let d = DelayControls::DEFAULT;
        let cap = family.max_delay.frames(rate);
        let max_secs = family.max_delay.get().to_f32();
        let frames = |secs: f32| -> f32 {
            let secs = Seconds::from_f32(secs.clamp(0.0, max_secs));
            fractional_samples(secs, rate).min(cap as f32)
        };
        let time = if c.time.get().is_finite() {
            c.time
        } else {
            d.time
        };
        let width = family.width();
        let mut delays = [0.0; MAX_DELAY_CHANNELS];
        let first = frames(time.to_f32());
        delays[0] = first;
        for (ch, slot) in delays.iter_mut().enumerate().take(width).skip(1) {
            *slot = match family.times.fixed().get(ch - 1) {
                Some(t) if !family.times.is_linked() => frames(t.to_f32()),
                _ => first,
            };
        }
        let routed = !family.routing.is_none();
        Self {
            rate,
            interpolation: family.interpolation,
            width,
            delays,
            cap,
            feedback: finite_or(c.feedback.get(), d.feedback.get()),
            cross: if routed {
                finite_or(c.cross_feedback.get(), d.cross_feedback.get())
            } else {
                0.0
            },
            routing: family.routing,
        }
    }

    /// The channels.
    #[must_use]
    pub fn width(&self) -> usize {
        self.width
    }

    /// The delay of `channel`, in frames (fractional), clamped into the line.
    #[must_use]
    pub fn delay_frames(&self, channel: usize) -> f32 {
        self.delays[channel.min(self.width - 1)]
    }

    /// The loop gains `(feedback, cross-feedback)`, bounded as a pair
    /// ([`Feedback::stable_pair`]).
    #[must_use]
    pub fn gains(&self) -> (Feedback, Feedback) {
        Feedback::stable_pair(self.feedback, self.cross)
    }

    /// The routing the cross-feedback goes through.
    #[must_use]
    pub fn routing(&self) -> &Routing {
        &self.routing
    }

    /// The interpolation of the reads.
    #[must_use]
    pub fn interpolation(&self) -> InterpolationMode {
        self.interpolation
    }

    /// Whether the channels recirculate into each other: a routing and a cross-feedback
    /// that is not zero.
    #[must_use]
    pub fn is_coupled(&self) -> bool {
        self.gains().1.get() != 0.0 && !self.routing.is_none()
    }

    /// The loop's gain bound `q`: the largest sum, over a line, of the feedback and the
    /// cross-feedback of the taps routed into it.
    fn loop_gain(&self) -> f64 {
        let (g, c) = self.gains();
        let rows = (0..self.width)
            .map(|r| f64::from(self.routing.row_sum(r)))
            .fold(0.0, f64::max);
        f64::from(g.get()) + f64::from(c.get()) * rows
    }

    /// The output taps of `channel` (read after the push: `i` frames back is `z^-i`) and
    /// its feedback taps (read before it: `i` frames back is `z^-(i+1)`).
    fn taps(&self, channel: usize) -> (([(usize, f32); 4], usize), ([(usize, f32); 4], usize)) {
        let d = self.delays[channel];
        (
            read_taps(d, self.cap, self.interpolation),
            read_taps((d.max(1.0) - 1.0).max(0.0), self.cap, self.interpolation),
        )
    }

    /// The most frames back the loop reaches: a feedback tap's farthest read, one
    /// frame back more.
    fn loop_frames(&self) -> usize {
        (0..self.width)
            .map(|c| {
                let ((_, _), (taps, n)) = self.taps(c);
                taps[..n].iter().map(|t| t.0).max().unwrap_or(0) + 1
            })
            .max()
            .unwrap_or(1)
    }

    /// The trips an echo makes before it is `floor` under the first one, at gain `q`.
    fn trips(q: f64, floor: Db) -> f64 {
        if q <= 0.0 {
            return 0.0;
        }
        let target = libm::log(10f64.powf(f64::from(floor.get()) / 20.0));
        (target / libm::log(q)).ceil().max(0.0)
    }
}

/// How long the echoes of a delay of `frames` frames ring: `1 + trips` periods of the
/// delay and its interpolator's spread (the echo `k` is spread over `k + 1` frames at
/// most; two per trip errs long), in seconds.
fn ring_seconds(frames: usize, trips: f64, rate: SampleRate) -> Seconds {
    Seconds((1.0 + trips) * (frames as f64 + 2.0) / rate.get())
}

impl System for DelayMember {
    /// Zero, whatever the delay time: musical delay is not latency.
    fn latency(&self) -> Samples {
        Samples::ZERO
    }

    /// The echoes still to come, to `floor` under the first: the loop's period (the
    /// longest delay) a trip, and as many trips as it takes the loop gain to fall to it.
    fn tail(&self, floor: Db) -> Ring {
        let longest = self.delays[..self.width]
            .iter()
            .map(|d| d.ceil() as usize)
            .max()
            .unwrap_or(0);
        Ring::Finite(ring_seconds(
            longest,
            Self::trips(self.loop_gain(), floor),
            self.rate,
        ))
    }
}

impl Lti for DelayMember {
    fn inputs(&self) -> usize {
        self.width
    }

    fn outputs(&self) -> usize {
        self.width
    }

    /// The response of `output` to **the same signal on every input** (what the contract
    /// feeds): `O_o v_o`, with `v` solving `(I - A T) v = 1`. A delay with no coupling is
    /// diagonal, and that is `O_o / (1 - g T_o)`, the response of channel `output` to its
    /// own input.
    // Index arithmetic over small square matrices reads as the maths it is.
    #[allow(clippy::needless_range_loop)]
    fn frequency_response(&self, output: usize, freqs: &[Hz], gain: &mut [Complex64]) {
        if output >= self.width {
            gain.fill(Complex64::new(f64::NAN, f64::NAN));
            return;
        }
        let n = self.width;
        let (g, cf) = self.gains();
        let (g, cf) = (f64::from(g.get()), f64::from(cf.get()));
        let coupled = self.is_coupled();
        for (out, f) in gain.iter_mut().zip(freqs) {
            let w = std::f64::consts::TAU * f64::from(f.get()) / self.rate.get();
            // `z^-k` for a tap `k` frames back.
            let z = |k: usize| Complex64::from_polar(1.0, -w * k as f64);
            let mut o = [Complex64::new(0.0, 0.0); MAX_DELAY_CHANNELS];
            let mut t = o;
            for c in 0..n {
                let ((ot, on), (tt, tn)) = self.taps(c);
                o[c] = ot[..on].iter().map(|&(k, a)| z(k) * f64::from(a)).sum();
                t[c] = tt[..tn].iter().map(|&(k, a)| z(k + 1) * f64::from(a)).sum();
            }
            let one = Complex64::new(1.0, 0.0);
            let v = if coupled {
                // (I - A T) v = 1, with A = g I + c R.
                let mut m = [[Complex64::new(0.0, 0.0); MAX_DELAY_CHANNELS]; MAX_DELAY_CHANNELS];
                for r in 0..n {
                    for c in 0..n {
                        let a = cf * f64::from(self.routing.weight(r, c))
                            + if r == c { g } else { 0.0 };
                        m[r][c] = if r == c {
                            one
                        } else {
                            Complex64::new(0.0, 0.0)
                        } - t[c] * a;
                    }
                }
                solve(&mut m, n)[output]
            } else {
                one / (one - t[output] * g)
            };
            *out = o[output] * v;
        }
    }

    /// `(q L)^(1/K)`: every pole is inside it. A pole `z` has `1 = |A T(z)|` for some
    /// row, and `|T(z)| <= L |z|^-K` for `|z| < 1`, with `K` the most frames back the
    /// loop reaches and `L` the interpolator's l1 gain, so `|z| <= (q L)^(1/K)`. It is
    /// the radius the loop's longest delay gives, and the loop is stable by it when
    /// `q L < 1` (the cubic's `L` of 1.25 makes it inconclusive above a feedback of 0.79).
    fn spectral_radius(&self) -> f64 {
        let ql = self.loop_gain() * interpolation_gain(self.interpolation);
        if ql <= 0.0 {
            return 0.0;
        }
        ql.powf(1.0 / self.loop_frames() as f64)
    }

    /// `L / (1 - q L)`: `|v| <= 1 / (1 - q L)` for `|x| <= 1` (the geometric series of
    /// the loop), and the output is `L` times a `v`. Infinite when `q L` is not under 1.
    fn l1_norm(&self) -> f64 {
        let l = interpolation_gain(self.interpolation);
        let ql = self.loop_gain() * l;
        if ql < 1.0 {
            l / (1.0 - ql)
        } else {
            f64::INFINITY
        }
    }
}

/// Solves `m x = 1` in place for the first `n` rows (Gaussian elimination, partial
/// pivoting): the loop of a coupled delay at one frequency. A singular system gives NaN.
// Index arithmetic over a small square matrix reads as the elimination it is.
#[allow(clippy::needless_range_loop)]
fn solve(
    m: &mut [[Complex64; MAX_DELAY_CHANNELS]; MAX_DELAY_CHANNELS],
    n: usize,
) -> [Complex64; MAX_DELAY_CHANNELS] {
    let mut b = [Complex64::new(1.0, 0.0); MAX_DELAY_CHANNELS];
    for col in 0..n {
        let pivot = (col..n)
            .max_by(|&a, &c| m[a][col].norm().total_cmp(&m[c][col].norm()))
            .unwrap_or(col);
        m.swap(col, pivot);
        b.swap(col, pivot);
        let p = m[col][col];
        for row in col + 1..n {
            let k = m[row][col] / p;
            for c in col..n {
                let sub = m[col][c] * k;
                m[row][c] -= sub;
            }
            let sub = b[col] * k;
            b[row] -= sub;
        }
    }
    let mut x = [Complex64::new(0.0, 0.0); MAX_DELAY_CHANNELS];
    for row in (0..n).rev() {
        let mut acc = b[row];
        for c in row + 1..n {
            acc -= m[row][c] * x[c];
        }
        x[row] = acc / m[row][row];
    }
    x
}

/// The sizes a delay allocates: the longest delay a line holds, and how long the echoes
/// of the longest delay ring ([`Capacity`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DelayCapacity {
    /// The longest delay a line holds, in frames. A line is one frame longer, so a read at
    /// exactly this still has a neighbour to blend toward.
    pub frames: usize,
    tail: Ring,
}

impl DelayCapacity {
    /// The sizes for a delay of `frames` at its worst (`q` the loop gain's bound).
    fn worst(frames: usize, q: f64, floor: Db, rate: SampleRate) -> Self {
        Self {
            frames,
            tail: Ring::Finite(ring_seconds(frames, DelayMember::trips(q, floor), rate)),
        }
    }
}

impl Capacity for DelayCapacity {
    type Source = DelayMember;

    fn need(member: &DelayMember, floor: Db) -> Self {
        let frames = (0..member.width)
            .map(|c| member.delays[c].ceil() as usize)
            .max()
            .unwrap_or(0);
        Self {
            frames,
            tail: member.tail(floor),
        }
    }

    fn tail(&self) -> Ring {
        self.tail
    }

    fn dominates(&self, other: &Self) -> bool {
        self.frames >= other.frames && self.tail.covers(other.tail)
    }
}

/// The [`Family`] of the feedback delay: `input` wide, each channel a line of up to
/// [`MaxDelay`], recirculating through its own tap and, with a [`Routing`], the others'.
///
/// The maximum, the interpolation, the routing and the channel times are **construction
/// constants**, not controls: the maximum sizes the lines, and the others are a
/// build-time choice, not something to automate. A stereo delay is routed by
/// [`Routing::swap`] (a ping-pong) and any other width by [`Routing::none`], until
/// [`with_routing`](Self::with_routing) says otherwise.
#[derive(Clone, Copy, Debug)]
pub struct DelayFamily {
    input: BusTopology,
    max_delay: MaxDelay,
    interpolation: InterpolationMode,
    routing: Routing,
    times: ChannelTimes,
}

impl DelayFamily {
    /// A delay on the bus `input`, holding up to `max_delay`, reading with
    /// `interpolation`, its channels linked in time and routed by the default for its
    /// width.
    #[must_use]
    pub fn new(input: BusTopology, max_delay: MaxDelay, interpolation: InterpolationMode) -> Self {
        let routing = if input == BusTopology::STEREO {
            Routing::swap()
        } else {
            Routing::none(input)
        };
        Self {
            input,
            max_delay,
            interpolation,
            routing,
            times: ChannelTimes::linked(),
        }
    }

    /// The same delay with its cross-feedback routed by `routing`.
    ///
    /// # Errors
    ///
    /// [`RoutingError::Width`] when `routing` is for another bus.
    pub fn with_routing(mut self, routing: Routing) -> Result<Self, RoutingError> {
        if routing.input() != self.input {
            return Err(RoutingError::Width {
                routing: routing.width(),
                family: self.width(),
            });
        }
        self.routing = routing;
        Ok(self)
    }

    /// The same delay with its channels past the first delayed by `times`.
    ///
    /// # Errors
    ///
    /// [`ChannelTimesError::Length`] unless `times` is [linked](ChannelTimes::linked) or
    /// has one time for each channel past the first, and
    /// [`ChannelTimesError::Past`] for a time past the longest delay.
    pub fn with_times(mut self, times: ChannelTimes) -> Result<Self, ChannelTimesError> {
        if !times.is_linked() && times.fixed().len() != self.width() - 1 {
            return Err(ChannelTimesError::Length {
                width: self.width(),
                expected: self.width() - 1,
                got: times.fixed().len(),
            });
        }
        let max = self.max_delay.get();
        if let Some(i) = times.fixed().iter().position(|t| t.get() > max.get()) {
            return Err(ChannelTimesError::Past {
                channel: i + 1,
                time: times.fixed()[i],
                max,
            });
        }
        self.times = times;
        Ok(self)
    }

    /// The channels.
    #[must_use]
    pub fn width(&self) -> usize {
        usize::from(self.input.layout().count())
    }

    /// The longest delay a line holds: the [`Domain`](crate::Domain) of the time.
    #[must_use]
    pub const fn max_delay(&self) -> MaxDelay {
        self.max_delay
    }

    /// The interpolation of the reads.
    #[must_use]
    pub const fn interpolation(&self) -> InterpolationMode {
        self.interpolation
    }

    /// The routing of the cross-feedback.
    #[must_use]
    pub const fn routing(&self) -> &Routing {
        &self.routing
    }

    /// The times of the channels past the first.
    #[must_use]
    pub const fn times(&self) -> &ChannelTimes {
        &self.times
    }
}

impl Family for DelayFamily {
    type Controls = DelayControls;
    type Member = DelayMember;
    type Recurrence = DelayRecurrence;

    /// Solved on the stream's grid of 16 frames, like the filters: a cell that moves
    /// inside a block reaches the render within 16 frames, wherever the blocks fall.
    const CADENCE: Cadence = COEFF_STREAM;

    fn channels(&self) -> Channels {
        Channels::same(self.input)
    }

    fn blank(&self, rate: SampleRate) -> DelayMember {
        DelayMember::of(self, &DelayControls::DEFAULT, rate)
    }

    /// The controls, made runnable: a time that is not finite is the default's and one
    /// out of range is clamped into `0..=max_delay`; a feedback that is not finite is the
    /// default's (the pair is bounded where it is read, [`DelayMember::gains`]).
    fn member_into(&self, c: &DelayControls, rate: SampleRate, out: &mut DelayMember) {
        *out = DelayMember::of(self, c, rate);
    }

    fn recurrence(&self) -> DelayRecurrence {
        DelayRecurrence {
            width: self.width(),
            interpolation: self.interpolation,
            linked: self.times.is_linked(),
            routing: self.routing,
            routed: !self.routing.is_none(),
            max_secs: self.max_delay.get().to_f32(),
            rate: SampleRate::DEFAULT,
            cap: 0,
        }
    }
}

impl Grows for DelayFamily {
    type Domain = MaxDelay;

    /// A line of the longest delay, and the echoes of it at the loop's greatest gain,
    /// [`Feedback::MAX_STABLE`].
    fn reach(&self, reach: &Reach<MaxDelay>) -> DelayCapacity {
        DelayCapacity::worst(
            reach.domain().frames(reach.rate()),
            f64::from(Feedback::MAX_STABLE.get()),
            reach.floor(),
            reach.rate(),
        )
    }
}

/// The coefficient point of a delay: each channel's delay in frames, and the feedback and
/// cross-feedback as asked (the pair is bounded per frame, after the glide).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DelayTarget {
    delays: [f32; MAX_DELAY_CHANNELS],
    feedback: f32,
    cross: f32,
}

/// The coefficients in flight: channel 0's delay gliding at [`DELAY_SLEW`], the other
/// channels' delays landed whole (they are constants, or they follow channel 0), and the
/// feedbacks gliding over [`GAIN_GLIDE`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DelayCoeffs {
    delay: Glide<f32>,
    delays: [f32; MAX_DELAY_CHANNELS],
    feedback: Glide<f32>,
    cross: Glide<f32>,
    /// The steps a feedback glide takes: [`GAIN_GLIDE`] at the prepared
    /// rate.
    steps: u32,
}

impl Approach for DelayCoeffs {
    type Target = DelayTarget;

    fn aim(&mut self, target: &DelayTarget) {
        self.delay.aim_by(target.delays[0], Steps::Rate(DELAY_SLEW));
        self.delays = target.delays;
        self.feedback
            .aim_by(target.feedback, Steps::Fixed(self.steps));
        self.cross.aim_by(target.cross, Steps::Fixed(self.steps));
    }

    #[inline]
    fn step(&mut self) -> bool {
        // Each moves, so no short circuit.
        let (d, f, c) = (self.delay.step(), self.feedback.step(), self.cross.step());
        d | f | c
    }

    fn settle(&mut self) {
        self.delay.settle();
        self.feedback.settle();
        self.cross.settle();
    }
}

/// What a block changes: a line and a tap for each channel.
#[derive(Clone, Debug)]
pub struct DelayState {
    lines: Vec<DelayLine>,
    /// Each channel's feedback tap of the frame, read before any line is written (the
    /// coupled loop's).
    taps: Vec<f32>,
}

/// The [`Recurrence`] of a [`DelayFamily`].
///
/// K, per frame, is the loop of the module docs: an uncoupled channel is
/// [`delay_step`] (a channel at a time, since the lines are independent); a coupled one
/// reads every channel's tap before any line is written, adds the routed taps scaled by
/// the cross-feedback to each line's input, then reads the outputs. The gains are the
/// glided pair, bounded per frame ([`Feedback::stable_pair`]).
///
/// **Lane 0 is the delay time in seconds, and lane 1 the feedback, per frame.** A time
/// frame that is not finite falls back to the member's glided delay, and one that is
/// finite is clamped into the line; a feedback frame that is not finite falls back to the
/// glided feedback and is clamped into the stable range.
///
/// A re-prepare empties the lines (the rate is new, and the delays in frames were the old
/// one's).
#[derive(Clone, Copy, Debug)]
pub struct DelayRecurrence {
    width: usize,
    interpolation: InterpolationMode,
    linked: bool,
    routing: Routing,
    routed: bool,
    /// The longest delay, in seconds, as the lane's `f32`.
    max_secs: f32,
    /// The prepared rate, and the line's capacity in frames there.
    rate: SampleRate,
    cap: usize,
}

impl DelayRecurrence {
    /// The delay of a time lane's frame, in frames: the lane value clamped into
    /// `0..=max_delay`, and into the line.
    #[inline(always)]
    fn lane_frames(&self, secs: f32) -> f32 {
        let secs = Seconds::from_f32(secs.clamp(0.0, self.max_secs));
        fractional_samples(secs, self.rate).min(self.cap as f32)
    }

    /// The block, `gain(i)` the bounded `(fb, cf)` pair at frame `i` and `delay_at(c, i)`
    /// channel `c`'s delay in frames. Constant closures compile down to constants.
    #[inline(always)]
    fn kernel(
        &self,
        state: &mut DelayState,
        feed: &Feed<'_>,
        output: &mut [&mut [f32]],
        frames: Range<usize>,
        coupled: bool,
        gain: impl Fn(usize) -> (f32, f32),
        delay_at: impl Fn(usize, usize) -> f32,
    ) {
        let interp = self.interpolation;
        if !coupled {
            // Independent channels: channel-outer, one line at a time.
            for (c, line) in state.lines.iter_mut().enumerate() {
                let input = &feed.audio[c][frames.clone()];
                let out = &mut output[c][frames.clone()];
                for (n, (y, &x)) in out.iter_mut().zip(input).enumerate() {
                    let i = frames.start + n;
                    let (fb, _) = gain(i);
                    *y = delay_step(line, x, delay_at(c, i), fb, interp);
                }
            }
            return;
        }
        // Cross-fed: every channel's feedback tap is read before any line is written,
        // so the loop is frame-outer.
        let n = self.width;
        for i in frames {
            let (fb, cf) = gain(i);
            for (c, line) in state.lines.iter().enumerate() {
                let d = delay_at(c, i);
                state.taps[c] = line.read_sample((d.max(1.0) - 1.0).max(0.0), interp);
            }
            for (c, line) in state.lines.iter_mut().enumerate() {
                let mut acc = feed.audio[c][i] + state.taps[c] * fb;
                for j in 0..n {
                    let w = self.routing.weight(c, j);
                    if w != 0.0 {
                        acc += (w * cf) * state.taps[j];
                    }
                }
                line.push_sample(acc);
            }
            for (c, line) in state.lines.iter().enumerate() {
                output[c][i] = line.read_sample(delay_at(c, i), interp);
            }
        }
    }
}

impl Recurrence for DelayRecurrence {
    type Member = DelayMember;
    type Capacity = DelayCapacity;
    type Target = DelayTarget;
    type Coeffs = DelayCoeffs;
    type State = DelayState;

    const STEP: usize = 1;

    fn start(&self, member: &DelayMember) -> Start<Self> {
        let mut target = DelayTarget {
            delays: [0.0; MAX_DELAY_CHANNELS],
            feedback: 0.0,
            cross: 0.0,
        };
        self.coefficients_into(member, &mut target);
        Start {
            coeffs: DelayCoeffs {
                delay: Glide::landed(target.delays[0]),
                delays: target.delays,
                feedback: Glide::landed(target.feedback),
                cross: Glide::landed(target.cross),
                steps: 0,
            },
            state: DelayState {
                lines: (0..self.width).map(|_| DelayLine::new(0)).collect(),
                taps: vec![0.0; self.width],
            },
            target,
        }
    }

    /// Sizes a line per channel from the capacity, and the feedback glide from
    /// [`GAIN_GLIDE`] at the rate. The conversion saturates: a
    /// prepare must not panic.
    fn prepare(
        &mut self,
        capacity: &DelayCapacity,
        setup: &Setup,
        coeffs: &mut DelayCoeffs,
        state: &mut DelayState,
        _: &mut DelayTarget,
    ) {
        self.rate = setup.rate;
        self.cap = capacity.frames;
        coeffs.steps =
            u32::try_from(GAIN_GLIDE.to_samples_ceil(setup.rate).get()).unwrap_or(u32::MAX);
        state.lines = (0..self.width)
            .map(|_| DelayLine::new(capacity.frames))
            .collect();
    }

    fn coefficients_into(&self, m: &DelayMember, target: &mut DelayTarget) -> bool {
        *target = DelayTarget {
            delays: m.delays,
            feedback: m.feedback,
            cross: m.cross,
        };
        true
    }

    fn clear(&self, state: &mut DelayState) {
        for line in &mut state.lines {
            line.reset();
        }
    }

    fn run(
        &self,
        coeffs: &DelayCoeffs,
        state: &mut DelayState,
        feed: &Feed<'_>,
        output: &mut [&mut [f32]],
        frames: Range<usize>,
        _: usize,
    ) {
        let (time, feedback) = (feed.lanes[0], feed.lanes[1]);
        let delay = coeffs.delay.now();
        let (fb, cf) = (coeffs.feedback.now(), coeffs.cross.now());
        let coupled = self.routed && (cf != 0.0 || coeffs.cross.target() != 0.0);
        // The delay of channel `c`: the glided control's if it follows it, else its own.
        let base = |c: usize| {
            if c == 0 || self.linked {
                delay
            } else {
                coeffs.delays[c]
            }
        };
        match (time, feedback) {
            (None, None) => {
                // Nothing is driven: every control is a constant of the loop, the common
                // case and the one worth keeping tight.
                let (fb, cf) = Feedback::stable_pair(fb, cf);
                let (fb, cf) = (fb.get(), cf.get());
                self.kernel(
                    state,
                    feed,
                    output,
                    frames,
                    coupled,
                    |_| (fb, cf),
                    |c, _| base(c),
                );
            }
            _ => {
                // A feedback lane is clamped, and a frame that is not finite is the
                // glided feedback; self- and cross-feedback feed the same loop, so they
                // are bounded as a pair whatever drives them.
                let gain = |i: usize| -> (f32, f32) {
                    let fb =
                        feedback.map_or(fb, |s| Feedback::new_clamped(finite_or(s[i], fb)).get());
                    let (fb, cf) = Feedback::stable_pair(fb, cf);
                    (fb.get(), cf.get())
                };
                let delay_at = |c: usize, i: usize| -> f32 {
                    match time {
                        Some(s) if s[i].is_finite() => self.lane_frames(s[i]),
                        _ => base(c),
                    }
                };
                self.kernel(state, feed, output, frames, coupled, gain, delay_at);
            }
        }
    }

    fn effort(&self) -> Effort {
        Effort::Moderate
    }
}

#[cfg(test)]
mod tests;
