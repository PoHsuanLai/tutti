//! [`PhaserFamily`]: the phaser as a [`Family`].
//!
//! A swept all-pass chain of any width: `stages` first-order all-passes per channel, whose
//! centre frequency an LFO moves across a [`SweepRange`], with the chain's output fed back
//! into its input. The output is the chain's, the **wet** signal: an all-pass chain alone
//! barely changes the magnitude, and the cancellation notches appear where it is blended
//! against the dry input, which is the graph's mix. It holds no delay line, which is why a
//! phaser's notches are fewer and unevenly spaced where a flanger's are a comb.
//!
//! **The LFO is state on the stream's clock.** Its phase is a field of the recurrence's
//! state, an `f32` [`Phase`] walked by the rate's increment, counted from the last `clear`
//! (and so from every `prepare`: **a re-prepare restarts the sweep**). It is not anchored to
//! the transport. The chain's coefficient is solved at **control points** every
//! [`COEFF_INTERVAL`] frames of the stream (a `tan` per point and channel) and joined by
//! chords, so the cut of the blocks does not move it: the render is the same whatever the
//! blocks, the property the move onto the stream grid is for. The chord of the sweep over a
//! step misses it by a few millionths at the sweep rates a phaser has (3.5e-6 for 2 Hz), and
//! is exact at a depth of zero, where every point is equal. A rate that moves lands whole at
//! its grid point and the phase is continuous across it.
//!
//! A depth and a feedback that move glide over [`GAIN_GLIDE`]: a depth move shifts the
//! notches, and the feedback is a loop gain.
//!
//! The chain has **no latency**. Its tail is a ring that no closed form bounds
//! ([`Ring::Unbounded`], and the node declares it unknown): the closed loop's poles are the
//! roots of a polynomial of degree `stages + 1`, and the small-gain argument that proves
//! them inside the unit circle ([`PhaserMember::spectral_radius`]) gives no rate a tail
//! could be cut at.

use core::ops::Range;

use tutti_types::{
    BusTopology, Db, Depth, Feedback, Hz, MAX_BUS_CHANNELS, Phase, PhaseIncrement, SampleRate,
    Samples,
};

use super::allpass::{allpass_chain, allpass_coeff, sweep_hz};
use super::mod_delay::MIN_RATE;
use super::sweep::{Stagger, StaggerError};
use crate::control::COEFF_STREAM;
use crate::delay::finite_or;
use crate::{
    Approach, COEFF_INTERVAL, Cadence, Channels, Complex64, Effort, Family, Feed, GAIN_GLIDE,
    Glide, Lti, Recurrence, Ring, Setup, Start, Steps, System,
};

/// The most channels a phaser has: a bus's.
pub const MAX_PHASER_CHANNELS: usize = MAX_BUS_CHANNELS;

/// How many first-order all-pass sections a phaser's chain has, per channel: 2 to 12.
///
/// Stages come in pairs, each pair making one notch: 4 gives the classic two-notch phaser
/// and higher counts thicken the effect.
///
/// A range-checked value: [`new`](Self::new) refuses a
/// count the chain is not defined for and never clamps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Stages(u8);

/// A refused stage count: the raw it carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0} stages: a phaser has {min} to {max}", min = Stages::MIN, max = Stages::MAX)]
pub struct StagesError(pub usize);

impl Stages {
    /// The fewest stages: 2, one notch.
    pub const MIN: usize = 2;
    /// The most stages: 12, six notches.
    pub const MAX: usize = 12;

    /// `count` stages, or `None` when it is not in `2..=12`. Never clamps.
    #[must_use]
    pub const fn new(count: usize) -> Option<Self> {
        if count >= Self::MIN && count <= Self::MAX {
            Some(Self(count as u8))
        } else {
            None
        }
    }

    /// A stage count for a literal or a constant.
    ///
    /// # Panics
    ///
    /// If [`new`](Self::new) refuses `count`.
    #[must_use]
    pub const fn of(count: usize) -> Self {
        match Self::new(count) {
            Some(s) => s,
            None => panic!("a phaser has two to twelve stages"),
        }
    }

    /// The count.
    #[must_use]
    pub const fn get(self) -> usize {
        self.0 as usize
    }
}

impl TryFrom<usize> for Stages {
    type Error = StagesError;

