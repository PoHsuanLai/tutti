//! [`ModDelayFamily`]: the chorus and the flanger as a [`Family`] (design 024 section 3,
//! row 12).
//!
//! A swept delay of any width: one [`DelayLine`] per channel, each read at a delay an LFO
//! moves around a base, with feedback. The output is the delayed signal alone, the **wet**
//! signal: the blend with the dry input is the graph's mix. A [`Sweep`] says what it is
//! built to (a chorus sits at a long base delay, a flanger at a short one), a [`Stagger`]
//! how the channels' sweeps are offset, and the controls are three: the LFO's rate, the
//! sweep's depth and the feedback ([`ModDelayControls`]).
//!
//! **The LFO is state on the stream's clock.** Its phase is a field of the recurrence's
//! state, an `f32` [`Phase`] walked by the rate's increment, counted from the last `clear`
//! (and so from every `prepare`: a re-prepare restarts the sweep). It is not anchored to
//! the transport. Its sine is solved only at **control points** every
//! [`COEFF_INTERVAL`] frames of the stream and joined by chords, so a
//! block's cut does not move it: the chord of a sine over a step of `h` radians misses it
//! by at most `h^2 / 8`, which at 5 Hz and 48 kHz is 1.4e-5 of the depth (0.03 samples
//! against the deepest chorus, 40 ms), and it kinks the delay's slope at each point, a small
//! ripple on the pitch modulation. A rate that moves lands whole at its grid point and the
//! phase is continuous across it, so a rate change is click free by itself.
//!
//! **A moving depth is a pitch bend**, so its speed is bounded, not its duration: a depth
//! that moves glides at most [`DELAY_SLEW`] frames of delay per frame. The feedback glides
//! over [`GAIN_GLIDE`].
//!
//! The delay has **no latency**: the sweep is the effect, not processing delay, and PDC
//! must not delay the rest of the mix to line up with it.
//!
//! One frame of a channel is [`delay_step`], the delay family's loop: the
//! feedback tap is read one frame nearer than the delay **before** the push and the output at
//! the delay **after** it, so the loop's period is exactly the delay
//! ([`ModDelayMember::loop_frames`]) and the first echo is at the delay too: a comb
//! `1 / (1 - g z^-d)`.
//!
//! The memory is the sweep's own [`Domain`](crate::Domain), its [`MaxDelay`]: a line holds
//! that long, and `base + depth_max` fits in it by [`Sweep::new`] ([`Grows`]).

use core::ops::{Range, RangeInclusive};

use tutti_types::{
    BusTopology, Db, Feedback, Hz, MAX_BUS_CHANNELS, Phase, PhaseIncrement, SampleRate, Samples,
    Seconds,
};

use super::sweep::{Stagger, StaggerError, Sweep};
use crate::control::COEFF_STREAM;
use crate::delay::{delay_step, finite_or, read_taps, ring_seconds};
use crate::{
    Approach, COEFF_INTERVAL, Cadence, Capacity, Channels, Complex64, DELAY_SLEW, DelayLine,
    DelayMember, Effort, Family, Feed, GAIN_GLIDE, Glide, Grows, InterpolationMode, Lti, MaxDelay,
    Reach, Recurrence, Ring, Setup, Start, Steps, System,
};

/// The LFO's slowest rate: 0.01 Hz, a lane's value and a cell's alike. A rate below it
/// (or negative) reads as it.
pub const MIN_RATE: Hz = Hz(0.01);

/// The most channels a swept delay has: a bus's.
pub const MAX_MOD_DELAY_CHANNELS: usize = MAX_BUS_CHANNELS;

/// The reads are linear: no stepping under modulation, at the cost of a little damping of
/// the highs (the sweep's interpolation is not a control).
const READ: InterpolationMode = InterpolationMode::Linear;

