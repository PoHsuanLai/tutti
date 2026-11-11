//! Filter algorithms: state-variable, Moog ladder and the Linkwitz-Riley
//! crossover, over the [`Real`] float they share.

mod biquad;
mod crossover;
mod crossover_design;
mod crossover_model;
mod ladder;
mod ladder_design;
mod ladder_model;
mod real;
mod sizes;
mod svf;
mod svf_design;
mod svf_model;

pub use biquad::{Biquad, DirectForm1};
pub use crossover::{Crossover, CrossoverError, MAX_SPLITS, SOLVE_CEILING, fit_to_rate};
pub use crossover_design::{CrossoverControls, CrossoverDesign, CrossoverRealization};
pub use crossover_model::CrossoverModel;
pub use ladder::{LadderCoeffs, LadderType};
pub use ladder_design::{LadderDesign, LadderRealization};
pub use ladder_model::{LadderControls, LadderModel};
pub use real::Real;
pub use sizes::FilterSizes;
pub use svf::{SvfCoeffs, SvfType};
pub use svf_design::{SvfDesign, SvfRealization};
pub use svf_model::{SvfControls, SvfModel};
