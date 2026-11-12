//! [`GateFamily`]: the noise gate as a [`Family`] (design 024 section 3, row 6).
//!
//! A gate reads a **key**: the side-chain as wide as the signal (an unwired one is
//! silence and holds the gate closed) or the signal itself, as the family's [`KeySource`] says.
//! It finds the key's level (the loudest channel at each frame), decides *open* when the
//! level is at or over the threshold, and runs a follower on that decision: an attack
//! toward 1 while open, a hold of whole frames after the level falls, then a release toward
//! 0. The signal is multiplied, on **every** channel, by one gain between the closed floor
//! (the `range`) and 1: `range + open * (1 - range)`. The gain is linked, so the stereo
//! image does not move when one side is louder.
//!
//! The threshold is the one control read **per frame** when the graph drives it (lane 0 of
//! the [`Feed`]; design 024 section 4.5, "Where cadence stops"): it is a level to compare
//! against, not a coefficient. Every other control lands on the stream grid
//! ([`Family::CADENCE`]), the range over [`GAIN_GLIDE`] (it multiplies the output
//! directly, so a step in it is a click) and the rest whole: the times only move what the
//! follower is smoothing toward, and the follower is the smoother.

use core::ops::Range;

use tutti_types::{Amplitude, BusTopology, Db, SampleRate, Samples, Seconds};

use super::compressor::GAIN_GLIDE;
use super::curves::{amplitude_to_db, gate_gain_at};
use super::key_source::KeySource;
use crate::control::COEFF_STREAM;
use crate::smoother::Smoother;
use crate::{
    Approach, Cadence, Channels, Effort, Family, Feed, Glide, Recurrence, Ring, Setup, Start,
    Steps, System,
};

/// The controls of a [`GateFamily`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GateControls {
    /// The key level, in dB, at or above which the gate opens.
    pub threshold: Db,
    /// How fast the gate opens: the time constant of a one-pole smoother toward 1. Below
    /// 0 reads as 0 (instant).
    pub attack: Seconds,
    /// How long the gate stays fully open after the key falls under the threshold: whole
    /// frames, `floor(hold * rate)`. Below 0 reads as 0. Hold is what stops a gate
    /// chattering on a key that hovers at the threshold.
    pub hold: Seconds,
    /// How fast the gate closes once the hold is over: the time constant of a one-pole
    /// smoother toward 0. Below 0 reads as 0.
    pub release: Seconds,
    /// The gain of a closed gate, in dB, at most 0: a floor, not a mute (`-80` is
    /// effectively silent, a gentler value ducks, `0` is no gate at all). Above 0 reads
    /// as 0.
    pub range: Db,
}

impl GateControls {
    /// A gate that passes everything: threshold at -40 dB, no times and a range of 0 dB.
    /// What a non-finite control falls back to.
    pub const DEFAULT: Self = Self {
        threshold: Db(-40.0),
        attack: Seconds(0.0),
        hold: Seconds(0.0),
        release: Seconds(0.0),
        range: Db(0.0),
    };
}

impl Default for GateControls {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// `x` if it is finite, else `default`.
fn finite_or(x: f32, default: f32) -> f32 {
    if x.is_finite() { x } else { default }
}

/// A gate at one set of controls and one rate: the comparison, the hold in frames and the
/// closed floor. Memoryless in the sense that matters to the graph: no latency (the
/// detector is on the key, never ahead of it) and no tail (the gain only scales, so a
/// silent signal is a silent output whatever the follower is doing).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GateMember {
    threshold: Db,
    attack: Seconds,
    hold: Seconds,
    release: Seconds,
    range: Db,
    rate: SampleRate,
    /// The threshold as a linear level: the detector compares in the linear domain, so no
    /// `log10` runs per frame.
    threshold_linear: f32,
    /// Whether a silent key (pinned at [`Db::FLOOR`]) is at or above the threshold. Only
    /// a threshold at or under the floor opens on silence.
    silence_opens: bool,
    /// `floor(hold * rate)`.
    hold_samples: usize,
}

impl GateMember {
    /// The member of `c` at `rate`, the controls made runnable.
    fn of(c: &GateControls, rate: SampleRate) -> Self {
        let d = GateControls::DEFAULT;
        let threshold = Db(finite_or(c.threshold.get(), d.threshold.get()));
        // A NaN time reads as 0 (`max` ignores it), instant; an infinite one holds the
        // follower (a coefficient of 0, as `Seconds::one_pole_coeff` says) or, for a
        // hold, saturates to the longest count.
        let hold = Seconds(c.hold.get().max(0.0));
        Self {
            threshold,
            attack: Seconds(c.attack.get().max(0.0)),
            hold,
            release: Seconds(c.release.get().max(0.0)),
            range: Db(finite_or(c.range.get(), d.range.get()).min(0.0)),
            rate,
            threshold_linear: threshold.to_amplitude().get(),
            silence_opens: Db::FLOOR >= threshold,
            hold_samples: hold.to_samples_floor(rate).get(),
        }
    }

