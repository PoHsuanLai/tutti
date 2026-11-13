//! [`LimiterFamily`]: the lookahead limiter as a [`Family`] (design 024 section 3, row 7).
//!
//! A limiter delays its audio by the **lookahead** and runs its gain decision ahead of
//! the delayed signal, so a peak is already reduced when it arrives. The decision is
//! made on the **linked peak** (the loudest channel at each frame, a NaN ignored), in
//! four steps:
//!
//! 1. the curve turns the peak into a target gain, `min(drive, ceiling / peak)` with
//!    `drive = ceiling - threshold` (in dB a difference; [`LimiterCurve`]): the threshold
//!    drives the input, a signal under it is moved by the drive (a boost when the
//!    ceiling is above the threshold, a cut when below) and a louder one settles at the
//!    ceiling;
//! 2. the envelope takes that target **at once** when it is lower (an instant attack) and
//!    releases toward it by a one-pole otherwise, `e += c * (target - e)` with
//!    `c = 1 - exp(-1 / (release * rate))`;
//! 3. the minimum of the envelope over the window, the delayed frame and the lookahead
//!    after it, is the gain (so the gain that scales a sample is never later than its own
//!    target);
//! 4. every channel is that gain times its signal, delayed by the lookahead.
//!
//! The output is therefore bounded by the ceiling (a gain divide and a product can in
//! principle round one ulp past). The lookahead is the family's own constant, fixed when
//! it is built, and **is the member's latency** ([`System::latency`]): the graph's PDC
//! compensates for it. It has no tail, which excludes latency: what the delay holds
//! comes out within the latency, and gain on silence is silence.
//!
//! The threshold and the ceiling are the two controls read **per frame** when the graph
//! drives them (lanes 0 and 1 of the [`Feed`]; design 024 section 4.5, "Where cadence
//! stops"); a frame that is not finite falls back to the member's. Every coefficient
//! lands whole: a glided ceiling would let a sample past the new one.
//!
//! A prepare or a reset leaves the envelope **unset** (`+inf`, no history), and the first
//! frame takes its target at once, whatever it is: the first audio is limited from its
//! first sample (design 024 section 2.7.4, law 1), as a limiter that had heard silence
//! for ever would limit it (on silence the envelope settles at the drive, the target of a
//! zero peak, which is where an unset envelope meets its first quiet frame). Until this
//! was fixed the envelope started at 0, and the first audio faded in over about one
//! release time.

use core::ops::Range;

/// The envelope with no history: the next frame takes its target at once.
const UNSET: f32 = f32::INFINITY;

use tutti_types::{Amplitude, BusTopology, Db, SampleRate, Samples, Seconds};

use super::curves::{LimiterCurve, amplitude_to_db, compute_limiter_gain};
use super::lookahead::{LookaheadRing, fold_peak};
use crate::control::COEFF_STREAM;
use crate::smoother::Smoother;
use crate::{
    Cadence, Channels, Effort, Family, Feed, Glide, Lerp, Recurrence, Ring, Setup, Start, System,
};

/// The controls of a [`LimiterFamily`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LimiterControls {
    /// The input level, in dB, at which the curve reaches the ceiling: where reduction
    /// begins. `-inf` is a threshold (every signal is over it, so the gain is always the
    /// ceiling over the peak) and `+inf` is one (every signal is under it, so a gain of
    /// 0); only a NaN has no level and reads as the default's.
    pub threshold: Db,
    /// The level the output is bounded by, in dB. `+-inf` are ceilings (`-inf` mutes);
    /// only a NaN has no level and reads as the default's.
    pub ceiling: Db,
    /// How fast the gain recovers after a reduction: the time constant of a one-pole
    /// smoother toward the curve's gain. Floored at 1 ms (a limiter that recovers in no
    /// time is a clipper); not finite reads as the default's.
    pub release: Seconds,
}

impl LimiterControls {
    /// The usual mastering limiter: a -6 dB threshold, a -0.3 dB ceiling (the headroom
    /// an inter-sample peak needs) and a release of 100 ms. A quiet signal is raised by
    /// 5.7 dB, and the output reaches -0.3 dB when the input reaches -6 dB. What a
    /// non-finite control falls back to.
    pub const DEFAULT: Self = Self {
        threshold: Db(-6.0),
        ceiling: Db(-0.3),
        release: Seconds(0.1),
    };

    /// The shortest release a member has: 1 ms.
    pub const MIN_RELEASE: Seconds = Seconds(0.001);
}