    fn try_from(count: usize) -> Result<Self, StagesError> {
        Self::new(count).ok_or(StagesError(count))
    }
}

impl From<Stages> for usize {
    fn from(s: Stages) -> usize {
        s.get()
    }
}

/// The band the all-pass centre sweeps across: its trough `min` and its peak `max`, in
/// [`Hz`].
///
/// A range-checked value: [`new`](Self::new) refuses a bound that is not finite, a `min`
/// under [`FLOOR`](Self::FLOOR) (20 Hz) and a `min` above the `max`, and never clamps. What
/// depends on the **rate** stays the member's: the sweep's top is held at 0.90 of Nyquist
/// there, a wider margin than the filters take because several all-pass stages compound
/// their phase error near the limit, and the authored top is kept, so a later, higher rate
/// restores it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SweepRange {
    min: Hz,
    max: Hz,
}

/// A refused [`SweepRange`]: the raws it carries.
#[derive(Clone, Copy, Debug, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum SweepRangeError {
    /// A bound is not finite.
    #[error("the sweep range {min:?}..{max:?} is not finite")]
    NotFinite {
        /// The refused bottom.
        min: Hz,
        /// The refused top.
        max: Hz,
    },
    /// The bottom is under [`SweepRange::FLOOR`].
    #[error("the sweep bottom {0:?} is under the {floor:?} floor", floor = SweepRange::FLOOR)]
    BelowFloor(Hz),
    /// The bottom is above the top.
    #[error("the sweep bottom {min:?} is above its top {max:?}")]
    Order {
        /// The refused bottom.
        min: Hz,
        /// The refused top.
        max: Hz,
    },
}

impl SweepRange {
    /// The lowest bottom a sweep has: 20 Hz.
    pub const FLOOR: Hz = Hz(20.0);

    /// The default sweep: 200 to 4000 Hz.
    pub const DEFAULT: Self = Self::of(Hz(200.0), Hz(4_000.0));

    /// A sweep from `min` to `max`.
    ///
    /// # Errors
    ///
    /// [`SweepRangeError::NotFinite`], [`BelowFloor`](SweepRangeError::BelowFloor) and
    /// [`Order`](SweepRangeError::Order).
    pub const fn new(min: Hz, max: Hz) -> Result<Self, SweepRangeError> {
        let (lo, hi) = (min.get(), max.get());
        if !(lo.is_finite() && hi.is_finite()) {
            return Err(SweepRangeError::NotFinite { min, max });
        }
        if lo < Self::FLOOR.get() {
            return Err(SweepRangeError::BelowFloor(min));
        }
        if lo > hi {
            return Err(SweepRangeError::Order { min, max });
        }
        Ok(Self { min, max })
    }

    /// A sweep for a literal or a constant.
    ///
    /// # Panics
    ///
    /// If [`new`](Self::new) refuses it.
    #[must_use]
    pub const fn of(min: Hz, max: Hz) -> Self {
        match Self::new(min, max) {
            Ok(r) => r,
            Err(_) => panic!("a sweep range is finite, from 20 Hz up and in order"),
        }
    }

    /// The bottom: where the all-pass centre sits at the LFO's trough.
    #[must_use]
    pub const fn min(&self) -> Hz {
        self.min
    }

    /// The authored top: where the centre sits at the LFO's peak, before the rate holds it
    /// at 0.90 of Nyquist.
    #[must_use]
    pub const fn max(&self) -> Hz {
        self.max
    }
}

impl Default for SweepRange {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// The controls of a [`PhaserFamily`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PhaserControls {
    /// The LFO's rate: how fast the notches sweep. Phaser rates are slow, typically 0.1 to
    /// 2 Hz. **Floored at 0.01 Hz**, a lane's value and a cell's alike ([`MIN_RATE`]): a
    /// negative rate does not run the sweep backwards. Not finite reads as the default's.
    pub rate: Hz,
    /// How much of the [`SweepRange`] the sweep covers: **unitless**, `0..=1` (a chorus's
    /// depth is seconds of delay). 1 sweeps the whole range, 0 parks the notches at its
    /// bottom. A negative depth sweeps down from the bottom (held above 1 Hz). A
    /// [`Depth`] is in `-1..=1` and finite by its type.
    pub depth: Depth,
    /// How much of the chain's output recirculates into its input: deepens and sharpens the
    /// notches. Clamped into `0..=`[`Feedback::MAX_STABLE`]; not finite reads as the
    /// default's.
    pub feedback: Feedback,
}

