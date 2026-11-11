//! [`LadderDesign`]: the Moog ladder as a [`Design`] (design 024 step 2b).
//!
//! The controls are [`LadderControls`], the model [`LadderModel`] (the
//! small-signal linearisation) and the realization [`LadderRealization`]: the four
//! stages with the `tanh` in the loop, whose coefficients and drive are read from
//! the model and narrowed once to the state precision `F`. Like the SVF it is
//! designed on a 16-frame grid counted on the stream clock from the last reset
//! ([`LadderDesign::CADENCE`]): the coefficients **and the drive** ramp linearly
//! over the 16 frames after each retarget and arrive exactly on it, so a control
//! is in the render at most 16 frames after it was sampled and the render does not
//! depend on how audio is split into blocks.

use core::marker::PhantomData;
use core::num::NonZeroU32;

use tutti_types::{ChannelLayout, Db, SampleRate};

use super::ladder::{LadderCoefficients, ladder_step};
use super::real::Real;
use super::{LadderControls, LadderModel, LadderType};
use crate::{
    COEFF_INTERVAL, Cadence, Ceiling, Channels, Design, Effect, Effort, Realization, Ring, Setup,
    Sizes, lane_group,
};

/// A ladder allocates nothing from its controls: its width is the design's.
#[derive(Clone, Copy, Debug)]
pub struct LadderSizes;

impl Sizes for LadderSizes {
    type Source = LadderModel;

    fn measure(_: &LadderModel, _: Db) -> Self {
        LadderSizes
    }

    /// A resonant ladder rings for as long as its resonance says; the node
    /// declares [`Tail::Unknown`](tutti_types::Tail::Unknown) and never parks.
    fn tail(&self) -> Ring {
        Ring::Unbounded
    }

    fn dominates(&self, _: &Self) -> bool {
        true
    }
}

/// The [`Design`] of the ladder filter, `channels` wide, with state precision `F`
/// (`f64` in the nodes, for accuracy at low cutoffs). The coefficients and the
/// drive are shared across the channels and only the four stage words are per
/// channel.
#[derive(Clone, Copy, Debug)]
pub struct LadderDesign<F> {
    channels: ChannelLayout,
    state: PhantomData<fn() -> F>,
}

impl<F> LadderDesign<F> {
    /// A design `channels` wide.
    #[must_use]
    pub fn new(channels: ChannelLayout) -> Self {
        Self {
            channels,
            state: PhantomData,
        }
    }
}

impl<F: Real> Design for LadderDesign<F> {
    type Controls = LadderControls;
    type Model = LadderModel;
    type Bounds = LadderSizes;
    type Range = ();
    type Realization = LadderRealization<F>;

    /// Solved every 16 frames on the stream clock (doc 024 Q27), at the first frame
    /// of each segment, and ramped to over it.
    const CADENCE: Cadence = Cadence::stream(match NonZeroU32::new(COEFF_INTERVAL as u32) {
        Some(n) => n,
        None => unreachable!(),
    });

    fn channels(&self) -> Channels {
        Channels {
            input: self.channels,
            output: self.channels,
        }
    }

    fn blank(&self, rate: SampleRate) -> LadderModel {
        LadderModel::new(LadderControls::DEFAULT, rate)
    }

    fn design_into(&self, controls: &LadderControls, rate: SampleRate, out: &mut LadderModel) {
        *out = LadderModel::new(*controls, rate);
    }

    fn bounds(&self, _: &Ceiling<()>) -> LadderSizes {
        LadderSizes
    }

    fn realization(&self) -> LadderRealization<F> {
        let rest = Point::of(&self.blank(SampleRate::DEFAULT));
        LadderRealization {
            ty: LadderType::default(),
            prev: rest,
            next: rest,
            stages: vec![[F::default(); 4]; usize::from(self.channels.count()).max(1)],
            phase: COEFF_INTERVAL,
            landed: false,
        }
    }
}

/// What the ramp interpolates: the coefficients and the drive.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Point<F: Real> {
    c: LadderCoefficients<F>,
    drive: F,
}

