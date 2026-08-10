//! The ring-buffer plumbing between a driver callback and the audio thread.
//!
//! A backend pushes inbound events into an [`InputProducerHandle`] from whatever
//! thread the OS delivers on; the audio thread drains every port for the block
//! through [`HardwareMidiInputs`]. The lock-free SPSC ring underneath is private
//! (`spsc`) because its soundness rests on a single-producer / single-consumer
//! invariant that only this module's API can uphold.
//!
//! Nothing here allocates on the drain path — the per-block scratch is
//! fixed-capacity, and a flood spreads across blocks rather than growing it.

pub(crate) mod async_port;
mod manager;
mod spsc;

pub use async_port::InputProducerHandle;
pub use manager::{HardwareMidiInputs, PortInfo, PortType};