impl PhaserControls {
    /// 0.3 Hz, half depth, half feedback: where a phaser starts, and what a control that is
    /// not finite falls back to.
    pub const DEFAULT: Self = Self {
        rate: Hz(0.3),
        depth: Depth::of(0.5),
        feedback: Feedback(0.5),
    };
}

impl Default for PhaserControls {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// A phaser at one set of controls and one rate: the band it sweeps, how deep, the feedback
/// and the LFO's step.
///
/// The description the recurrence is held to. It is **linear and time invariant at a held
/// sweep position**, so [`Lti`] describes the member frozen at the sweep's **centre**
/// (LFO position 0): with a depth of zero that is the render, parked at the bottom of the
/// range, and with a depth it is the figure the sweep moves around. [`at`](Self::at) freezes
/// it at any position. The frozen system is
/// `H(z) = A(z) / (1 - g z^-1 A(z))`, `A` the `stages`-th power of the first-order all-pass
/// `(c + z^-1) / (1 + c z^-1)` with `c =` [`allpass_coeff`] of the centre: `|A| = 1` on the
/// unit circle, so at `g = 0` the response is flat and its phase is `stages` times a stage's.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PhaserMember {
    rate: SampleRate,
    width: usize,
    stages: Stages,
    /// The sample rate narrowed once to `f32`, for the coefficient.
    sr: f32,
    /// The sweep's bottom.
    min_hz: Hz,
    /// The sweep's top: the authored one, held at 0.90 of Nyquist at this rate.
    max_hz: Hz,
    /// The sweep depth: a [`Depth`] is in `-1..=1` and finite by its type.
    depth: Depth,
    /// The feedback, clamped into `0..=MAX_STABLE`.
    feedback: f32,
    /// The LFO's step per frame, in cycles.
    inc: PhaseIncrement,
}

impl PhaserMember {
    fn of(family: &PhaserFamily, c: &PhaserControls, rate: SampleRate) -> Self {
        let d = PhaserControls::DEFAULT;
        let hz = finite_or(c.rate.get(), d.rate.get());
        let feedback = Feedback::new_clamped(finite_or(c.feedback.get(), d.feedback.get())).get();
        Self {
            rate,
            width: family.width(),
            stages: family.stages,
            sr: rate.get() as f32,
            min_hz: family.range.min(),
            max_hz: family.range.max().min(rate.nyquist_scaled(0.90)),
            depth: c.depth,
            feedback,
            inc: PhaseIncrement::per_sample(Hz(hz.max(MIN_RATE.get())), rate),
        }
    }

    /// The channels.
    #[must_use]
    pub fn width(&self) -> usize {
        self.width
    }

    /// The all-pass sections per channel.
    #[must_use]
    pub fn stages(&self) -> Stages {
        self.stages
    }

    /// The sweep depth.
    #[must_use]
    pub fn depth(&self) -> Depth {
        self.depth
    }

    /// The feedback, clamped into `0..=MAX_STABLE`.
    #[must_use]
    pub fn feedback(&self) -> Feedback {
        Feedback(self.feedback)
    }

    /// The all-pass centre at the sweep's centre (LFO position 0), in [`Hz`]:
    /// [`sweep_hz`] there. With a depth of zero it is the bottom of the range.
    #[must_use]
    fn centre_hz(&self) -> Hz {
        sweep_hz(self.min_hz, self.max_hz, 0.0, self.depth)
    }

    /// The all-pass coefficient at the centre: [`allpass_coeff`] of the centre frequency.
    #[must_use]
    pub fn coefficient(&self) -> f32 {
        allpass_coeff(self.centre_hz().get(), self.sr)
    }

    /// The same sweep frozen at LFO position `lfo` (in `-1..=1`): a member with no depth,
    /// parked where the sweep is there. The position the contract's frozen probes are at is
    /// `at(0.0)`.
    #[must_use]
    pub fn at(&self, lfo: f32) -> Self {
        let f = sweep_hz(self.min_hz, self.max_hz, lfo, self.depth);
        Self {
            min_hz: f,
            max_hz: f,
            depth: Depth::of(0.0),
            ..*self
        }
    }

