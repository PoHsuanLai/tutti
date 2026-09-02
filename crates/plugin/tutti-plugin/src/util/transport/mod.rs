//! Host-side transports between this crate and the plugin-server subprocess.
//!
//! Two transports:
//! - [`control`] — typed length-prefixed bincode over a local socket for
//!   commands and responses.
//! - [`shm`] — named shared-memory audio storage for bulk sample transfer.
//!
//! [`state_chunk`] is not a third transport: it is the splitting and
//! reassembly plugin state needs in order to *use* [`control`], whose frames
//! are deliberately too small to carry a preset whole.

pub mod control;
pub mod shm;
pub mod state_chunk;