impl Default for LimiterControls {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// How far ahead a limiter looks: the delay it runs its gain decision ahead of the
/// audio by, and so the processing latency it declares to PDC.
///
/// A range-checked value (CLAUDE.md, "Range-checked values"): finite, above zero and at
/// most [`CAP`](Self::CAP). [`new`](Self::new) refuses anything else and never clamps,
/// since the lookahead is the latency the graph compensates, and a silently different
/// figure would misalign everything downstream.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub struct Lookahead(Seconds);

/// A refused lookahead: the raw it carries.
#[derive(Clone, Copy, Debug, PartialEq, thiserror::Error)]
#[error("{0:?} is not a lookahead: it must be above zero and at most {cap:?}", cap = Lookahead::CAP)]
pub struct LookaheadError(pub Seconds);

impl Lookahead {
    /// The lookahead of a limiter built without one: 5 ms, which catches peaks
    /// transparently for a delay a mix tolerates.
    pub const DEFAULT: Self = Self(Seconds(0.005));
    /// The longest lookahead [`new`](Self::new) accepts: one second, far past any
    /// limiter's (a mastering limiter looks a few to a few tens of milliseconds ahead).
    pub const CAP: Seconds = Seconds(1.0);

    /// A lookahead of `seconds`, or `None` when it is not finite, not above zero or
    /// above [`CAP`](Self::CAP). Never clamps.
    #[must_use]
    pub const fn new(seconds: Seconds) -> Option<Self> {
        let s = seconds.get();
        if s.is_finite() && s > 0.0 && s <= Self::CAP.get() {
            Some(Self(seconds))
        } else {
            None
        }
    }

    /// A lookahead for a literal or a constant.
    ///
    /// # Panics
    ///
    /// If [`new`](Self::new) refuses `seconds`.
    #[must_use]
    pub const fn of(seconds: Seconds) -> Self {
        match Self::new(seconds) {
            Some(l) => l,
            None => panic!("a lookahead is above zero and at most one second"),
        }
    }

    /// The lookahead in seconds.
    #[must_use]
    pub const fn get(self) -> Seconds {
        self.0
    }
}

impl Default for Lookahead {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl TryFrom<Seconds> for Lookahead {
    type Error = LookaheadError;

    fn try_from(seconds: Seconds) -> Result<Self, LookaheadError> {
        Self::new(seconds).ok_or(LookaheadError(seconds))
    }
}

impl From<Lookahead> for Seconds {
    fn from(lookahead: Lookahead) -> Seconds {
        lookahead.0
    }
}

/// The longest window a limiter allocates, in frames. [`Lookahead`] already bounds the
/// seconds (one second is 192000 frames at 192 kHz); this guards only against an absurd
/// rate, since a prepare must not panic and an allocation that size would abort.
const MAX_LOOKAHEAD_FRAMES: usize = 1 << 24;

/// The lookahead window in frames at `rate`: `ceil(lookahead * rate)`, at least one (a
/// lookahead is above zero, so its ceiling is). The ring must hold at least the window,
/// so it rounds up; and it is the one figure the member's latency and the recurrence's
/// delay both are.
fn lookahead_frames(lookahead: Lookahead, rate: SampleRate) -> usize {
    lookahead
        .get()
        .to_samples_ceil(rate)
        .get()
        .clamp(1, MAX_LOOKAHEAD_FRAMES)
}

/// A limiter at one set of controls and one rate: the curve, the release, and the delay
/// it makes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LimiterMember {
    threshold: Db,
    ceiling: Db,
    release: Seconds,
    rate: SampleRate,
    /// [`lookahead_frames`] at `rate`.
    lookahead: usize,
    curve: LimiterCurve,
}

impl LimiterMember {
    /// The member of `controls` at `rate`, delaying by `lookahead`: the controls made
    /// runnable.
    fn of(controls: &LimiterControls, lookahead: Lookahead, rate: SampleRate) -> Self {
        let d = LimiterControls::DEFAULT;
        // `+-inf` are thresholds and ceilings; only a NaN has no level.
        let threshold = if controls.threshold.get().is_nan() {
            d.threshold
        } else {
            controls.threshold
        };
        let ceiling = if controls.ceiling.get().is_nan() {
            d.ceiling
        } else {
            controls.ceiling
        };
        let release = if controls.release.get().is_finite() {
            controls.release
        } else {
            d.release
        };
        Self {
            threshold,
            ceiling,
            release: Seconds(release.get().max(LimiterControls::MIN_RELEASE.get())),
            rate,
            lookahead: lookahead_frames(lookahead, rate),
            curve: LimiterCurve::new(threshold, ceiling),
        }
    }

    /// The threshold, in dB.
    #[must_use]
    pub fn threshold(&self) -> Db {
        self.threshold
    }

    /// The ceiling, in dB.
    #[must_use]
    pub fn ceiling(&self) -> Db {
        self.ceiling
    }

    /// The release time constant, at least 1 ms.
    #[must_use]
    pub fn release(&self) -> Seconds {
        self.release
    }

