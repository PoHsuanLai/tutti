//! Host-side transports between this crate and the plugin-server subprocess.
//!
//! Two transports:
//! - [`control`] — typed length-prefixed postcard over a local socket for
//!   commands and responses.
//! - [`shm`] — named shared-memory audio storage for bulk sample transfer.
//!
//! [`state_chunk`] is not a third transport: it is the splitting and
//! reassembly plugin state needs in order to *use* [`control`], whose frames
//! are deliberately too small to carry a preset whole.

pub mod control;
pub mod shm;
pub mod state_chunk;

// The control stream's platform difference (socket naming, waiting for bytes)
// is `sys`'s: this `mod` line carries the transport's only platform `cfg`.
#[cfg(unix)]
#[path = "sys/unix.rs"]
mod sys;
#[cfg(not(unix))]
#[path = "sys/windows.rs"]
mod sys;
