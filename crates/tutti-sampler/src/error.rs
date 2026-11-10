//! The crate-wide error type, for the paths that can fail on file access.
//!
//! Narrower failures live with the operation that raises them — [`ProbeError`]
//! for a header read, `StreamerGone` for a command that outlived its thread —
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

/// Why a wave cannot be placed into a voice's channels.
///
/// Raised on the control side when a source is built over a wave
/// ([`MemorySource::with_config`](crate::MemorySource::with_config),
/// [`memory_voice`](crate::memory_voice)), never on the audio thread: a wave
/// that cannot be placed is refused rather than played with a channel in the
/// wrong place or dropped.
#[derive(Error, Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum PlaceError {
    /// A channel of the wave has no place in the voice's frame; the
    /// [`FoldError`](tutti_types::FoldError) says which and why. A file
    /// channel no speaker names (a second LFE, a wide or high speaker) is
    /// [`FoldError::Unnamed`](tutti_types::FoldError::Unnamed).
    #[error("the wave cannot be placed: {0}")]
    Fold(#[from] tutti_types::FoldError),
}
