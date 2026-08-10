//! The crate-wide error type, for the paths that can fail on file access.
//!
//! Narrower failures live with the operation that raises them — [`ProbeError`]
//! for a header read, `ButlerGone` for a command that outlived its thread —
//! because a caller of those branches on the specific cause rather than on "an
//! error happened".
//!
//! [`ProbeError`]: crate::ProbeError

use thiserror::Error;

/// A failure on a sampler path that touches the filesystem.
///
/// `#[non_exhaustive]`: a match on this must carry a `_` arm, so adding a
/// variant for a new failure mode is not a breaking change.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum Error {
    /// The underlying file could not be read or written.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// `Result` specialized to this crate's [`Error`](enum@Error).
pub type Result<T> = std::result::Result<T, Error>;
