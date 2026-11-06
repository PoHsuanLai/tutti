//! CLAP `clap_istream`/`clap_ostream` adapters backed by in-memory buffers,
//! used by state save/load. The FFI crate owns them: a plugin reads from an
//! [`InputStream`] and writes to an [`OutputStream`] without the host ever
//! having to touch FFI.

pub use tutti_clap_ffi::{InputStream, OutputStream};
