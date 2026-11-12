//! [`LadderDesign`]: the Moog ladder as a [`Design`](crate::Design) (design 024 step 2b).
//!
//! The controls are [`LadderControls`], the model [`LadderModel`] (the small-signal
//! linearisation) and the kernel [`Ladder`]: the four stages with the `tanh` in the
//! loop, whose coefficients and drive are read from the model and narrowed once to the
//! state precision `F`. Like the SVF it is designed on a 16-frame grid counted on the
//! stream clock from the last reset ([`Design::CADENCE`](crate::Design::CADENCE)): the coefficients **and
//! the drive** ramp linearly over the 16 frames after each retarget and arrive exactly
//! on it, so a control is in the render at most 16 frames after it was sampled and the
//! render does not depend on how audio is split into blocks.

use core::marker::PhantomData;

use tutti_types::{BusTopology, SampleRate};

use super::kernel::LadderCoefficients;
use super::{LadderControls, LadderModel, LadderType};
use crate::Lerp;
use crate::filter::real::Real;
use crate::{ChannelKernel, Filter};

/// The [`Design`](crate::Design) of the ladder filter, `channels` wide, with state
/// precision `F` (`f64` in the nodes, for accuracy at low cutoffs). The coefficients
/// and the drive are shared across the channels and only the four stage words are per
/// channel.
pub type LadderDesign<F> = Filter<Ladder<F>>;

/// The ladder as a [`ChannelKernel`]: `[s0, s1, s2, s3]` per channel, in state precision
/// `F`.
#[derive(Clone, Copy, Debug)]
pub struct Ladder<F>(PhantomData<fn() -> F>);

impl<F> LadderDesign<F> {
    /// A design on the bus `input`, one output bus where it is.
    #[must_use]
    pub fn new(input: BusTopology) -> Self {
        Self::over(Ladder(PhantomData), input)
    }
}

/// What the ladder's glide blends: the coefficients and the drive, with the tap taken
/// as the output. A blend keeps the target's tap, so a type changes at once.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LadderPoint<F: Real> {
    c: LadderCoefficients<F>,
    drive: F,
    ty: LadderType,
}

impl<F: Real> LadderPoint<F> {
    /// The point of `model`. The model's drive is an `f64` made of an `f32`, so
    /// narrowing it to `F` is exact.
    fn of(model: &LadderModel) -> Self {
        Self {
            c: LadderCoefficients::from(model.coeffs()),
            drive: F::from_f64(model.drive()),
            ty: model.ladder_type(),
        }
    }
}

impl<F: Real> Lerp for LadderPoint<F> {
    #[inline]
    fn lerp(&self, to: &Self, k: u32, n: u32) -> Self {
        let t = F::from_f64(f64::from(k)) / F::from_f64(f64::from(n));
        Self {
            c: self.c.lerp(&to.c, t),
            drive: self.drive + (to.drive - self.drive) * t,
            ty: to.ty,
        }
    }
}

impl<F: Real> ChannelKernel for Ladder<F> {
    type Controls = LadderControls;
    type Model = LadderModel;
    type Coeffs = LadderPoint<F>;
    type State = [F; 4];
    type Frame = [f32; 1];

    const STEPS: u32 = 16;

    fn blank(&self, rate: SampleRate) -> LadderModel {
        LadderModel::new(LadderControls::DEFAULT, rate)
    }

    fn design_into(&self, controls: &LadderControls, rate: SampleRate, out: &mut LadderModel) {
        *out = LadderModel::new(*controls, rate);
    }

    fn coeffs(&self, model: &LadderModel) -> LadderPoint<F> {
        LadderPoint::of(model)
    }

    #[inline(always)]
    fn tick(p: &LadderPoint<F>, s: &mut [F; 4], x: f32) -> [f32; 1] {
        [p.c.step(p.ty, s, F::from_f32(x), p.drive).to_f32()]
    }
}

#[cfg(test)]
mod tests;
