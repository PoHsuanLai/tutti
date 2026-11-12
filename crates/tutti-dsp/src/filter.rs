//! Filter algorithms: state-variable, Moog ladder and the Linkwitz-Riley
//! crossover, over the [`Real`] float they share, each a [`ChannelRecurrence`] run by
//! [`PerChannel`] under the one [`Filter`] design.

mod biquad;
mod crossover;
mod ladder;
mod per_channel;
mod real;
mod svf;

pub use biquad::{Biquad, DirectForm1};
pub use crossover::{
    Crossover, CrossoverControls, CrossoverError, CrossoverFamily, CrossoverModel, CrossoverTree,
    MAX_SPLITS, SOLVE_CEILING, fit_to_rate,
};
pub use ladder::{
    Ladder, LadderCoefficients, LadderCoeffs, LadderControls, LadderFamily, LadderModel,
    LadderType, ladder_step,
};
pub use per_channel::{ChannelRecurrence, Filter, PerChannel};
pub use real::{Lanes, Real};
pub use svf::{
    Svf, SvfCoefficients, SvfCoeffs, SvfControls, SvfFamily, SvfModel, SvfType, svf_step,
};