    /// The threshold, in dB.
    #[must_use]
    pub fn threshold(&self) -> Db {
        self.threshold
    }

    /// The attack time constant, at least 0.
    #[must_use]
    pub fn attack(&self) -> Seconds {
        self.attack
    }

    /// The hold time, at least 0.
    #[must_use]
    pub fn hold(&self) -> Seconds {
        self.hold
    }

    /// The release time constant, at least 0.
    #[must_use]
    pub fn release(&self) -> Seconds {
        self.release
    }

    /// The range, in dB and at most 0: the closed gain.
    #[must_use]
    pub fn range(&self) -> Db {
        self.range
    }

    /// The hold in whole frames: `floor(hold * rate)`.
    #[must_use]
    pub fn hold_samples(&self) -> usize {
        self.hold_samples
    }

    /// Whether a silent key opens the gate: only a threshold at or under [`Db::FLOOR`]
    /// does, since silence is pinned at the floor for the comparison.
    #[must_use]
    pub fn silence_opens(&self) -> bool {
        self.silence_opens
    }

    /// Whether a key at `level` opens the gate: at or over the threshold. A level of
    /// silence is at [`Db::FLOOR`].
    #[must_use]
    pub fn opens(&self, level: Amplitude) -> bool {
        opens(level.get(), self.threshold_linear, self.silence_opens)
    }

    /// The gain of a fully closed gate: the range as an amplitude.
    #[must_use]
    pub fn closed_gain(&self) -> Amplitude {
        self.range.to_amplitude()
    }
}

/// The decision against the member's threshold, in the linear domain.
///
/// Exact equality is unreachable: `10^(t/20)` rounds, so no level a test can name sits on
/// the threshold, and `>=` versus `>` cannot be pinned by a test.
#[inline]
fn opens(level: f32, threshold_linear: f32, silence_opens: bool) -> bool {
    level >= threshold_linear || (level <= 0.0 && silence_opens)
}

impl System for GateMember {
    fn latency(&self) -> Samples {
        Samples::ZERO
    }

    fn tail(&self, _: Db) -> Ring {
        Ring::Finite(Seconds(0.0))
    }
}

/// The [`Family`] of the noise gate, `input` wide, its detector on the [`KeySource`] it is
/// given. One open/closed decision is made from the loudest key channel and one gain is
/// applied to every channel.
#[derive(Clone, Copy, Debug)]
pub struct GateFamily {
    input: BusTopology,
    key: KeySource,
}

impl GateFamily {
    /// A gate on the bus `input` whose detector reads `key`, and one output bus where the
    /// input is. A [`KeySource::Sidechain`] is a second bus as wide as the input.
    #[must_use]
    pub const fn new(input: BusTopology, key: KeySource) -> Self {
        Self { input, key }
    }
}

impl Family for GateFamily {
    type Controls = GateControls;
    type Member = GateMember;
    type Recurrence = GateRecurrence;

    /// Solved on the stream's grid of 16 frames, like the filters: a control that moves
    /// inside a block reaches the render within 16 frames, wherever the blocks fall.
    const CADENCE: Cadence = COEFF_STREAM;

    fn channels(&self) -> Channels {
        self.key.channels(self.input)
    }

    fn blank(&self, rate: SampleRate) -> GateMember {
        GateMember::of(&GateControls::DEFAULT, rate)
    }

    /// The controls, made runnable: a non-finite one is the default's, the times are
    /// floored at 0 and the range is capped at 0 dB.
    fn member_into(&self, c: &GateControls, rate: SampleRate, out: &mut GateMember) {
        *out = GateMember::of(c, rate);
    }

    fn recurrence(&self) -> GateRecurrence {
        GateRecurrence { key: self.key }
    }
}

/// The figures the detector runs on, which land whole: the comparison, the times as
/// one-pole coefficients at the rate, and the hold in frames.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GateDetector {
    threshold_linear: f32,
    silence_opens: bool,
    /// The one-pole coefficient while the gate opens.
    attack: f32,
    /// The one-pole coefficient while it closes.
    release: f32,
    hold_samples: usize,
}

/// The coefficient point of a gate: the [`GateDetector`], which lands whole, and the
/// range, which glides.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GateTarget {
    detector: GateDetector,
    range: Db,
}

/// The coefficients in flight: the detector's, landed, and a range gliding over
/// [`GAIN_GLIDE`].
///
/// A generic glide of the whole target would run for every frame of the glide when only
/// a time moved; the detector lands at once (the follower is its own smoother) and only
/// the range, which has nothing smoothing it, takes steps.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GateCoeffs {
    detector: GateDetector,
    /// The range in dB.
    range: Glide<f32>,
    /// The steps a range glide takes: [`GAIN_GLIDE`] at the prepared rate.
    steps: u32,
}

impl Approach for GateCoeffs {
    type Target = GateTarget;

    fn aim(&mut self, target: &GateTarget) {
        self.detector = target.detector;
        self.range
            .aim_by(target.range.get(), Steps::Fixed(self.steps));
    }

    #[inline]
    fn step(&mut self) -> bool {
        self.range.step()
    }

