//! [`Filter`]: the filters' one [`Family`], over a [`ChannelRecurrence`], and
//! [`PerChannel`], the one [`Recurrence`] that runs such a recurrence's channels each alone
//! (design 024 section 4.3a).
//!
//! The SVF, the ladder and the crossover differ in their coefficients, their few
//! words of state per channel and the step that turns one sample into the next; a
//! channel never reads another. That is a [`ChannelRecurrence`]. The glide of its
//! coefficients, the held path that runs up to eight channels side by side and the
//! first-retarget landing are written once, here and in [`Realized`](crate::Realized).

use core::ops::Range;

use tutti_types::{BusTopology, ChannelLayout, SampleRate};

use crate::control::{COEFF_STREAM, lane_group};
use crate::{Cadence, Channels, Effort, Family, Glide, LANES, Lerp, Recurrence, Start, System};

/// The most outputs one input channel makes: a crossover's bands.
const MAX_OUTPUTS: usize = 6;

/// A filter whose channels run alone: per-channel `Copy` state, one step from a sample
/// to the next, and coefficients shared by every channel.
pub trait ChannelRecurrence: Clone + core::fmt::Debug + Send + 'static {
    /// The plain values of the filter's controls.
    type Controls: Copy + PartialEq + Send;
    /// The system the controls describe: a member of the family.
    type Member: System + Copy + core::fmt::Debug + Send;
    /// What the step reads: the model's own coefficients in the state's precision.
    /// A glide blends two of them ([`Lerp`]).
    type Coeffs: Lerp + Send;
    /// One channel's words.
    type State: Copy + Default + core::fmt::Debug + Send;
    /// What one sample makes: `[y]`, or a crossover's bands.
    type Frame: AsRef<[f32]>;

    /// The steps a glide to a new model takes: 16 frames for the SVF and the ladder, 0
    /// for the crossover, which lands whole (a blend of a Linkwitz-Riley split's
    /// biquads is not one, so its bands would stop summing to an all-pass inside the
    /// glide; doc 024 section 4.5).
    const STEPS: u32;

    /// How many outputs each input channel makes, at most `MAX_OUTPUTS` (6): the bands
    /// of [`Channels::bands`], output `o` of every channel in band `o`.
    fn outputs(&self) -> usize {
        1
    }

    /// An empty member at `rate`: what [`member_into`](Self::member_into) writes into.
    fn blank(&self, rate: SampleRate) -> Self::Member;

    /// Writes the member for `controls` at `rate` into `out` ([`Family::member_into`]).
    fn member_into(&self, controls: &Self::Controls, rate: SampleRate, out: &mut Self::Member);

    /// The coefficients the step reads, from `model`.
    fn coeffs(&self, model: &Self::Member) -> Self::Coeffs;

    /// One sample `x` through one channel's `state` at coefficients `c`.
    fn tick(c: &Self::Coeffs, state: &mut Self::State, x: f32) -> Self::Frame;
}

/// The [`Family`] of a filter over a [`ChannelRecurrence`], on one input bus.
///
/// Solved every 16 frames on the stream clock, at the first frame of each segment
/// (doc 024 Q27), so the render is a function of the stream and not of how it is cut
/// into blocks. The member is only `Member::new(controls)`; a family's own checks stay
/// in its constructor.
#[derive(Clone, Copy, Debug)]
pub struct Filter<R> {
    recurrence: R,
    input: BusTopology,
}

impl<R> Filter<R> {
    pub(crate) fn over(recurrence: R, input: BusTopology) -> Self {
        Self { recurrence, input }
    }

    /// The recurrence.
    #[must_use]
    pub fn recurrence(&self) -> &R {
        &self.recurrence
    }

    /// The input width; each output is this wide.
    #[must_use]
    pub fn layout(&self) -> ChannelLayout {
        self.input.layout()
    }
}

impl<R: ChannelRecurrence> Family for Filter<R> {
    type Controls = R::Controls;
    type Member = R::Member;
    type Recurrence = PerChannel<R>;

    const CADENCE: Cadence = COEFF_STREAM;

    fn channels(&self) -> Channels {
        // One output per input sits where the input does; several are a
        // crossover's bands, each where the input is.
        Channels::bands(self.input, self.recurrence.outputs() as u8)
            .expect("a recurrence makes at least one output")
    }

    fn blank(&self, rate: SampleRate) -> R::Member {
        self.recurrence.blank(rate)
    }

    fn member_into(&self, controls: &R::Controls, rate: SampleRate, out: &mut R::Member) {
        self.recurrence.member_into(controls, rate, out);
    }

    fn recurrence(&self) -> PerChannel<R> {
        let width = usize::from(self.layout().count()).max(1);
        PerChannel {
            recurrence: self.recurrence.clone(),
            width,
        }
    }
}

/// The [`Recurrence`] of a [`ChannelRecurrence`]: one state per channel (`len()` is the audio
/// width, built with the family and never resized) and a [`Glide`] of the coefficients
/// every channel shares.
#[derive(Clone, Debug)]
///
/// The outputs are the family's bands in order ([`Channels`]): band `o` is the
/// `o`-th run of `width` output slices, and channel `c`'s output `o` is channel `c`
/// of it.
pub struct PerChannel<C> {
    recurrence: C,
    width: usize,
}

