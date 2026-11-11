//! Filter algorithms: state-variable, Moog ladder and the Linkwitz-Riley
//! crossover, over the [`Real`] float they share.

mod biquad;
mod crossover;
mod crossover_model;
mod ladder;
mod ladder_design;
mod ladder_model;
mod real;
mod svf;
mod svf_design;
mod svf_model;

pub use biquad::{Biquad, DirectForm1};
pub use crossover::{Crossover, CrossoverError, MAX_SPLITS, SOLVE_CEILING, fit_to_rate};
pub use crossover_model::CrossoverModel;
pub use ladder::{LadderCoeffs, LadderType};
pub use ladder_design::{LadderDesign, LadderRealization, LadderSizes};
pub use ladder_model::{LadderControls, LadderModel};
pub use real::Real;
pub use svf::{SvfCoeffs, SvfType};
pub use svf_design::{SvfDesign, SvfRealization, SvfSizes};
pub use svf_model::{SvfControls, SvfModel};