/// The controls of a [`ModDelayFamily`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModDelayControls {
    /// The LFO's rate: how fast the delay sweeps. Chorus and flanger rates are slow,
    /// typically 0.1 to 2 Hz; faster reads as vibrato. **Floored at 0.01 Hz**, a lane's
    /// value and a cell's alike ([`MIN_RATE`]). Not finite reads as the default's.
    pub rate: Hz,
    /// How far the delay moves either side of the base, in seconds of delay (not a
    /// fraction: it is added to the base delay, unlike the phaser's unitless depth).
    /// Clamped into the sweep's `depth_min..=depth_max`. Not finite reads as the default's.
    pub depth: Seconds,
    /// How much of the delayed signal recirculates; on a flanger the most characterful
    /// control, since more sharpens the comb notches into a resonant sweep. Clamped into
    /// `0..=`[`feedback_max`](Sweep::feedback_max). Not finite reads as the default's.
    pub feedback: Feedback,
}

impl ModDelayControls {
    /// No sweep, no recirculation, at 1 Hz: what a non-finite control falls back to.
    pub const DEFAULT: Self = Self {
        rate: Hz(1.0),
        depth: Seconds(0.0),
        feedback: Feedback::NONE,
    };

    /// Chorus: 1 Hz, a 5 ms sweep, 0.3 feedback (for [`Sweep::CHORUS`]).
    pub const CHORUS: Self = Self {
        rate: Hz(1.0),
        depth: Seconds(0.005),
        feedback: Feedback(0.3),
    };

    /// Flanger: 0.5 Hz, a 2 ms sweep, 0.7 feedback (for [`Sweep::FLANGER`]).
    pub const FLANGER: Self = Self {
        rate: Hz(0.5),
        depth: Seconds(0.002),
        feedback: Feedback(0.7),
    };

    /// The General MIDI chorus: 0.4 Hz, a 1.9 ms sweep, no feedback (for
    /// [`Sweep::GENERAL_MIDI_CHORUS`]).
    pub const GENERAL_MIDI_CHORUS: Self = Self {
        rate: Hz(0.4),
        depth: Seconds(0.0019),
        feedback: Feedback::NONE,
    };
}

impl Default for ModDelayControls {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// A swept delay at one set of controls and one rate: the delay it is swept around, how
/// far, the feedback and the LFO's step.
///
/// The description the recurrence is held to. It is **linear and time invariant at a held
/// delay**, so [`Lti`] describes the member frozen at the sweep's **centre** (the base
/// delay): with a depth of zero that is the render, and with a depth it is the figure the
/// sweep moves around. [`at`](Self::at) freezes it at any position of the sweep. The
/// frozen member is the comb `O / (1 - g T)` with `O` and `T` the line's own interpolated
/// taps (so exact at every delay, the edges included): for a whole delay of `d >= 1` frames,
/// `z^-d / (1 - g z^-d)`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModDelayMember {
    rate: SampleRate,
    width: usize,
    /// The sample rate narrowed once to `f32`: the delay positions feed an interpolated
    /// read, so they keep their fraction rather than going through `Seconds::to_samples`.
    sr: f32,
    /// The centre delay in frames (fractional), before the floor of one frame.
    base_frames: f32,
    /// The line's capacity in frames: no delay is past it.
    cap: usize,
    /// The sweep's depth in seconds, clamped into the sweep's range.
    depth: f32,
    /// The feedback, clamped into `0..=feedback_max`.
    feedback: f32,
    /// The LFO's step per frame, in cycles.
    inc: PhaseIncrement,
}

