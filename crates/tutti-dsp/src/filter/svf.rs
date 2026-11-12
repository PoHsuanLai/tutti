//! The state-variable filter: its model (controls to coefficients and response), its kernel
//! (the coefficients the step reads, and the step), and its design over [`Filter`](super::Filter).

mod design;
mod kernel;
mod model;

pub use design::{Svf, SvfDesign};
pub use kernel::{SvfCoefficients, SvfCoeffs, SvfType, svf_step};
pub use model::{SvfControls, SvfModel};
