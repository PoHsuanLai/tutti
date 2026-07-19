//! Shared-memory audio transport.

mod mmap;
mod slab;

pub use crate::protocol::SlabLayout;
pub use slab::AudioSlab;