impl ModDelayMember {
    fn of(family: &ModDelayFamily, c: &ModDelayControls, rate: SampleRate) -> Self {
        let d = ModDelayControls::DEFAULT;
        let sweep = &family.sweep;
        let sr = rate.get() as f32;
        let cap = sweep.max_delay().frames(rate);
        let hz = if c.rate.get().is_finite() {
            c.rate.get()
        } else {
            d.rate.get()
        };
        let depth = if c.depth.get().is_finite() {
            c.depth
        } else {
            d.depth
        };
        // `max` then `min` rather than `clamp`: the same value for a well formed sweep.
        let depth = depth
            .to_f32()
            .max(sweep.depth_min().to_f32())
            .min(sweep.depth_max().to_f32());
        let feedback = finite_or(c.feedback.get(), d.feedback.get())
            .max(0.0)
            .min(sweep.feedback_max().get());
        Self {
            rate,
            width: family.width(),
            sr,
            base_frames: (sweep.base().to_f32() * sr).min(cap as f32),
            cap,
            depth,
            feedback,
            inc: PhaseIncrement::per_sample(Hz(hz.max(MIN_RATE.get())), rate),
        }
    }

    /// The channels.
    #[must_use]
    pub fn width(&self) -> usize {
        self.width
    }

    /// The delay at the sweep's centre, in frames (fractional): the base, floored at one
    /// frame (a delay is at least a frame: the feedback tap is read one before it).
    #[must_use]
    pub fn centre_frames(&self) -> f32 {
        self.base_frames.max(1.0)
    }

    /// How far the sweep reaches either side of the centre, in frames: the depth, clamped
    /// into the sweep's range, in delay.
    #[must_use]
    pub fn depth_frames(&self) -> f32 {
        self.depth * self.sr
    }

    /// The feedback, clamped into the sweep's range.
    #[must_use]
    pub fn feedback(&self) -> Feedback {
        Feedback(self.feedback)
    }

    /// The delays the sweep reaches, in frames: from the shortest, `base - depth` floored
    /// at one frame (a `depth` past the base flat-bottoms the sweep), to the longest,
    /// `base + depth`, which is inside the line.
    #[must_use]
    pub fn delay_range(&self) -> RangeInclusive<f32> {
        let reach = self.depth_frames();
        let longest = (self.base_frames + reach).max(1.0).min(self.cap as f32);
        let shortest = (self.base_frames - reach).max(1.0).min(longest);
        shortest..=longest
    }

    /// The same sweep frozen at LFO position `lfo` (in `-1..=1`): a member with no depth
    /// whose centre is the delay there, `base + lfo * depth` floored at one frame. The
    /// position the contract's frozen probes are at is `at(0.0)`.
    #[must_use]
    pub fn at(&self, lfo: f32) -> Self {
        let delay = (self.base_frames + lfo * self.depth * self.sr)
            .max(1.0)
            .min(self.cap as f32);
        Self {
            base_frames: delay,
            depth: 0.0,
            ..*self
        }
    }

    /// The feedback taps of the loop: the line read one frame nearer than the delay, before
    /// the push (`k` frames back is `z^-(k + 1)`), as [`delay_step`] reads
    /// it.
    fn feedback_taps(&self) -> ([(usize, f32); 4], usize) {
        read_taps(
            (self.centre_frames().max(1.0) - 1.0).max(0.0),
            self.cap,
            READ,
        )
    }

    /// The frames the loop reaches back: the farthest feedback tap with a weight, plus the
    /// frame it is read behind the push. For a whole delay of `d` frames that is `d`, the
    /// loop's period; a fractional one reaches a frame farther.
    #[must_use]
    pub fn loop_frames(&self) -> usize {
        let (taps, n) = self.feedback_taps();
        // A tap with no weight (the neighbour of a whole delay) reaches nothing.
        let reach = taps[..n].iter().filter(|t| t.1 != 0.0).map(|t| t.0).max();
        reach.unwrap_or(0) + 1
    }
}

impl System for ModDelayMember {
    /// Zero, whatever the sweep: the modulated delay is the effect's sound, not
    /// processing latency.
    fn latency(&self) -> Samples {
        Samples::ZERO
    }

    /// The echoes still to come, to `floor` under the first: the loop's period (the
    /// longest delay) a trip, and as many trips as it takes the feedback to fall to it.
    fn tail(&self, floor: Db) -> Ring {
        let longest = *self.delay_range().end();
        Ring::Finite(ring_seconds(
            longest.ceil() as usize,
            DelayMember::trips(f64::from(self.feedback), floor),
            self.rate,
        ))
    }
}

