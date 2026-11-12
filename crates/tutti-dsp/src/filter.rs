//! Filter algorithms: state-variable, Moog ladder and the Linkwitz-Riley
//! crossover, over the [`Real`] float they share, each a [`ChannelKernel`] run by
//! [`PerChannel`] under the one [`Filter`] design.

mod biquad;
mod crossover;
mod ladder;
mod per_channel;
mod real;
mod sizes;
mod svf;

pub use biquad::{Biquad, DirectForm1};
pub use crossover::{
    Crossover, CrossoverControls, CrossoverDesign, CrossoverError, CrossoverModel, CrossoverTree,
    MAX_SPLITS, SOLVE_CEILING, fit_to_rate,
};
pub use ladder::{
    Ladder, LadderCoeffs, LadderControls, LadderDesign, LadderModel, LadderType, ladder_step,
};
pub use per_channel::{ChannelKernel, Filter, PerChannel};
pub use real::{Lanes, Real};
pub use sizes::FilterSizes;
pub use svf::{Svf, SvfCoeffs, SvfControls, SvfDesign, SvfModel, SvfType, svf_step};
