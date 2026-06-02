//! Spatial utilities — re-exports the shared smoothing primitives that used
//! to live here so the rest of `spatial/` can continue importing from
//! `super::utils`.

pub(crate) use crate::smoothing::{ExponentialSmoother, DEFAULT_POSITION_SMOOTH_TIME};
