//! [`CompressorFamily`]: the feed-forward compressor as a [`Family`].
//!
//! A compressor reads a **key**: the side-chain as wide as the signal (an unwired one is
//! silence and compresses nothing) or the signal itself, as the family's [`KeySource`] says. It
//! finds its level (the loudest key channel at
//! each frame), turns the level over the threshold into a reduction by the static
//! curve, smooths that reduction with an attack and a release, and multiplies **every**
//! channel by one gain: the reduction's, plus the makeup. The gain is linked, so the
//! stereo image does not move when one side is louder.
//!
//! The threshold is the one control read **per frame** when the graph drives it (lane 0
//! of the [`Feed`]): it is a level to
//! compare against, not a coefficient. Every other control lands on the stream grid
//! ([`Family::CADENCE`]), the detector's whole and the makeup over [`GAIN_GLIDE`]: a
//! makeup multiplies the output directly, so a step in it is a click; the knee, the
//! ratio and the times only move what the follower is smoothing toward, and the follower
//! is the smoother.

use core::ops::Range;

use tutti_types::{Amplitude, BusTopology, CompressionRatio, Db, SampleRate, Samples, Seconds};

use super::curves::{amplitude_to_db, compute_compressor_gain_reduction};
use super::key_source::KeySource;
use crate::control::COEFF_STREAM;
use crate::smoother::Smoother;
use crate::{
    Approach, Cadence, Channels, Effort, Family, Feed, Glide, Recurrence, Ring, Setup, Start,
    Steps, System,
};

/// How long a makeup that moves takes to arrive: 5 ms, counted on the stream, so the
/// de-zipper does not depend on the block (a block ramp was 1.3 to 10.7 ms at 48 kHz,
/// by the host's block).
pub const GAIN_GLIDE: Seconds = Seconds(0.005);

/// The controls of a [`CompressorFamily`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompressorControls {
    /// The level above which reduction begins, in dB.
    pub threshold: Db,
    /// The width of the soft knee in dB, centred on the threshold; 0 is a hard knee.
    /// Below 0 reads as 0.
    pub knee: Db,
    /// dB in per dB out above the threshold. Below 1 reads as 1: a compressor does not
    /// expand.
    pub ratio: CompressionRatio,
    /// How fast the reduction rises toward a louder level: the time constant of a
    /// one-pole smoother. Below 0 reads as 0 (instant).
    pub attack: Seconds,
    /// How fast the reduction falls once the level drops. Below 0 reads as 0.
    pub release: Seconds,
    /// Gain added after the reduction, in dB.
    pub makeup: Db,
}

impl CompressorControls {
    /// A compressor that does nothing: threshold at 0 dB, a hard knee, no ratio, no
    /// times and no makeup. What a non-finite control falls back to.
    pub const DEFAULT: Self = Self {
        threshold: Db(0.0),
        knee: Db(0.0),
        ratio: CompressionRatio::UNITY,
        attack: Seconds(0.0),
        release: Seconds(0.0),
        makeup: Db(0.0),
    };
}

impl Default for CompressorControls {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// `x` if it is finite, else `default`.
fn finite_or(x: f32, default: f32) -> f32 {
    if x.is_finite() { x } else { default }
}

/// A compressor at one set of controls and one rate: the static curve, and the figures
/// its smoother runs on. Memoryless in the sense that matters to the graph: no latency
/// (the detector is on the signal and the key, never ahead of them) and no tail (the
/// gain only scales, so a silent signal is a silent output whatever the follower is
/// doing).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompressorMember {
    threshold: Db,
    knee: Db,
    ratio: CompressionRatio,
    attack: Seconds,
    release: Seconds,
    makeup: Db,
    rate: SampleRate,
}

impl CompressorMember {
    /// The threshold, in dB.
    #[must_use]
    pub fn threshold(&self) -> Db {
        self.threshold
    }

    /// The knee width, in dB, at least 0.
    #[must_use]
    pub fn knee(&self) -> Db {
        self.knee
    }

    /// The ratio, at least 1.
    #[must_use]
    pub fn ratio(&self) -> CompressionRatio {
        self.ratio
    }

    /// The attack time constant, at least 0.
    #[must_use]
    pub fn attack(&self) -> Seconds {
        self.attack
    }

    /// The release time constant, at least 0.
    #[must_use]
    pub fn release(&self) -> Seconds {
        self.release
    }

    /// The makeup gain, in dB.
    #[must_use]
    pub fn makeup(&self) -> Db {
        self.makeup
    }

    /// The reduction, in dB and positive, that a steady key at `level` asks for: the
    /// static curve (Giannoulis, Massberg and Reiss, 2012). Nothing below
    /// `threshold - knee/2`, `(1 - 1/ratio) (level - threshold)` above
    /// `threshold + knee/2`, and the quadratic that joins them inside the knee.
    #[must_use]
    pub fn reduction_db(&self, level: Db) -> Db {
        compute_compressor_gain_reduction(level, self.threshold, self.ratio, self.knee)
    }
}

