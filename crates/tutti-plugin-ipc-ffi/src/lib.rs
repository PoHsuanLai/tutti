//! The `unsafe` of Tutti's plugin process edge, each piece behind a safe call,
//! so `tutti-plugin-ipc` and `tutti-plugin-server` forbid `unsafe` (the owner's
//! rule: `unsafe` lives in one self-contained place behind a safe wrapper; the
//! per-format `-ffi` crates are the model).
//!
//! - **The audio slab's shared memory** ([`MappingWriter`], [`SharedMapping`],
//!   [`SlabHeader`]): the file, the mapping, the header both processes map (its
//!   layout is the ABI between them), and the only copies in and out. A mapping
//!   exists only with its one writer, which is what makes the copies safe to
//!   call. What the data region holds, and where, is `tutti-plugin-ipc`'s.
//! - **The OS calls** (`wait_readable` on unix, `process_exists`, and
//!   `parent_process_id` and `pipe_bytes_available` on Windows): the server's
//!   wait for its host and its check that the host still runs, and the host's
//!   peek at a named pipe.
//!
//! It names no tutti crate.

mod header;
mod mapping;
mod os;

pub use header::{Direction, RING_SLOTS, SLAB_HEADER_BYTES, SlabHeader, slot_for};
pub use mapping::{MappingWriter, SharedMapping, remove};
pub use os::process_exists;
#[cfg(unix)]
pub use os::wait_readable;
#[cfg(windows)]
pub use os::{parent_process_id, pipe_bytes_available};

/// Why a slab could not be made, opened or trusted.
#[derive(Debug, thiserror::Error)]
pub enum SlabError {
    /// The backing file could not be created, opened, sized or mapped.
    #[error("{0}")]
    File(String),
    /// The peer's slab is shorter than the layout this side expects.
    #[error("slab is {actual} bytes, layout needs {expected}")]
    Short {
        /// The file's length.
        actual: u64,
        /// The length the layout needs.
        expected: usize,
    },
    /// The mapping is not a tutti slab of this build's header version and ring
    /// depth.
    #[error("{0}")]
    Header(String),
}
