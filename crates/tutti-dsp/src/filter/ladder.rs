//! The Moog ladder: its model, its kernel (coefficients and step), and its design over
//! [`Filter`](super::Filter).

mod design;
mod kernel;
mod model;

pub use design::{Ladder, LadderDesign};
pub use kernel::{LadderCoefficients, LadderCoeffs, LadderType, ladder_step};
pub use model::{LadderControls, LadderModel};
