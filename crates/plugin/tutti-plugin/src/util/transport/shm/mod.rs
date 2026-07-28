//! Shared-memory audio transport.

mod header;
mod mmap;
mod slab;

pub use crate::protocol::SlabLayout;
pub use header::RING_SLOTS;
pub use slab::AudioSlab;
