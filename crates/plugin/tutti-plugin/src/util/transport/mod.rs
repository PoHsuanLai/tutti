//! Host-side transports between this crate and the plugin-server subprocess.
//!
//! Two transports:
//! - [`control`] — typed length-prefixed bincode over a local socket for
//!   commands and responses.
//! - [`shm`] — named shared-memory audio storage for bulk sample transfer.

pub mod control;
pub mod shm;