    /// The feedback loop's `z^-1 A(z)` has modulus at most `g r ((r - |c|) / (1 - |c| r))^N`
    /// on a circle `|w| = r` (`w = z^-1`, `1 < r < 1/|c|`; at `r = 1` the all-pass is 1),
    /// the largest radius `r` at which that is under 1: by the maximum principle the loop
    /// `g w A(w) = 1` has no root in `|w| <= r`, so every pole is inside `|z| < 1/r`.
    ///
    /// The bisection runs a fixed 80 halvings, so the query always ends, and returns `1/lo`,
    /// `lo` the end of the final bracket known to satisfy the condition: the safe side.
    fn pole_bound(&self) -> f64 {
        let (g, c) = (
            f64::from(self.feedback),
            f64::from(self.coefficient()).abs(),
        );
        if g <= 0.0 {
            // The `stages` poles are `-c`.
            return c;
        }
        let n = self.stages.get() as i32;
        let loop_gain = |r: f64| g * r * ((r - c) / (1.0 - c * r)).powi(n);
        // `f(r) >= g r`, so the loop gain is 1 or more by `1 / g`, and it is unbounded at
        // `1 / |c|`.
        let top = if c > 0.0 {
            (1.0 / c).min(1.0 / g)
        } else {
            1.0 / g
        };
        let (mut lo, mut hi) = (1.0f64, top);
        for _ in 0..80 {
            let mid = 0.5 * (lo + hi);
            if c * mid < 1.0 && loop_gain(mid) < 1.0 {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        1.0 / lo
    }
}

impl System for PhaserMember {
    /// Zero: the sweep is the effect, not processing latency.
    fn latency(&self) -> Samples {
        Samples::ZERO
    }

    /// [`Ring::Unbounded`]: the closed loop's poles have no closed form (see the module
    /// docs), so no time to a floor is known, and the node declares its tail unknown.
    fn tail(&self, _: Db) -> Ring {
        Ring::Unbounded
    }
}

impl Lti for PhaserMember {
    fn inputs(&self) -> usize {
        self.width
    }

    fn outputs(&self) -> usize {
        self.width
    }

    /// The response of `output` to its own input: `A / (1 - g z^-1 A)` at the centre, the
    /// same on every channel (the channels differ only by the phase their sweeps start at,
    /// which a frozen member has none of).
    fn frequency_response(&self, output: usize, freqs: &[Hz], gain: &mut [Complex64]) {
        if output >= self.width {
            gain.fill(Complex64::new(f64::NAN, f64::NAN));
            return;
        }
        let c = f64::from(self.coefficient());
        let g = f64::from(self.feedback);
        let one = Complex64::new(1.0, 0.0);
        for (out, f) in gain.iter_mut().zip(freqs) {
            let w = std::f64::consts::TAU * f64::from(f.get()) / self.rate.get();
            let zi = Complex64::from_polar(1.0, -w);
            let stage = (zi + c) / (one + zi * c);
            let a = stage.powi(self.stages.get() as i32);
            *out = a / (one - zi * a * g);
        }
    }

    /// An **upper bound** on the true spectral radius, strictly under 1 for every feedback
    /// under 1: a sufficient bound from the maximum principle on the loop `g z^-1 A(z) = 1`
    /// (`pole_bound` states it). It is exact with no feedback (the `stages` poles of `A`,
    /// at `-c`) and at `c = 0` (the loop is `1 - g z^-(N+1)`, radius `g^(1/(N+1))`). It says the loop is stable and how far from
    /// the circle at least; it is no rate to cut a tail at (the bound is loose near a
    /// coefficient of `+-1`), which is why the tail is [`Ring::Unbounded`].
    fn spectral_radius(&self) -> f64 {
        self.pole_bound()
    }

    /// Infinity: no bound on `||h||_1` is known. An all-pass stage's impulse response has an
    /// l1 norm of `1 + 2|c|`, so the chain's is not 1, and the loop's geometric series
    /// has no closed form that stays finite as the feedback nears 1.
    fn l1_norm(&self) -> f64 {
        f64::INFINITY
    }

    /// `1 / (1 - g)`: `|A| = 1` on the circle, so `|1 - g z^-1 A| >= 1 - g`. It is the
    /// peak (at DC, where `A = 1`).
    fn hinf_norm(&self) -> f64 {
        1.0 / (1.0 - f64::from(self.feedback))
    }
}

/// The [`Family`] of the phaser: `input` wide, each channel a chain of `stages` all-passes
/// swept across a [`SweepRange`] by an LFO, its channels' sweeps offset by a [`Stagger`].
///
/// Every channel sweeps in step by default ([`Stagger::NONE`]), which widens nothing by
/// itself; a stagger such as a quarter cycle between two channels sweeps the sides apart,
/// which is what widens it. **A re-prepare restarts the sweep**: the LFO is at its start
/// again, like after a `clear` (the rate is new).
#[derive(Clone, Copy, Debug)]
pub struct PhaserFamily {
    input: BusTopology,
    stages: Stages,
    range: SweepRange,
    stagger: Stagger,
}

impl PhaserFamily {
    /// A phaser on the bus `input`, `stages` all-passes per channel sweeping `range`, its
    /// channels' sweeps offset by `stagger`.
    ///
    /// # Errors
    ///
    /// [`StaggerError::Length`] when `stagger` names an offset for each channel
    /// ([`Stagger::each`]) and not for the `input`'s.
    pub fn new(
        input: BusTopology,
        stages: Stages,
        range: SweepRange,
        stagger: Stagger,
    ) -> Result<Self, StaggerError> {
        let width = usize::from(input.layout().count());
        match stagger.channels() {
            Some(got) if got != width => Err(StaggerError::Length { width, got }),
            _ => Ok(Self {
                input,
                stages,
                range,
                stagger,
            }),
        }
    }

    /// The channels.
    #[must_use]
    pub fn width(&self) -> usize {
        usize::from(self.input.layout().count())
    }

    /// The all-pass sections per channel.
    #[must_use]
    pub const fn stages(&self) -> Stages {
        self.stages
    }

    /// The band the notches sweep across.
    #[must_use]
    pub const fn range(&self) -> &SweepRange {
        &self.range
    }
}

impl Family for PhaserFamily {
    type Controls = PhaserControls;
    type Member = PhaserMember;
    type Recurrence = PhaserRecurrence;

    /// Solved on the stream's grid of 16 frames, like the filters and the chorus: a cell
    /// that moves inside a block reaches the render within 16 frames, wherever the blocks
    /// fall, and the LFO's control points are on the same grid. (A block-anchored grid
    /// made the render depend on the block's length.)
    const CADENCE: Cadence = COEFF_STREAM;

    fn channels(&self) -> Channels {
        Channels::same(self.input)
    }

    fn blank(&self, rate: SampleRate) -> PhaserMember {
        PhaserMember::of(self, &PhaserControls::DEFAULT, rate)
    }

    /// The controls, made runnable: a rate below 0.01 Hz is 0.01 Hz (a depth is in range by its
    /// type), a feedback is clamped into `0..=MAX_STABLE`, a control that is not finite is the
    /// default's, and the sweep's top is held at 0.90 of Nyquist at this rate.
    fn member_into(&self, c: &PhaserControls, rate: SampleRate, out: &mut PhaserMember) {
        *out = PhaserMember::of(self, c, rate);
    }

    fn recurrence(&self, _: SampleRate) -> PhaserRecurrence {
        let mut offsets = [PhaseIncrement(0.0); MAX_PHASER_CHANNELS];
        for (c, o) in offsets.iter_mut().enumerate().take(self.width()) {
            *o = self.stagger.offset(c);
        }
        PhaserRecurrence {
            width: self.width(),
            stages: self.stages,
            offsets,
        }
    }
}

/// The coefficient point of a phaser: the sweep band, the depth, the feedback and the LFO's
/// step.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PhaserTarget {
    min: f32,
    max: f32,
    sr: f32,
    depth: f32,
    feedback: f32,
    inc: PhaseIncrement,
}

/// The coefficients in flight: the depth and the feedback gliding over [`GAIN_GLIDE`], and
/// the band, the rate and the LFO's step landed whole (a rate change is phase continuous by
/// itself).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PhaserCoeffs {
    min: f32,
    max: f32,
    sr: f32,
    depth: Glide<f32>,
    feedback: Glide<f32>,
    inc: PhaseIncrement,
    /// The steps a glide takes: [`GAIN_GLIDE`] at the prepared rate.
    steps: u32,
}

