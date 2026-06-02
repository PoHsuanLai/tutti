//! Internal implementation modules for the facade types. The public API
//! is assembled in `crate::lib.rs` via the namespace modules (`play`,
//! `capture`, `metrics`, `input`, `stretch`, `file`, `preview`) plus the
//! root `Sampler` / `SamplerBuilder` re-exports.

pub(crate) mod auditioner;
pub(crate) mod builders;
pub(crate) mod channel;
pub(crate) mod import;
pub(crate) mod metrics;
mod system;

pub use system::{Sampler, SamplerBuilder};
