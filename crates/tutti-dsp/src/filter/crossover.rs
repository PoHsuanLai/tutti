//! The Linkwitz-Riley crossover: its model, its design over [`Filter`](super::Filter) (the
//! band tree as a kernel) and the state that runs the tree (`state`).

mod bands;
mod design;
mod model;
mod state;

pub use bands::{CrossoverError, MAX_SPLITS, SOLVE_CEILING, fit_to_rate};
pub use design::{CrossoverControls, CrossoverFamily, CrossoverTree};
pub use model::CrossoverModel;
pub(crate) use state::CrossoverState;