impl CompressorMember {
    /// The member for `c` at `rate`: a non-finite control is the default's, a knee and
    /// the times are floored at 0 and a ratio at 1.
    pub(crate) fn designed(c: &CompressorControls, rate: SampleRate) -> Self {
        let d = CompressorControls::DEFAULT;
        // A NaN time reads as 0 (`max` ignores it), instant; an infinite one holds the
        // follower (a coefficient of 0), as `Seconds::one_pole_coeff` says.
        Self {
            threshold: Db(finite_or(c.threshold.get(), d.threshold.get())),
            knee: Db(finite_or(c.knee.get(), d.knee.get()).max(0.0)),
            ratio: CompressionRatio::new_clamped(finite_or(c.ratio.get(), d.ratio.get())),
            attack: Seconds(c.attack.get().max(0.0)),
            release: Seconds(c.release.get().max(0.0)),
            makeup: Db(finite_or(c.makeup.get(), d.makeup.get())),
            rate,
        }
    }
}

impl System for CompressorMember {
    fn latency(&self) -> Samples {
        Samples::ZERO
    }

    fn tail(&self, _: Db) -> Ring {
        Ring::Finite(Seconds(0.0))
    }
}

/// The [`Family`] of the compressor, `input` wide, its detector on the [`KeySource`] it is
/// given. One gain is computed from the loudest key channel and applied to every channel.
#[derive(Clone, Copy, Debug)]
pub struct CompressorFamily {
    input: BusTopology,
    key: KeySource,
}

impl CompressorFamily {
    /// A compressor on the bus `input` whose detector reads `key`, and one output bus
    /// where the input is. A [`KeySource::Sidechain`] is a second bus as wide as the input.
    #[must_use]
    pub const fn new(input: BusTopology, key: KeySource) -> Self {
        Self { input, key }
    }
}

impl Family for CompressorFamily {
    type Controls = CompressorControls;
    type Member = CompressorMember;
    type Recurrence = CompressorRecurrence;

    /// Solved on the stream's grid of 16 frames, like the filters: a control that
    /// moves inside a block reaches the render within 16 frames, wherever the blocks
    /// fall.
    const CADENCE: Cadence = COEFF_STREAM;

    fn channels(&self) -> Channels {
        self.key.channels(self.input)
    }

    fn blank(&self, rate: SampleRate) -> CompressorMember {
        let d = CompressorControls::DEFAULT;
        CompressorMember {
            threshold: d.threshold,
            knee: d.knee,
            ratio: d.ratio,
            attack: d.attack,
            release: d.release,
            makeup: d.makeup,
            rate,
        }
    }

    /// The controls, made runnable: a non-finite one is the default's, a knee and the
    /// times are floored at 0 and a ratio at 1.
    fn member_into(&self, c: &CompressorControls, rate: SampleRate, out: &mut CompressorMember) {
        *out = CompressorMember::designed(c, rate);
    }

    fn recurrence(&self, _: SampleRate) -> CompressorRecurrence {
        CompressorRecurrence { key: self.key }
    }
}

/// The figures the detector runs on: the controls it follows, with the times as
/// one-pole coefficients at the rate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Detector {
    threshold: Db,
    knee: Db,
    ratio: CompressionRatio,
    /// The one-pole coefficient while the reduction rises.
    attack: f32,
    /// The one-pole coefficient while the reduction falls.
    release: f32,
}

impl Detector {
    /// The figures member `m` asks for: its controls, and the times as coefficients at
    /// its rate.
    pub(crate) fn of(m: &CompressorMember) -> Self {
        Self {
            threshold: m.threshold,
            knee: m.knee,
            ratio: m.ratio,
            attack: m.attack.one_pole_coeff(m.rate),
            release: m.release.one_pole_coeff(m.rate),
        }
    }

    /// The figures of a compressor that does nothing, for a target to start from.
    pub(crate) const NEUTRAL: Self = Self {
        threshold: Db(0.0),
        knee: Db(0.0),
        ratio: CompressionRatio::UNITY,
        attack: 0.0,
        release: 0.0,
    };

    /// The threshold, in dB.
    pub(crate) fn threshold(&self) -> Db {
        self.threshold
    }

    /// One frame of the compressor: the key's `level` (an amplitude) in dB, the static
    /// curve at `threshold`, the one-pole follower `reduction` on it (the attack
    /// coefficient while it rises, the release while it falls) and the linear gain
    /// `-reduction + makeup_db`. The one copy of the compressor's frame, run by
    /// [`CompressorRecurrence`] and by each band of the multiband.
    #[inline]
    pub(crate) fn frame(
        &self,
        reduction: &mut f32,
        level: f32,
        threshold: Db,
        makeup_db: f32,
    ) -> f32 {
        let input_db = amplitude_to_db(Amplitude::new(level));
        let target = compute_compressor_gain_reduction(input_db, threshold, self.ratio, self.knee);
        let coeff = if target.get() > *reduction {
            self.attack
        } else {
            self.release
        };
        *reduction = Smoother::toward(*reduction, target.get(), coeff);
        Db::new(-*reduction + makeup_db).to_amplitude().get()
    }
}