impl<F: Real> Point<F> {
    /// The set of `model`. The model's drive is an `f64` made of an `f32`, so
    /// narrowing it to `F` is exact.
    fn of(model: &LadderModel) -> Self {
        Self {
            c: LadderCoefficients::from(model.coeffs()),
            drive: F::from_f64(model.drive()),
        }
    }

    #[inline]
    fn lerp(&self, to: &Self, t: F) -> Self {
        Self {
            c: self.c.lerp(&to.c, t),
            drive: self.drive + (to.drive - self.drive) * t,
        }
    }
}

/// The realization of a [`LadderModel`]: the four stage words of each channel and
/// the coefficients and drive it runs, interpolated over 16 frames from where the
/// last solve left them to the model it was last retargeted to. It counts its own
/// position in that ramp, so a ramp cut by the end of a call goes on in the next.
#[derive(Clone, Debug)]
pub struct LadderRealization<F: Real> {
    /// The tap taken as the output.
    ty: LadderType,
    /// The set the ramp starts from.
    prev: Point<F>,
    /// The set the ramp ends at.
    next: Point<F>,
    /// `[s0, s1, s2, s3]`, one per channel; `len()` is the audio width. Built with
    /// the design and never resized (no allocation on the audio thread).
    stages: Vec<[F; 4]>,
    /// Frames of the ramp already rendered, [`COEFF_INTERVAL`] once it is done.
    phase: usize,
    /// Whether a model has been retargeted since [`prepare`](Realization::prepare):
    /// the first lands whole, with nothing to interpolate from.
    landed: bool,
}

impl<F: Real> LadderRealization<F> {
    /// The set `p` frames into the ramp: `next` from the segment's last frame on.
    #[inline]
    fn at(&self, p: usize) -> Point<F> {
        if p >= COEFF_INTERVAL {
            self.next
        } else {
            let t = F::from_f64(p as f64) / F::from_f64(COEFF_INTERVAL as f64);
            self.prev.lerp(&self.next, t)
        }
    }

    /// The channels `first..first + N` through the held set, sample-inner, the
    /// group's stage words in registers. One channel alone is latency-bound (every
    /// sample's `tanh` waits on the previous sample's fourth stage); running the
    /// independent chains of a group side by side is what fills that latency
    /// (measured: one channel at a time is 2x slower).
    #[inline]
    fn held_group<const N: usize>(
        &mut self,
        first: usize,
        input: &[&[f32]],
        output: &mut [&mut [f32]],
    ) {
        let (Point { c, drive }, ty) = (self.next, self.ty);
        let xs: [&[f32]; N] = core::array::from_fn(|k| input[first + k]);
        let mut group = output.iter_mut().skip(first);
        let ys: [&mut [f32]; N] =
            core::array::from_fn(|_| &mut **group.next().expect("the group is inside the width"));
        let mut st: [[F; 4]; N] = core::array::from_fn(|k| self.stages[first + k]);
        for i in 0..xs[0].len() {
            for k in 0..N {
                ys[k][i] = ladder_step(&c, ty, &mut st[k], F::from_f32(xs[k][i]), drive).to_f32();
            }
        }
        self.stages[first..first + N].copy_from_slice(&st);
    }
}

impl<F: Real> Realization<LadderModel> for LadderRealization<F> {
    type Bounds = LadderSizes;

    fn prepare(&mut self, _: &LadderSizes, _: &Setup) {
        self.landed = false;
    }

    /// Starts a ramp to `model` from the set in force now (a ramp cut short is not
    /// jumped over); the first after `prepare` lands whole.
    fn retarget(&mut self, model: &LadderModel) {
        let next = Point::of(model);
        self.ty = model.ladder_type();
        if self.landed {
            self.prev = self.at(self.phase);
            self.phase = 0;
        } else {
            self.prev = next;
            self.phase = COEFF_INTERVAL;
            self.landed = true;
        }
        self.next = next;
    }

    fn reset(&mut self) {
        self.stages.fill([F::default(); 4]);
        self.prev = self.next;
        self.phase = COEFF_INTERVAL;
    }