impl Lti for ModDelayMember {
    fn inputs(&self) -> usize {
        self.width
    }

    fn outputs(&self) -> usize {
        self.width
    }

    /// The response of `output` to its own input: `O / (1 - g T)` at the centre delay,
    /// the same on every channel (the channels differ only by the phase their sweeps
    /// start at, which a frozen member has none of).
    fn frequency_response(&self, output: usize, freqs: &[Hz], gain: &mut [Complex64]) {
        if output >= self.width {
            gain.fill(Complex64::new(f64::NAN, f64::NAN));
            return;
        }
        let (taps, n) = read_taps(self.centre_frames(), self.cap, READ);
        let (ftaps, ftn) = self.feedback_taps();
        let g = f64::from(self.feedback);
        for (out, f) in gain.iter_mut().zip(freqs) {
            let w = std::f64::consts::TAU * f64::from(f.get()) / self.rate.get();
            // `z^-k` for a tap `k` frames back.
            let z = |k: usize| Complex64::from_polar(1.0, -w * k as f64);
            // The output is read after the push (`k` frames back is `z^-k`), the feedback
            // tap one frame nearer, before it (`z^-(k+1)`): the same delay.
            let o: Complex64 = taps[..n].iter().map(|&(k, a)| z(k) * f64::from(a)).sum();
            let t: Complex64 = ftaps[..ftn]
                .iter()
                .map(|&(k, a)| z(k + 1) * f64::from(a))
                .sum();
            *out = o / (Complex64::new(1.0, 0.0) - t * g);
        }
    }

    /// `g^(1/K)`: every pole is inside it. A pole `z` has `1 = |g T(z)|`, and
    /// `|T(z)| <= |z|^-K` for `|z| < 1` (the linear read's taps are a convex blend), with `K`
    /// the most frames back the loop reaches ([`loop_frames`](Self::loop_frames)).
    fn spectral_radius(&self) -> f64 {
        let g = f64::from(self.feedback);
        if g <= 0.0 {
            return 0.0;
        }
        g.powf(1.0 / self.loop_frames() as f64)
    }

    /// `1 / (1 - g)`: `|v| <= 1 / (1 - g)` for `|x| <= 1` (the loop's geometric series),
    /// and the output is a convex blend of `v`.
    fn l1_norm(&self) -> f64 {
        let g = f64::from(self.feedback);
        if g < 1.0 {
            1.0 / (1.0 - g)
        } else {
            f64::INFINITY
        }
    }
}

/// The sizes a swept delay allocates: the longest delay a line holds, and how long the
/// echoes of the longest sweep ring ([`Capacity`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModDelayCapacity {
    /// The longest delay a line holds, in frames. A line is one frame longer, so a read at
    /// exactly this still has a neighbour to blend toward.
    pub frames: usize,
    tail: Ring,
}

impl Capacity for ModDelayCapacity {
    type Source = ModDelayMember;