/// The coefficient point of a compressor: the [`Detector`], which lands whole, and the
/// makeup, which glides.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompressorTarget {
    detector: Detector,
    makeup: Db,
}

/// The coefficients in flight: the detector's, landed, and a makeup gliding over
/// [`GAIN_GLIDE`].
///
/// A generic glide of the whole target would run for every frame of the glide when
/// only the ratio moved; the detector lands at once (the follower is its own smoother)
/// and only the makeup, which has nothing smoothing it, takes steps.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompressorCoeffs {
    detector: Detector,
    /// The makeup in dB.
    makeup: Glide<f32>,
    /// The steps a makeup glide takes: [`GAIN_GLIDE`] at the prepared rate.
    steps: u32,
}

impl Approach for CompressorCoeffs {
    type Target = CompressorTarget;

    fn aim(&mut self, target: &CompressorTarget) {
        self.detector = target.detector;
        self.makeup
            .aim_by(target.makeup.get(), Steps::Fixed(self.steps));
    }

    #[inline]
    fn step(&mut self) -> bool {
        self.makeup.step()
    }

    fn settle(&mut self) {
        self.makeup.settle();
    }
}

/// What a block changes: the follower, and the gain of each frame of the block.
#[derive(Clone, Debug)]
pub struct CompressorState {
    /// The smoothed reduction, in dB and positive.
    reduction: f32,
    /// The linked gain of each frame of the block being run, sized to the longest
    /// block in `prepare`.
    gains: Vec<f32>,
}

/// The [`Recurrence`] of a [`CompressorFamily`].
///
/// K, per frame: the key's level (the loudest channel of the [`KeySource`]'s source, the
/// side-chain or the signal; a NaN reads as quiet) in dB
/// (floored at [`Db::FLOOR`]), the static curve at the threshold (lane 0 of the
/// [`Feed`] when it is finite, else the member's), the one-pole follower on the
/// reduction (the attack coefficient while it rises, the release while it falls), and
/// the gain `-reduction + makeup` as an amplitude. The gain of every frame is written
/// first, and the channels are then multiplied by it, a multiply that vectorizes.
///
/// **Lane 0 is the threshold in dB, per frame.** A frame that is not finite falls back
/// to the member's threshold.
#[derive(Clone, Copy, Debug)]
pub struct CompressorRecurrence {
    key: KeySource,
}

impl Recurrence for CompressorRecurrence {
    type Member = CompressorMember;
    type Capacity = ();
    type Target = CompressorTarget;
    type Coeffs = CompressorCoeffs;
    type State = CompressorState;

    const STEP: usize = 1;

    fn start(&self, member: &CompressorMember) -> Start<Self> {
        let mut target = CompressorTarget {
            detector: Detector::NEUTRAL,
            makeup: Db(0.0),
        };
        self.coefficients_into(member, &mut target);
        Start {
            coeffs: CompressorCoeffs {
                detector: target.detector,
                makeup: Glide::landed(target.makeup.get()),
                steps: 0,
            },
            state: CompressorState {
                reduction: 0.0,
                gains: Vec::new(),
            },
            target,
        }
    }

    fn prepare(
        &mut self,
        _: &(),
        setup: &Setup,
        coeffs: &mut CompressorCoeffs,
        state: &mut CompressorState,
        _: &mut CompressorTarget,
    ) {
        // 5 ms is a few hundred frames at any real rate; an absurd rate saturates
        // rather than panic, since a prepare must not.
        coeffs.steps =
            u32::try_from(GAIN_GLIDE.to_samples_ceil(setup.rate).get()).unwrap_or(u32::MAX);
        state.gains = vec![0.0; setup.max_block.get()];
    }

    fn coefficients_into(&self, m: &CompressorMember, target: &mut CompressorTarget) -> bool {
        target.detector = Detector::of(m);
        target.makeup = m.makeup;
        true
    }

    fn clear(&self, state: &mut CompressorState) {
        state.reduction = 0.0;
    }

    fn run(
        &self,
        coeffs: &CompressorCoeffs,
        state: &mut CompressorState,
        feed: &Feed<'_>,
        output: &mut [&mut [f32]],
        frames: Range<usize>,
        _: usize,
    ) {
        let d = &coeffs.detector;
        let makeup = coeffs.makeup.now();
        let detected = self.key.detected(feed);
        let gains = &mut state.gains[frames.clone()];
        for (g, i) in gains.iter_mut().zip(frames.clone()) {
            let level = detected.iter().fold(0.0f32, |m, ch| m.max(ch[i].abs()));
            let threshold = feed.lanes[0]
                .map(|lane| Db(lane[i]))
                .filter(|t| t.get().is_finite())
                .unwrap_or(d.threshold);
            *g = d.frame(&mut state.reduction, level, threshold, makeup);
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
