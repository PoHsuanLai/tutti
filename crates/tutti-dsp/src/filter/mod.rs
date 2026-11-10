//! Filter algorithms: state-variable, Moog ladder and the Linkwitz-Riley
//! crossover, over the [`Real`] float they share.

mod biquad;
mod crossover;
mod crossover_model;
mod ladder;
mod ladder_model;
mod real;
mod svf;
#[cfg(test)]
mod svf_contract;
mod svf_model;

pub use biquad::Biquad;
pub use crossover::{Crossover, CrossoverError, MAX_SPLITS, SOLVE_CEILING, fit_to_rate};
pub use crossover_model::CrossoverModel;
pub use ladder::{LadderCoefficients, LadderCoeffs, LadderType, ladder_step};
pub use ladder_model::LadderModel;
pub use real::Real;
pub use svf::{SvfCoefficients, SvfCoeffs, SvfType, svf_step};
pub use svf_model::{SvfBounds, SvfControls, SvfDesign, SvfModel, SvfRealization};
