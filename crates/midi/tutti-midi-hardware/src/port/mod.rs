//! The ring-buffer plumbing between a driver callback and the audio thread.
//!
//! A backend pushes inbound events into an [`InputProducerHandle`] from whatever
//! thread the OS delivers on; the audio thread drains every port for the block
//! through [`HardwareMidiInputs`]. Each port is a lock-free SPSC ring split
//! once: the handle owns the producer half and the port keeps the consumer, so
//! a ring has one pusher and one drainer by ownership, with no `unsafe` here.
//!
//! Nothing here allocates on the drain path — the per-block scratch is
//! fixed-capacity, and a flood spreads across blocks rather than growing it.

pub(crate) mod async_port;
mod manager;

pub use async_port::InputProducerHandle;
pub use manager::{HardwareMidiInputs, PortId, PortInfo, PortType};