impl<C: ChannelRecurrence> PerChannel<C> {
    /// The channels `first..first + N` through `c`, sample-inner, the group's states in
    /// registers. A recursive filter is latency-bound on one channel (each sample waits
    /// for the last), so running a few channels' independent chains side by side fills
    /// that latency while each still reads and writes its own planar slices. Measured on
    /// the f64 SVF at width 6, one block: all six as one group 0.49 us, as 3 + 3
    /// 0.60 us, one channel at a time 1.33 us.
    #[inline]
    fn held_group<const N: usize>(
        &self,
        c: &C::Coeffs,
        state: &mut [C::State],
        first: usize,
        input: &[&[f32]],
        output: &mut [&mut [f32]],
        frames: &Range<usize>,
    ) {
        let outputs = self.recurrence.outputs();
        let xs: [&[f32]; N] = core::array::from_fn(|k| &input[first + k][frames.clone()]);
        let mut ys: [&mut [f32]; LANES * MAX_OUTPUTS] =
            core::array::from_fn(|_| Default::default());
        for (o, band) in output.chunks_mut(self.width).take(outputs).enumerate() {
            for (k, out) in band[first..first + N].iter_mut().enumerate() {
                ys[k * MAX_OUTPUTS + o] = &mut out[frames.clone()];
            }
        }
        let mut st: [C::State; N] = core::array::from_fn(|k| state[first + k]);
        for i in 0..frames.len() {
            for k in 0..N {
                let frame = C::tick(c, &mut st[k], xs[k][i]);
                for (o, &y) in frame.as_ref().iter().take(outputs).enumerate() {
                    ys[k * MAX_OUTPUTS + o][i] = y;
                }
            }
        }
        state[first..first + N].copy_from_slice(&st);
    }
}

impl<C: ChannelRecurrence> Recurrence for PerChannel<C> {
    type Member = C::Member;
    type Capacity = ();
    type Target = C::Coeffs;
    type Coeffs = Glide<C::Coeffs>;
    type State = Vec<C::State>;

    const STEP: usize = 1;

    fn start(&self, model: &C::Member) -> Start<Self> {
        let at = self.recurrence.coeffs(model);
        Start {
            coeffs: Glide::landed(at).by(C::STEPS),
            state: vec![C::State::default(); self.width],
            target: at,
        }
    }

    /// κ: the coefficients the step reads at `model`. Every model is one it can run.
    fn coefficients_into(&self, model: &C::Member, target: &mut C::Coeffs) -> bool {
        *target = self.recurrence.coeffs(model);
        true
    }

    fn clear(&self, state: &mut Vec<C::State>) {
        state.fill(C::State::default());
    }

    fn run(
        &self,
        coeffs: &Glide<C::Coeffs>,
        state: &mut Vec<C::State>,
        input: &[&[f32]],
        output: &mut [&mut [f32]],
        frames: Range<usize>,
        _: usize,
    ) {
        let c = coeffs.now();
        if frames.len() == 1 {
            // A step of a glide: no inner loop to run a group over.
            let (i, outputs) = (frames.start, self.recurrence.outputs());
            for (ch, s) in state.iter_mut().enumerate() {
                let frame = C::tick(&c, s, input[ch][i]);
                let bands = output.chunks_mut(self.width).take(outputs);
                for (band, &y) in bands.zip(frame.as_ref()) {
                    band[ch][i] = y;
                }
            }
            return;
        }
        macro_rules! group {
            ($n:literal, $first:expr) => {
                self.held_group::<$n>(&c, state, $first, input, output, &frames)
            };
        }
        let mut first = 0;
        while first < self.width {
            let n = lane_group(self.width - first);
            match n {
                8 => group!(8, first),
                7 => group!(7, first),
                6 => group!(6, first),
                5 => group!(5, first),
                4 => group!(4, first),
                3 => group!(3, first),
                2 => group!(2, first),
                _ => group!(1, first),
            }
            first += n;
        }
    }

    fn effort(&self) -> Effort {
        Effort::Moderate
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};
    use tutti_types::SampleRate;

    use super::*;
    use crate::contract::FamilyCase;

    /// The glide's midpoint between two reachable controls is itself stable: an impulse
    /// through the recurrence with the half-way coefficients held decays (the last
    /// thousand frames of 150,000 peak below half of the first thousand's). A biquad's
    /// stable region in `(a1, a2)` is a triangle and so convex, and the ladder's loop is
    /// stable while its feedback is under the resonance limit, which a blend of two
    /// reachable sets keeps under; a glide that extrapolates leaves both.
    pub(crate) fn assert_midpoints_decay<R>(d: &Filter<R>)
    where
        R: ChannelRecurrence,
        R::Controls: core::fmt::Debug,
        Filter<R>: FamilyCase<Controls = R::Controls, Member = R::Member>,
    {
        const FRAMES: usize = 150_000;
        const WINDOW: usize = 1_000;
        let rate = SampleRate(48_000.0);
        let all = d.reachable();
        let mut rng = StdRng::seed_from_u64(11);
        for _ in 0..300 {
            let (a, b) = (
                all[rng.gen_range(0..all.len())],
                all[rng.gen_range(0..all.len())],
            );
            let (from, to) = (
                d.recurrence.coeffs(&d.member(&a, rate)),
                d.recurrence.coeffs(&d.member(&b, rate)),
            );
            let mid = from.lerp(&to, R::STEPS / 2, R::STEPS);
            let mut state = R::State::default();
            let mut peak = |from: usize, to: usize, x: f32| {
                (from..to)
                    .map(|i| {
                        let input = if i == 0 { x } else { 0.0 };
                        R::tick(&mid, &mut state, input).as_ref()[0].abs()
                    })
                    .fold(0.0f32, f32::max)
            };
            let first = peak(0, WINDOW, 1.0);
            peak(WINDOW, FRAMES - WINDOW, 0.0);
            let last = peak(FRAMES - WINDOW, FRAMES, 0.0);
            assert!(
                first.is_finite() && last.is_finite() && last <= 0.5 * first,
                "{a:?} -> {b:?}: first {first}, last {last}"
            );
        }
    }
}