    /// The transfer curve: the gain at a linked peak, before the envelope.
    #[must_use]
    pub fn curve(&self) -> LimiterCurve {
        self.curve
    }
}

/// The lookahead is the member's latency: processing delay, never musical. A tail excludes
/// latency, so it has none: the delay holds the last `lookahead` frames, which come out
/// within the latency, and gain applied to silence is silence.
impl System for LimiterMember {
    fn latency(&self) -> Samples {
        Samples(self.lookahead)
    }

    fn tail(&self, _: Db) -> Ring {
        Ring::Finite(Seconds(0.0))
    }
}

/// The [`Family`] of the lookahead limiter, as wide as its input bus: one gain, linked
/// across every channel, from the loudest. There is no side-chain: it limits what it
/// delays.
///
/// The lookahead is a **construction constant**, not a control: it sizes the delay (and
/// is the latency the graph compensates), so it cannot move while the node runs.
#[derive(Clone, Copy, Debug)]
pub struct LimiterFamily {
    input: BusTopology,
    lookahead: Lookahead,
}

impl LimiterFamily {
    /// A limiter on the bus `input`, delaying by `lookahead` (at least one frame at any
    /// rate), and one output bus where it is.
    #[must_use]
    pub const fn new(input: BusTopology, lookahead: Lookahead) -> Self {
        Self { input, lookahead }
    }

    /// The lookahead window, as it was asked for: from its seconds every rate derives
    /// its own frame count (the count is a `ceil`, so it cannot be recovered).
    #[must_use]
    pub const fn lookahead(&self) -> Lookahead {
        self.lookahead
    }
}

impl Family for LimiterFamily {
    type Controls = LimiterControls;
    type Member = LimiterMember;
    type Recurrence = LimiterRecurrence;

    /// Solved on the stream's grid of 16 frames, like the filters. The coefficients land
    /// whole, so a control that moves inside a block reaches the render within 16 frames,
    /// wherever the blocks fall.
    const CADENCE: Cadence = COEFF_STREAM;

    fn channels(&self) -> Channels {
        Channels::same(self.input)
    }

    fn blank(&self, rate: SampleRate) -> LimiterMember {
        LimiterMember::of(&LimiterControls::DEFAULT, self.lookahead, rate)
    }

    /// The controls, made runnable: a NaN threshold or ceiling and a non-finite release
    /// are the default's, and the release is floored at 1 ms.
    fn member_into(&self, c: &LimiterControls, rate: SampleRate, out: &mut LimiterMember) {
        *out = LimiterMember::of(c, self.lookahead, rate);
    }

    fn recurrence(&self) -> LimiterRecurrence {
        LimiterRecurrence {
            lookahead: self.lookahead,
            channels: usize::from(self.input.layout().count()),
        }
    }
}

/// The coefficient point of a limiter: the curve and the figures the frame form runs on,
/// which all land whole.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LimiterTarget {
    /// The transfer curve at the member's own threshold and ceiling, for a block no lane
    /// drives.
    curve: LimiterCurve,
    /// What a frame nobody drives (or a non-finite one) is: the member's threshold.
    threshold: Db,
    /// And its ceiling.
    ceiling: Db,
    /// The one-pole coefficient the envelope releases at.
    release: f32,
}

/// A limiter's coefficients do not blend: its glide has no steps, so this is only ever
/// the target.
impl Lerp for LimiterTarget {
    fn lerp(&self, to: &Self, _: u32, _: u32) -> Self {
        *to
    }
}

/// What a block changes: the gain envelope, the delay and window minimum, and the peaks
/// (then gains) of the block being run.
#[derive(Clone, Debug)]
pub struct LimiterState {
    /// The envelope gain (linear): instant attack, one-pole release. `+inf` is **unset**
    /// (after a prepare or a reset): the first frame replaces it, through the attack
    /// branch, before any arithmetic reads it (a target is finite, so it is under `+inf`),
    /// and it never leaves the recurrence: the window minimum is pushed the replaced value,
    /// and the recurrence has no reading (`Readout`) to report it through.
    envelope: f32,
    ring: LookaheadRing,
    /// The linked peak of each frame of the block, which becomes its window-minimum gain;
    /// sized to the longest block in `prepare`.
    peaks: Vec<f32>,
}