    fn need(member: &ModDelayMember, floor: Db) -> Self {
        Self {
            frames: member.delay_range().end().ceil() as usize,
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

/// The [`Family`] of the chorus and the flanger: `input` wide, each channel a line swept by
/// an LFO, its [`Sweep`] and [`Stagger`] construction constants.
///
/// A chorus is [`ModDelayFamily::chorus`] and a flanger [`flanger`](Self::flanger): the
/// same family at different numbers. [`general_midi_chorus`](Self::general_midi_chorus) is
/// the chorus send of a General MIDI synthesizer, with no feedback.
#[derive(Clone, Copy, Debug)]
pub struct ModDelayFamily {
    input: BusTopology,
    sweep: Sweep,
    stagger: Stagger,
}

impl ModDelayFamily {
    /// A swept delay on the bus `input`, built to `sweep`, its channels' sweeps offset by
    /// `stagger`.
    ///
    /// # Errors
    ///
    /// [`StaggerError::Length`] when `stagger` names an offset for each channel
    /// ([`Stagger::each`]) and not for the `input`'s.
    pub fn new(input: BusTopology, sweep: Sweep, stagger: Stagger) -> Result<Self, StaggerError> {
        let width = usize::from(input.layout().count());
        match stagger.channels() {
            Some(got) if got != width => Err(StaggerError::Length { width, got }),
            _ => Ok(Self {
                input,
                sweep,
                stagger,
            }),
        }
    }

    /// A chorus: [`Sweep::CHORUS`], channel `c` a quarter cycle of the LFO behind channel
    /// `c - 1`. Sweeping the channels out of step is what makes it feel wide.
    #[must_use]
    pub fn chorus(input: BusTopology) -> Self {
        Self::preset(input, Sweep::CHORUS, 0.25)
    }

    /// A flanger: [`Sweep::FLANGER`], channels a half cycle apart.
    #[must_use]
    pub fn flanger(input: BusTopology) -> Self {
        Self::preset(input, Sweep::FLANGER, 0.5)
    }

    /// The General MIDI chorus: [`Sweep::GENERAL_MIDI_CHORUS`], channels a quarter cycle
    /// apart, no feedback.
    #[must_use]
    pub fn general_midi_chorus(input: BusTopology) -> Self {
        Self::preset(input, Sweep::GENERAL_MIDI_CHORUS, 0.25)
    }

    fn preset(input: BusTopology, sweep: Sweep, step: f32) -> Self {
        Self {
            input,
            sweep,
            stagger: Stagger::of_step(PhaseIncrement(step)),
        }
    }

    /// The channels.
    #[must_use]
    pub fn width(&self) -> usize {
        usize::from(self.input.layout().count())
    }

    /// What the delay is swept around and how far.
    #[must_use]
    pub const fn sweep(&self) -> &Sweep {
        &self.sweep
    }

    /// The longest delay a line holds: the [`Domain`](crate::Domain) of the sweep.
    #[must_use]
    pub const fn max_delay(&self) -> MaxDelay {
        self.sweep.max_delay()
    }

    /// How the channels' sweeps are offset.
    #[must_use]
    pub const fn stagger(&self) -> &Stagger {
        &self.stagger
    }
}

impl Family for ModDelayFamily {
    type Controls = ModDelayControls;
    type Member = ModDelayMember;
    type Recurrence = ModDelayRecurrence;

    /// Solved on the stream's grid of 16 frames, like the filters: a cell that moves
    /// inside a block reaches the render within 16 frames, wherever the blocks fall, and
    /// the LFO's control points are on the same grid.
    const CADENCE: Cadence = COEFF_STREAM;

    fn channels(&self) -> Channels {
        Channels::same(self.input)
    }

    fn blank(&self, rate: SampleRate) -> ModDelayMember {
        ModDelayMember::of(self, &ModDelayControls::DEFAULT, rate)
    }

    /// The controls, made runnable: a rate below 0.01 Hz is 0.01 Hz, a depth out of range
    /// is clamped into the sweep's, a feedback into `0..=feedback_max`; a control that is
    /// not finite is the default's.
    fn member_into(&self, c: &ModDelayControls, rate: SampleRate, out: &mut ModDelayMember) {
        *out = ModDelayMember::of(self, c, rate);
    }

    fn recurrence(&self) -> ModDelayRecurrence {
        let mut offsets = [PhaseIncrement(0.0); MAX_MOD_DELAY_CHANNELS];
        for (c, o) in offsets.iter_mut().enumerate().take(self.width()) {
            *o = self.stagger.offset(c);
        }
        ModDelayRecurrence {
            width: self.width(),
            offsets,
        }
    }
}

impl Grows for ModDelayFamily {
    type Domain = MaxDelay;

    /// A line of the longest delay, and the echoes of it at the sweep's most feedback.
    fn reach(&self, reach: &Reach<MaxDelay>) -> ModDelayCapacity {
        let frames = reach.domain().frames(reach.rate());
        ModDelayCapacity {
            frames,
            tail: Ring::Finite(ring_seconds(
                frames,
                DelayMember::trips(f64::from(self.sweep.feedback_max().get()), reach.floor()),
                reach.rate(),
            )),
        }
    }
}

/// The coefficient point of a swept delay: the centre delay in frames, the depth in
/// seconds, the feedback and the LFO's step.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModDelayTarget {
    base: f32,
    sr: f32,
    depth: f32,
    feedback: f32,
    inc: PhaseIncrement,
}

/// The coefficients in flight: the depth gliding at [`DELAY_SLEW`] (a pitch bend's speed),
/// the feedback over [`GAIN_GLIDE`], and the centre delay, the rate and the LFO's step
/// landed whole (a rate change is phase continuous by itself).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModDelayCoeffs {
    base: f32,
    sr: f32,
    depth: Glide<f32>,
    feedback: Glide<f32>,
    inc: PhaseIncrement,
    /// The steps a feedback glide takes: [`GAIN_GLIDE`] at the prepared rate.
    steps: u32,
}

impl Approach for ModDelayCoeffs {
    type Target = ModDelayTarget;