    fn settle(&mut self) {
        self.range.settle();
    }
}

/// What a block changes: the follower, the hold, and the gain of each frame of the block.
#[derive(Clone, Debug)]
pub struct GateState {
    /// The smoothed open fraction, 0 closed to 1 open.
    follower: f32,
    /// The frames of hold left.
    hold_counter: usize,
    /// The linked gain of each frame of the block being run, sized to the longest block
    /// in `prepare`.
    gains: Vec<f32>,
}

/// The [`Recurrence`] of a [`GateFamily`].
///
/// K, per frame: the key's level (the loudest channel of the [`KeySource`]'s source, the
/// side-chain or the signal; a NaN reads as quiet), the decision *open* (against the
/// member's linear threshold, or, when lane 0 of the [`Feed`] is finite at the frame,
/// against that threshold in dB: the level in dB, floored at [`Db::FLOOR`], at or over
/// it), the follower (the attack coefficient toward 1 and the hold counter reloaded while
/// open; the counter counting down while closed, the follower still; then the release
/// coefficient toward 0), and the gain `range + follower * (1 - range)` of the range as
/// an amplitude. The gain of every frame is written first, and the channels are then
/// multiplied by it, a multiply that vectorizes.
///
/// **Lane 0 is the threshold in dB, per frame.** A frame that is not finite falls back to
/// the member's threshold.
#[derive(Clone, Copy, Debug)]
pub struct GateRecurrence {
    key: KeySource,
}

impl Recurrence for GateRecurrence {
    type Member = GateMember;
    type Capacity = ();
    type Target = GateTarget;
    type Coeffs = GateCoeffs;
    type State = GateState;

    const STEP: usize = 1;

    fn start(&self, member: &GateMember) -> Start<Self> {
        let mut target = GateTarget {
            detector: GateDetector {
                threshold_linear: 0.0,
                silence_opens: false,
                attack: 0.0,
                release: 0.0,
                hold_samples: 0,
            },
            range: Db(0.0),
        };
        self.coefficients_into(member, &mut target);
        Start {
            coeffs: GateCoeffs {
                detector: target.detector,
                range: Glide::landed(target.range.get()),
                steps: 0,
            },
            state: GateState {
                follower: 0.0,
                hold_counter: 0,
                gains: Vec::new(),
            },
            target,
        }
    }

    fn prepare(
        &mut self,
        _: &(),
        setup: &Setup,
        coeffs: &mut GateCoeffs,
        state: &mut GateState,
        _: &mut GateTarget,
    ) {
        // 5 ms is a few hundred frames at any real rate; an absurd rate saturates
        // rather than panic, since a prepare must not.
        coeffs.steps =
            u32::try_from(GAIN_GLIDE.to_samples_ceil(setup.rate).get()).unwrap_or(u32::MAX);
        state.gains = vec![0.0; setup.max_block.get()];
    }

    fn coefficients_into(&self, m: &GateMember, target: &mut GateTarget) -> bool {
        target.detector = GateDetector {
            threshold_linear: m.threshold_linear,
            silence_opens: m.silence_opens,
            attack: m.attack.one_pole_coeff(m.rate),
            release: m.release.one_pole_coeff(m.rate),
            hold_samples: m.hold_samples,
        };
        target.range = m.range;
        true
    }

    fn clear(&self, state: &mut GateState) {
        state.follower = 0.0;
        state.hold_counter = 0;
    }

    fn run(
        &self,
        coeffs: &GateCoeffs,
        state: &mut GateState,
        feed: &Feed<'_>,
        output: &mut [&mut [f32]],
        frames: Range<usize>,
        _: usize,
    ) {
        let d = &coeffs.detector;
        let range = Db(coeffs.range.now()).to_amplitude().get();
        let detected = self.key.detected(feed);
        let gains = &mut state.gains[frames.clone()];
        for (g, i) in gains.iter_mut().zip(frames.clone()) {
            let level = detected.iter().fold(0.0f32, |m, ch| m.max(ch[i].abs()));
            let open = match feed.lanes[0]
                .map(|lane| Db(lane[i]))
                .filter(|t| t.get().is_finite())
            {
                Some(t) => amplitude_to_db(Amplitude::new(level)) >= t,
                None => opens(level, d.threshold_linear, d.silence_opens),
            };
            if open {
                state.hold_counter = d.hold_samples;
                state.follower = Smoother::toward(state.follower, 1.0, d.attack);
            } else if state.hold_counter > 0 {
                state.hold_counter -= 1;
            } else {
                state.follower = Smoother::toward(state.follower, 0.0, d.release);
            }
            *g = gate_gain_at(state.follower, range).get();
        }
        let gains = &state.gains[frames.clone()];
        for (out, input) in output.iter_mut().zip(feed.audio) {
            for ((o, &x), &g) in out[frames.clone()]
                .iter_mut()
                .zip(&input[frames.clone()])
                .zip(gains)
            {
                *o = x * g;
            }
        }
    }

    fn effort(&self) -> Effort {
        Effort::Moderate
    }
}

#[cfg(test)]
mod tests;