impl Approach for PhaserCoeffs {
    type Target = PhaserTarget;

    fn aim(&mut self, target: &PhaserTarget) {
        self.min = target.min;
        self.max = target.max;
        self.sr = target.sr;
        self.inc = target.inc;
        self.depth.aim_by(target.depth, Steps::Fixed(self.steps));
        self.feedback
            .aim_by(target.feedback, Steps::Fixed(self.steps));
    }

    #[inline]
    fn step(&mut self) -> bool {
        // Each moves, so no short circuit.
        let (d, f) = (self.depth.step(), self.feedback.step());
        d | f
    }

    fn settle(&mut self) {
        self.depth.settle();
        self.feedback.settle();
    }
}

/// What a block changes: each stage's history, each channel's last output, the LFO and the
/// control points.
#[derive(Clone, Debug)]
pub struct PhaserState {
    /// Stage input history, stage-major: `x1[s * width + c]`.
    x1: Vec<f32>,
    /// Stage output history, stage-major: `y1[s * width + c]`.
    y1: Vec<f32>,
    /// Each channel's last chain output, fed back into its input.
    feedback_sample: Vec<f32>,
    /// Per channel, the all-pass coefficient at the two control points around the current
    /// position: `[the segment's start, its end]`.
    points: Vec<[f32; 2]>,
    /// The LFO's phase at the segment's **end**, where `points` last solved (before the
    /// first segment, its start, which is where the LFO begins).
    phase: Phase,
    /// Frames since the last control point, in `0..COEFF_INTERVAL`: counted from the last
    /// `clear`, so where the points fall does not depend on how the stream is cut.
    pos: usize,
    /// Whether `points` hold the current segment's coefficients: false until the first
    /// segment after a `clear`.
    primed: bool,
}