    fn aim(&mut self, target: &ModDelayTarget) {
        self.base = target.base;
        self.sr = target.sr;
        self.inc = target.inc;
        // The depth is in seconds and the slew in frames of delay per frame: a depth that
        // moves `s` seconds moves the delay `s * sr` frames.
        self.depth
            .aim_by(target.depth, Steps::Rate(DELAY_SLEW / target.sr));
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

/// What a block changes: a line for each channel, and the LFO.
#[derive(Clone, Debug)]
pub struct ModDelayState {
    lines: Vec<DelayLine>,
    /// Per channel, the LFO's sine at the two control points around the current position:
    /// `[the segment's start, its end]`.
    points: Vec<[f32; 2]>,
    /// The LFO's phase at the segment's **end**, where `points` last solved (before the
    /// first segment, its start, which is where the LFO begins).
    phase: Phase,
    /// Frames since the last control point, in `0..COEFF_INTERVAL`: counted from the last
    /// `clear`, so where the points fall does not depend on how the stream is cut.
    pos: usize,
    /// Whether `points` hold the current segment's sines: false until the first segment
    /// after a `clear`.
    primed: bool,
}

/// The [`Recurrence`] of a [`ModDelayFamily`].
///
/// K, per frame, is the swept loop of the module docs, a channel at a time (the lines are
/// independent): [`delay_step`] at the delay, which reads the feedback
/// tap, adds it scaled by the feedback to the input, pushes, and reads the output. The delay is
/// `base + lfo * depth`, floored at one frame, with `lfo` the chord between the sine's two
/// control points.
///
/// A `prepare` empties the lines (the rate is new, and the delays in frames were the old
/// one's) **and restarts the sweep**: the LFO's phase is zero again, like after `clear`.
#[derive(Clone, Copy, Debug)]
pub struct ModDelayRecurrence {
    width: usize,
    /// Each channel's LFO phase offset (channel 0's is zero), wrapped.
    offsets: [PhaseIncrement; MAX_MOD_DELAY_CHANNELS],
}

impl ModDelayRecurrence {
    /// Channel `c`'s sine at the LFO phase `p`.
    #[inline]
    fn sine(&self, c: usize, p: Phase) -> f32 {
        // Channel 0's offset is zero; `offset_by(0)` returns the phase unchanged.
        p.offset_by(self.offsets[c]).to_radians().get().sin()
    }

    /// Steps the control points on: the segment that ended is the one that begins, and
    /// the new one ends [`COEFF_INTERVAL`] frames on at the LFO's step `inc`.
    fn solve(&self, state: &mut ModDelayState, inc: PhaseIncrement) {
        let start = state.phase;
        let mut end = start;
        for _ in 0..COEFF_INTERVAL {
            end = end.advance(inc);
        }
        for c in 0..self.width {
            let first = if state.primed {
                state.points[c][1]
            } else {
                self.sine(c, start)
            };
            state.points[c] = [first, self.sine(c, end)];
        }
        state.phase = end;
        state.primed = true;
    }
}

impl Recurrence for ModDelayRecurrence {
    type Member = ModDelayMember;
    type Capacity = ModDelayCapacity;
    type Target = ModDelayTarget;
    type Coeffs = ModDelayCoeffs;
    type State = ModDelayState;

    const STEP: usize = 1;

    fn start(&self, member: &ModDelayMember) -> Start<Self> {
        let mut target = ModDelayTarget {
            base: 0.0,
            sr: 1.0,
            depth: 0.0,
            feedback: 0.0,
            inc: PhaseIncrement(0.0),
        };
        self.coefficients_into(member, &mut target);
        Start {
            coeffs: ModDelayCoeffs {
                base: target.base,
                sr: target.sr,
                depth: Glide::landed(target.depth),
                feedback: Glide::landed(target.feedback),
                inc: target.inc,
                steps: 0,
            },
            state: ModDelayState {
                lines: (0..self.width).map(|_| DelayLine::new(0)).collect(),
                points: vec![[0.0; 2]; self.width],
                phase: Phase::START,
                pos: 0,
                primed: false,
            },
            target,
        }
    }

    /// Sizes a line per channel from the capacity, and the feedback glide from
    /// [`GAIN_GLIDE`] at the rate. The conversion saturates: a prepare must not panic. The
    /// sweep restarts.
    fn prepare(
        &mut self,
        capacity: &ModDelayCapacity,
        setup: &Setup,
        coeffs: &mut ModDelayCoeffs,
        state: &mut ModDelayState,
        _: &mut ModDelayTarget,
    ) {
        coeffs.steps =
            u32::try_from(GAIN_GLIDE.to_samples_ceil(setup.rate).get()).unwrap_or(u32::MAX);
        state.lines = (0..self.width)
            .map(|_| DelayLine::new(capacity.frames))
            .collect();
        self.clear(state);
    }

    fn coefficients_into(&self, m: &ModDelayMember, target: &mut ModDelayTarget) -> bool {
        *target = ModDelayTarget {
            base: m.base_frames,
            sr: m.sr,
            depth: m.depth,
            feedback: m.feedback,
            inc: m.inc,
        };
        true
    }

    fn clear(&self, state: &mut ModDelayState) {
        for line in &mut state.lines {
            line.reset();
        }
        state.points.fill([0.0; 2]);
        state.phase = Phase::START;
        state.pos = 0;
        state.primed = false;
    }

    fn run(
        &self,
        coeffs: &ModDelayCoeffs,
        state: &mut ModDelayState,
        feed: &Feed<'_>,
        output: &mut [&mut [f32]],
        frames: Range<usize>,
        _: usize,
    ) {
        const STEP: f32 = 1.0 / COEFF_INTERVAL as f32;
        let (base, sr) = (coeffs.base, coeffs.sr);
        let (depth, feedback) = (coeffs.depth.now(), coeffs.feedback.now());
        let mut i = frames.start;
        while i < frames.end {
            if state.pos == 0 {
                self.solve(state, coeffs.inc);
            }
            let end = i + (COEFF_INTERVAL - state.pos).min(frames.end - i);
            let channels = state
                .points
                .iter()
                .zip(state.lines.iter_mut())
                .zip(feed.audio)
                .zip(output.iter_mut());
            for (((&[prev, next], line), input), out) in channels.take(self.width) {
                let slope = (next - prev) * STEP;
                for (pos, k) in (state.pos..).zip(i..end) {
                    let lfo = prev + slope * pos as f32;
                    let delay = (base + lfo * depth * sr).max(1.0);
                    out[k] = delay_step(line, input[k], delay, feedback, READ);
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
