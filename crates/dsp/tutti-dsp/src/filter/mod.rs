//! Filter algorithms: state-variable, Moog ladder and the Linkwitz-Riley
//! crossover, over the [`Real`] float they share.

mod crossover;
mod ladder;
mod real;
mod svf;

pub use crossover::{Crossover, CrossoverError, MAX_SPLITS, SOLVE_CEILING, fit_to_rate};
pub use ladder::{LadderCoefficients, LadderCoeffs, LadderType, ladder_step};
pub use real::Real;
pub use svf::{SvfCoefficients, SvfCoeffs, SvfType, svf_step};