/// The [`Recurrence`] of a [`PhaserFamily`].
///
/// K, per frame, is the chain of the module docs, a frame at a time and each stage across
/// every channel: each channel's input plus its last output scaled by the feedback goes
/// through [`allpass_chain`] at the coefficient the control points give, and the chain's
/// output is the frame's and the next frame's feedback. The coefficient at a point is
/// [`allpass_coeff`] of [`sweep_hz`] at the LFO's sine there; a channel that has the same
/// stagger as the one before copies its points.
///
/// A `prepare` empties the chain (the coefficient was the old rate's) **and restarts the
/// sweep**: the LFO's phase is zero again, like after `clear`.
#[derive(Clone, Copy, Debug)]
pub struct PhaserRecurrence {
    width: usize,
    stages: Stages,
    /// Each channel's LFO phase offset (channel 0's is zero), wrapped.
    offsets: [PhaseIncrement; MAX_PHASER_CHANNELS],
}

impl PhaserRecurrence {
    /// Channel `c`'s all-pass coefficient with the LFO at `p` and the depth `depth`.
    #[inline]
    fn coefficient(&self, coeffs: &PhaserCoeffs, c: usize, p: Phase, depth: f32) -> f32 {
        let lfo = p.offset_by(self.offsets[c]).to_radians().get().sin();
        let sweep = sweep_hz(Hz(coeffs.min), Hz(coeffs.max), lfo, Depth::clamped(depth));
        allpass_coeff(sweep.get(), coeffs.sr)
    }

    /// Steps the control points on: the segment that ended is the one that begins, and the
    /// new one ends [`COEFF_INTERVAL`] frames on at the LFO's step. The end is solved with
    /// the depth the glide has there (the glide is a copy stepped as far).
    fn solve(&self, coeffs: &PhaserCoeffs, state: &mut PhaserState) {
        let start = state.phase;
        let mut end = start;
        let mut depth = coeffs.depth;
        for _ in 0..COEFF_INTERVAL {
            end = end.advance(coeffs.inc);
            depth.step();
        }
        let (depth_start, depth_end) = (coeffs.depth.now(), depth.now());
        for c in 0..self.width {
            if c > 0 && self.offsets[c] == self.offsets[c - 1] {
                state.points[c] = state.points[c - 1];
                continue;
            }
            let first = if state.primed {
                state.points[c][1]
            } else {
                self.coefficient(coeffs, c, start, depth_start)
            };
            state.points[c] = [first, self.coefficient(coeffs, c, end, depth_end)];
        }
        state.phase = end;
        state.primed = true;
    }
}