    fn effort(&self) -> Effort {
        Effort::Moderate
    }

    fn fresh(&self) -> Self {
        let mut fresh = self.clone();
        fresh.reset();
        fresh
    }
}

impl<F: Real> Effect<LadderModel> for LadderRealization<F> {
    /// Renders with the set held when nothing moves, else with the set blended
    /// linearly over the 16 frames after the last retarget, arriving **exactly** on
    /// the new one at the ramp's last frame, wherever the calls cut it.
    fn process(&mut self, input: &[&[f32]], output: &mut [&mut [f32]]) {
        let width = self.stages.len();
        if self.prev == self.next {
            let mut first = 0;
            while first < width {
                let n = lane_group(width - first);
                match n {
                    8 => self.held_group::<8>(first, input, output),
                    7 => self.held_group::<7>(first, input, output),
                    6 => self.held_group::<6>(first, input, output),
                    5 => self.held_group::<5>(first, input, output),
                    4 => self.held_group::<4>(first, input, output),
                    3 => self.held_group::<3>(first, input, output),
                    2 => self.held_group::<2>(first, input, output),
                    _ => self.held_group::<1>(first, input, output),
                }
                first += n;
            }
        } else {
            let (len, ty) = (input[0].len(), self.ty);
            for i in 0..len {
                let Point { c, drive } = self.at(self.phase + i + 1);
                for (ch, s) in self.stages.iter_mut().enumerate() {
                    output[ch][i] =
                        ladder_step(&c, ty, s, F::from_f32(input[ch][i]), drive).to_f32();
                }
            }
            self.phase = (self.phase + len).min(COEFF_INTERVAL);
        }
        if self.phase == COEFF_INTERVAL {
            self.prev = self.next;
        }
    }
}

