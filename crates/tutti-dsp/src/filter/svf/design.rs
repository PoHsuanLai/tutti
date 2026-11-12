//! [`SvfDesign`]: the state-variable filter as a [`Design`](crate::Design) (design 024 step 2b).
//!
//! The controls are [`SvfControls`], the model [`SvfModel`] and the kernel [`Svf`]:
//! Simper's trapezoidal form, whose coefficients are read from the model's own
//! (`SvfModel::coeffs`) and narrowed once to the state precision `F`. The filter is
//! designed on a 16-frame grid counted on the stream clock from the last reset
//! ([`Design::CADENCE`](crate::Design::CADENCE)): a node retargets it at each grid point it solves, and the
//! realization interpolates the coefficients linearly over the 16 frames that follow,
//! arriving exactly on the solved set at the segment's last frame. A control is
//! therefore in the render at most 16 frames after it was sampled, and the render does
//! not depend on how audio is split into blocks.

use core::marker::PhantomData;

use tutti_types::{BusTopology, SampleRate};

use super::kernel::SvfCoefficients;
use super::{SvfControls, SvfModel};
use crate::filter::real::Real;
use crate::{ChannelKernel, Filter};

/// The [`Design`](crate::Design) of the state-variable filter, `channels` wide, with
/// state precision `F`.
///
/// `F` defaults to `f64` in the nodes for accuracy at low cutoffs, where an `f32`
/// integrator loses resolution in the feedback path. The coefficients are shared
/// across the channels and only the integrator pair is per channel.
pub type SvfDesign<F> = Filter<Svf<F>>;

/// The state-variable filter as a [`ChannelKernel`]: `[ic1eq, ic2eq]` per channel, in
/// state precision `F`.
#[derive(Clone, Copy, Debug)]
pub struct Svf<F>(PhantomData<fn() -> F>);

impl<F> SvfDesign<F> {
    /// A design on the bus `input`, one output bus where it is.
    #[must_use]
    pub fn new(input: BusTopology) -> Self {
        Self::over(Svf(PhantomData), input)
    }
}

impl<F: Real> ChannelKernel for Svf<F> {
    type Controls = SvfControls;
    type Model = SvfModel;
    type Coeffs = SvfCoefficients<F>;
    type State = [F; 2];
    type Frame = [f32; 1];

    const STEPS: u32 = 16;

    fn blank(&self, rate: SampleRate) -> SvfModel {
        SvfModel::new(SvfControls::DEFAULT, rate)
    }

    fn design_into(&self, controls: &SvfControls, rate: SampleRate, out: &mut SvfModel) {
        *out = SvfModel::new(*controls, rate);
    }

    fn coeffs(&self, model: &SvfModel) -> SvfCoefficients<F> {
        SvfCoefficients::from(model.coeffs())
    }

    #[inline(always)]
    fn tick(c: &SvfCoefficients<F>, s: &mut [F; 2], x: f32) -> [f32; 1] {
        [c.step(s, F::from_f32(x)).to_f32()]
    }
}

#[cfg(test)]
mod tests;