impl Recurrence for PhaserRecurrence {
    type Member = PhaserMember;
    type Capacity = ();
    type Target = PhaserTarget;
    type Coeffs = PhaserCoeffs;
    type State = PhaserState;

    const STEP: usize = 1;

    fn start(&self, member: &PhaserMember) -> Start<Self> {
        let mut target = PhaserTarget {
            min: 0.0,
            max: 0.0,
            sr: 1.0,
            depth: 0.0,
            feedback: 0.0,
            inc: PhaseIncrement(0.0),
        };
        self.coefficients_into(member, &mut target);
        let (w, n) = (self.width, self.stages.get());
        Start {
            coeffs: PhaserCoeffs {
                min: target.min,
                max: target.max,
                sr: target.sr,
                depth: Glide::landed(target.depth),
                feedback: Glide::landed(target.feedback),
                inc: target.inc,
                steps: 0,
            },
            state: PhaserState {
                x1: vec![0.0; n * w],
                y1: vec![0.0; n * w],
                feedback_sample: vec![0.0; w],
                points: vec![[0.0; 2]; w],
                phase: Phase::START,
                pos: 0,
                primed: false,
            },
            target,
        }
    }

    /// Sets the glides' length from [`GAIN_GLIDE`] at the rate (the conversion saturates: a
    /// prepare must not panic) and clears the chain. The sweep restarts.
    fn prepare(
        &mut self,
        _: &(),
        setup: &Setup,
        coeffs: &mut PhaserCoeffs,
        state: &mut PhaserState,
        _: &mut PhaserTarget,
    ) {
        coeffs.steps =
            u32::try_from(GAIN_GLIDE.to_samples_ceil(setup.rate).get()).unwrap_or(u32::MAX);
        self.clear(state);
    }

    fn coefficients_into(&self, m: &PhaserMember, target: &mut PhaserTarget) -> bool {
        *target = PhaserTarget {
            min: m.min_hz.get(),
            max: m.max_hz.get(),
            sr: m.sr,
            depth: m.depth.get(),
            feedback: m.feedback,
            inc: m.inc,
        };
        true
    }

    fn clear(&self, state: &mut PhaserState) {
        state.x1.fill(0.0);
        state.y1.fill(0.0);
        state.feedback_sample.fill(0.0);
        state.points.fill([0.0; 2]);
        state.phase = Phase::START;
        state.pos = 0;
        state.primed = false;
    }

    fn run(
        &self,
        coeffs: &PhaserCoeffs,
        state: &mut PhaserState,
        feed: &Feed<'_>,
        output: &mut [&mut [f32]],
        frames: Range<usize>,
        _: usize,
    ) {
        const STEP: f32 = 1.0 / COEFF_INTERVAL as f32;
        let w = self.width;
        let fb = coeffs.feedback.now();
        let mut lane = [0.0f32; MAX_PHASER_CHANNELS];
        let mut coeff = [0.0f32; MAX_PHASER_CHANNELS];
        let mut slope = [0.0f32; MAX_PHASER_CHANNELS];
        let mut i = frames.start;
        while i < frames.end {
            if state.pos == 0 {
                self.solve(coeffs, state);
            }
            let end = i + (COEFF_INTERVAL - state.pos).min(frames.end - i);
            for (slope, [prev, next]) in slope.iter_mut().zip(&state.points) {
                *slope = (next - prev) * STEP;
            }
            for (pos, k) in (state.pos..).zip(i..end) {
                for c in 0..w {
                    coeff[c] = state.points[c][0] + slope[c] * pos as f32;
                    lane[c] = feed.audio[c][k] + state.feedback_sample[c] * fb;
                }
                allpass_chain(
                    self.stages.get(),
                    &coeff[..w],
                    &mut lane[..w],
                    &mut state.x1,
                    &mut state.y1,
                );
                for (c, out) in output.iter_mut().enumerate().take(w) {
                    state.feedback_sample[c] = lane[c];
                    out[k] = lane[c];
                }
            }
            state.pos += end - i;
            if state.pos == COEFF_INTERVAL {
                state.pos = 0;
            }
            i = end;
        }
    }

    fn effort(&self) -> Effort {
        Effort::Moderate
    }
}

#[cfg(test)]
mod tests;