#[cfg(test)]
mod tests {
    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};
    use tutti_types::{Drive, Hz, Resonance};

    use super::*;
    use crate::contract::{Anchor, DesignCase, LtiCase, RealizationCase};

    const TYPES: [LadderType; 4] = [
        LadderType::LP12,
        LadderType::LP24,
        LadderType::HP12,
        LadderType::HP24,
    ];

    fn controls(ty: LadderType, freq: f32, res: f32, drive: f32) -> LadderControls {
        LadderControls {
            ty,
            freq: Hz(freq),
            res: Resonance(res),
            drive: Drive(drive),
        }
    }

    fn design() -> LadderDesign<f64> {
        LadderDesign::new(ChannelLayout::MONO)
    }

    // The reachable controls stay where the small-signal model is stable: the
    // linearisation of the trapezoidal loop has a pole outside the unit circle at a
    // high cutoff with a middling resonance (15 kHz at 44.1 kHz, resonance 0.5: radius
    // 1.19; 6 kHz at resonance 0.7: 1.009), and past a resonance of about 1 everywhere, where the saturator is what
    // holds the real filter (`ladder_model.rs`).
    impl DesignCase for LadderDesign<f64> {
        fn reachable(&self) -> Vec<LadderControls> {
            let mut all = Vec::new();
            for ty in TYPES {
                for freq in [30.0, 400.0, 1_500.0, 3_000.0] {
                    for res in [0.0, 0.3, 0.5, 0.7] {
                        for drive in [0.1, 1.0, 6.0] {
                            all.push(controls(ty, freq, res, drive));
                        }
                    }
                }
            }
            let mut rng = StdRng::seed_from_u64(24);
            all.extend((0..200).map(|_| {
                controls(
                    TYPES[rng.gen_range(0..TYPES.len())],
                    rng.gen_range(20.0..3_000.0),
                    rng.gen_range(0.0..0.7),
                    rng.gen_range(0.1..8.0),
                )
            }));
            all
        }

        fn hostile(&self) -> Vec<LadderControls> {
            let nasty = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -3.0, 1e30];
            // The model is held to stability, and a clamp can land on what it calls
            // unstable: a resonance past 1 clamps to 1 (the saturator is what holds
            // the real filter), a cutoff past Nyquist clamps to it, where any
            // resonance is. A cutoff clamped to the 1 Hz floor puts the four poles
            // within 1e-4 of 1, closer than the root finder places them, so it is not
            // tried; the cutoff that clamps to
            // Nyquist is tried with no resonance.
            let mut all: Vec<_> = nasty
                .iter()
                .map(|&v| controls(LadderType::LP24, 1_000.0, 0.5, v))
                .collect();
            all.extend(
                [f32::NAN, f32::INFINITY, f32::NEG_INFINITY]
                    .map(|v| controls(LadderType::HP12, v, 0.2, 1.0)),
            );
            all.push(controls(LadderType::HP12, 1e30, 0.0, 1.0));
            all.extend(
                [f32::NAN, f32::NEG_INFINITY, -3.0]
                    .iter()
                    .map(|&v| controls(LadderType::LP24, 1_000.0, v, 1.0)),
            );
            all
        }
    }

    /// The closed forms of the linearisation (`ladder_model.rs`): DC is
    /// `drive / (1 + 4 res)` for a low-pass and 0 for a high-pass; the stages are
    /// `1/sqrt(2)` at the cutoff, so with no resonance a 12 dB type is 1/2 there and
    /// a 24 dB type 1/4.
    impl LtiCase for LadderDesign<f64> {
        fn anchors(&self) -> Vec<Anchor<LadderControls>> {
            let rate = SampleRate(48_000.0);
            let at = |c: LadderControls, freq: f32, gain: f64| Anchor {
                controls: c,
                rate,
                output: 0,
                freq: Hz(freq),
                gain,
                tolerance: 1e-9,
            };
            let mut all = Vec::new();
            for res in [0.0f32, 0.5, 0.9] {
                let k = 4.0 * f64::from(res);
                all.push(at(
                    controls(LadderType::LP24, 1_000.0, res, 1.0),
                    0.0,
                    1.0 / (1.0 + k),
                ));
                all.push(at(
                    controls(LadderType::LP12, 1_000.0, res, 2.0),
                    0.0,
                    2.0 / (1.0 + k),
                ));
                all.push(at(controls(LadderType::HP12, 1_000.0, res, 1.0), 0.0, 0.0));
            }
            for (ty, want) in [
                (LadderType::LP12, 0.5),
                (LadderType::LP24, 0.25),
                (LadderType::HP12, 0.5),
                (LadderType::HP24, 0.25),
            ] {
                all.push(at(controls(ty, 1_000.0, 0.0, 1.0), 1_000.0, want));
            }
            all
        }
    }

    impl RealizationCase for LadderDesign<f64> {
        fn probes(&self) -> Vec<LadderControls> {
            vec![
                controls(LadderType::LP24, 1_000.0, 0.3, 1.0),
                controls(LadderType::LP12, 3_000.0, 0.6, 1.0),
                controls(LadderType::HP24, 500.0, 0.2, 1.0),
                controls(LadderType::HP12, 6_000.0, 0.5, 1.0),
            ]
        }

        fn extremes(&self) -> Vec<LadderControls> {
            vec![
                controls(LadderType::LP24, 200.0, 0.9, 1.0),
                controls(LadderType::HP24, 8_000.0, 0.9, 1.0),
            ]
        }

        fn steps(&self) -> Vec<(LadderControls, LadderControls)> {
            vec![
                (
                    controls(LadderType::LP24, 800.0, 0.3, 1.0),
                    controls(LadderType::LP24, 1_200.0, 0.3, 1.0),
                ),
                (
                    controls(LadderType::LP24, 1_000.0, 0.4, 1.0),
                    controls(LadderType::LP24, 1_000.0, 0.4, 3.0),
                ),
            ]
        }
    }

    crate::design_contract!(design(), Lti);

    // No `Lti` group on the realization: it is the nonlinear ladder (the `tanh` in
    // the loop), and that group drives it at unit level and asks for linearity
    // (additivity, the impulse response against the model's, a decay measured
    // there), which the small-signal model does not claim at that level; the
    // model's own response is held against `ladder_step` at small signal in
    // `ladder_model.rs`. Solved on the stream's 16-frame grid, so the render does not
    // depend on how audio is split: `BlockIndependent`.
    crate::realization_contract!(design(), BlockIndependent);

    /// A blended call is the linear blend of the two sets, coefficients and drive
    /// alike, arriving **exactly** on the new set at its last frame: an impulse at
    /// frame `i` into a ladder with no state comes out as one `ladder_step` at the
    /// set `(i + 1) / 16` of the way from the old to the new, and at frame 15 as the
    /// held render of the new set gives it, bit for bit.
    ///
    /// Mutations (run): the blend at `i / 16` or `(i + 2) / 16` fails the frame-7
    /// row; leaving the drive out of the blend (`drive: to.drive` in `Point::lerp`)
    /// fails it too; `retarget` taking `prev = next` over a ramp cut short fails
    /// `the_cut_ramp_goes_on_from_where_it_was`.
    #[test]
    fn a_blend_is_linear_and_arrives_on_the_new_set() {
        let d = design();
        let rate = SampleRate(48_000.0);
        let model = |hz, drive| d.design(&controls(LadderType::HP12, hz, 0.3, drive), rate);
        let (old, new) = (model(500.0, 1.0), model(5_000.0, 4.0));
        for i in [7usize, 15] {
            let mut blended = d.realization();
            blended.retarget(&old);
            blended.retarget(&new);
            let mut x = [0.0f32; 16];
            x[i] = 0.5;
            let mut y = [0.0f32; 16];
            blended.process(&[&x], &mut [&mut y]);

            let t = (i + 1) as f64 / 16.0;
            let (a, b) = (Point::<f64>::of(&old), Point::<f64>::of(&new));
            let at = a.lerp(&b, t);
            // The drive is written out: 1 to 4 linearly.
            let drive = 1.0 + (4.0 - 1.0) * t;
            let want = ladder_step(&at.c, LadderType::HP12, &mut [0.0; 4], 0.5, drive);
            assert_eq!(y[i], want as f32, "frame {i}");
        }
        let mut held = d.realization();
        held.retarget(&new);
        let mut x = [0.0f32; 16];
        x[15] = 0.5;
        let (mut y, mut z) = ([0.0f32; 16], [0.0f32; 16]);
        let mut blended = d.realization();
        blended.retarget(&old);
        blended.retarget(&new);
        blended.process(&[&x], &mut [&mut y]);
        held.process(&[&x], &mut [&mut z]);
        assert_eq!(y[15], z[15]);
        assert_ne!(y[15], 0.0);
    }

    /// A retarget in the middle of a ramp starts from the set in force there, not
    /// from the set the ramp was heading for: 8 frames into a ramp to a cutoff, a
    /// retarget back leaves the first frame after it where the ramp was, one 16th of
    /// the way from there.
    ///
    /// Mutation (run): `prev = self.next` in `retarget` (a jump over the cut ramp)
    /// fails.
    #[test]
    fn the_cut_ramp_goes_on_from_where_it_was() {
        let d = design();
        let rate = SampleRate(48_000.0);
        let model = |hz| d.design(&controls(LadderType::HP12, hz, 0.3, 1.0), rate);
        let (a, b) = (model(500.0), model(5_000.0));
        let mut r = d.realization();
        r.retarget(&a);
        r.retarget(&b);
        r.process(&[&[0.0; 8]], &mut [&mut [0.0; 8]]);
        r.retarget(&a);
        let mut x = [0.0f32; 1];
        x[0] = 0.5;
        let mut y = [0.0f32; 1];
        r.process(&[&x], &mut [&mut y]);

        let (pa, pb) = (Point::<f64>::of(&a), Point::<f64>::of(&b));
        let from = pa.lerp(&pb, 8.0 / 16.0);
        let at = from.lerp(&pa, 1.0 / 16.0);
        let want = ladder_step(&at.c, LadderType::HP12, &mut [0.0; 4], 0.5, at.drive);
        assert_eq!(y[0], want as f32);
    }
}
