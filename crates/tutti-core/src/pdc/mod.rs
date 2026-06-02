//! Plugin Delay Compensation (PDC) system.

pub(crate) mod delay_buffer;
pub(crate) mod graph_compensator;
pub(crate) mod manager;
mod mono_delay;
mod unit;

pub use manager::{PdcManager, PdcState};
pub(crate) use mono_delay::MonoPdcDelayUnit;
pub use unit::PdcDelayUnit;
