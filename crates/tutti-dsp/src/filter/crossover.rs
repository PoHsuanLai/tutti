//! The Linkwitz-Riley crossover: its model, its design over [`Filter`](super::Filter) (the
//! band tree as a kernel), and the block processor `Crossover` the multiband node still runs
//! (`legacy`, deleted when the dynamics lane moves it onto the design).

mod design;
mod legacy;
mod model;

pub use design::{CrossoverControls, CrossoverDesign, CrossoverTree};
pub use legacy::{Crossover, CrossoverError, MAX_SPLITS, SOLVE_CEILING, fit_to_rate};
pub use model::CrossoverModel;