/// The [`Recurrence`] of a [`LimiterFamily`].
///
/// K, per frame: the linked peak (the loudest channel, a NaN ignored), the target gain
/// (the member's curve, or, when a lane of the [`Feed`] drives the threshold or the
/// ceiling, the curve in dB at that frame's values), the envelope (taken at once when
/// lower, released toward the target by the member's coefficient otherwise) and the
/// minimum over the lookahead window, with which every channel's delayed sample is
/// multiplied.
///
/// **Lane 0 is the threshold and lane 1 the ceiling, in dB, per frame.** A frame that is
/// not finite falls back to the member's (a lane value of `+-inf` is not a level to
/// divide by: the curve in dB would make a NaN of it, which the envelope would keep). The delay is the lookahead, which the *member*
/// declares as its latency: the recurrence adds none of its own.
#[derive(Clone, Copy, Debug)]
pub struct LimiterRecurrence {
    lookahead: Lookahead,
    channels: usize,
}

impl LimiterRecurrence {
    /// The target gain of frame `i`, whose linked peak is `peak`.
    #[inline]
    fn target_gain(target: &LimiterTarget, feed: &Feed<'_>, i: usize, peak: f32) -> f32 {
        if feed.lanes[0].is_none() && feed.lanes[1].is_none() {
            return target.curve.gain(peak);
        }
        let read = |lane: Option<&[f32]>, base: Db| match lane {
            Some(lane) if lane[i].is_finite() => Db(lane[i]),
            _ => base,
        };
        let peak_db = amplitude_to_db(Amplitude::new(peak));
        let threshold = read(feed.lanes[0], target.threshold);
        let ceiling = read(feed.lanes[1], target.ceiling);
        compute_limiter_gain(peak_db, threshold, ceiling).get()
    }
}

impl Recurrence for LimiterRecurrence {
    type Member = LimiterMember;
    type Capacity = ();
    type Target = LimiterTarget;
    type Coeffs = Glide<LimiterTarget>;
    type State = LimiterState;

    const STEP: usize = 1;

    fn start(&self, member: &LimiterMember) -> Start<Self> {
        let mut target = LimiterTarget {
            curve: member.curve,
            threshold: member.threshold,
            ceiling: member.ceiling,
            release: 0.0,
        };
        self.coefficients_into(member, &mut target);
        Start {
            coeffs: Glide::landed(target).by(0),
            state: LimiterState {
                envelope: UNSET,
                ring: LookaheadRing::new(self.channels, member.lookahead),
                peaks: Vec::new(),
            },
            target,
        }
    }

    /// Sizes the delay from the lookahead at the rate, and the peaks from the longest
    /// block. The conversions saturate: a prepare must not panic.
    fn prepare(
        &mut self,
        _: &(),
        setup: &Setup,
        _: &mut Glide<LimiterTarget>,
        state: &mut LimiterState,
        _: &mut LimiterTarget,
    ) {
        let frames = lookahead_frames(self.lookahead, setup.rate);
        state.ring = LookaheadRing::new(self.channels, frames);
        state.peaks = vec![0.0; setup.max_block.get()];
    }

    fn coefficients_into(&self, m: &LimiterMember, target: &mut LimiterTarget) -> bool {
        *target = LimiterTarget {
            curve: m.curve,
            threshold: m.threshold,
            ceiling: m.ceiling,
            release: m.release.one_pole_coeff(m.rate),
        };
        true
    }

    fn clear(&self, state: &mut LimiterState) {
        state.envelope = UNSET;
        state.ring.clear();
    }

    fn run(
        &self,
        coeffs: &Glide<LimiterTarget>,
        state: &mut LimiterState,
        feed: &Feed<'_>,
        output: &mut [&mut [f32]],
        frames: Range<usize>,
        _: usize,
    ) {
        let target = coeffs.now();
        let LimiterState {
            envelope,
            ring,
            peaks,
        } = state;
        let peaks = &mut peaks[frames.clone()];

        // The linked peak: the loudest channel at each frame, taken a channel at a time
        // (`max` is exact, so the order does not matter).
        peaks.fill(0.0);
        for input in feed.audio {
            for (p, &x) in peaks.iter_mut().zip(&input[frames.clone()]) {
                *p = fold_peak(*p, x);
            }
        }

        // The recursive half: the envelope and the sliding minimum, one frame at a time;
        // each frame's peak becomes its window-minimum gain.
        for (p, i) in peaks.iter_mut().zip(frames.clone()) {
            let gain = Self::target_gain(&target, feed, i, *p);
            // The instant attack: an unset envelope (`+inf`) takes its first target here.
            if gain < *envelope {
                *envelope = gain;
            } else {
                *envelope = Smoother::toward(*envelope, gain, target.release);
            }
            *p = ring.push_gain(*envelope);
        }

        for (c, (out, input)) in output.iter_mut().zip(feed.audio).enumerate() {
            ring.delay_scaled(c, &input[frames.clone()], peaks, &mut out[frames.clone()]);
        }
    }

    fn effort(&self) -> Effort {
        Effort::Moderate
    }
}

#[cfg(test)]
mod tests;
