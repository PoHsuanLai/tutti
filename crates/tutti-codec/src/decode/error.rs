//! What decoding a file can fail with.

use std::io;

use symphonia::core::errors::Error as Symphonia;
use thiserror::Error;

/// Why a file could not be probed, decoded or sought.
///
/// `#[non_exhaustive]`: a decoder behind it may bring failure modes of its
/// own, so a caller keeps a wildcard arm.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum DecodeError {
    /// The file could not be opened or read.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    /// The container or codec is not one this build reads.
    #[error("unsupported: {0}")]
    Unsupported(&'static str),

    /// The data is damaged or does not follow its format.
    #[error("malformed: {0}")]
    Malformed(&'static str),

    /// The container cannot seek there.
    #[error("cannot seek: {0}")]
    Seek(&'static str),

    /// The file exceeds a limit the decoder keeps.
    #[error("over a decoder limit: {0}")]
    Limit(&'static str),

    /// The container holds no track this build can decode.
    #[error("the file holds no audio track this build can decode")]
    NoAudioTrack,
}

impl Clone for DecodeError {
    /// A copy that keeps an I/O error's kind and message, not its source.
    fn clone(&self) -> Self {
        match self {
            Self::Io(e) => Self::Io(io::Error::new(e.kind(), e.to_string())),
            Self::Unsupported(m) => Self::Unsupported(m),
            Self::Malformed(m) => Self::Malformed(m),
            Self::Seek(m) => Self::Seek(m),
            Self::Limit(m) => Self::Limit(m),
            Self::NoAudioTrack => Self::NoAudioTrack,
        }
    }
}

/// The decoder's error as ours. A free function and not a `From` impl, which
/// would put the decoder's type in a public trait impl.
pub(super) fn convert(error: Symphonia) -> DecodeError {
    use symphonia::core::errors::SeekErrorKind;
    match error {
        Symphonia::IoError(e) => DecodeError::Io(e),
        Symphonia::DecodeError(m) => DecodeError::Malformed(m),
        Symphonia::SeekError(kind) => DecodeError::Seek(match kind {
            SeekErrorKind::Unseekable => "the stream is not seekable",
            SeekErrorKind::ForwardOnly => "the stream only seeks forward",
            SeekErrorKind::OutOfRange => "the position is out of range",
            SeekErrorKind::InvalidTrack => "the track does not exist",
        }),
        Symphonia::Unsupported(m) => DecodeError::Unsupported(m),
        Symphonia::LimitError(m) => DecodeError::Limit(m),
        Symphonia::ResetRequired => DecodeError::Malformed("the decoder needs a reset"),
    }
}

#[cfg(test)]
mod tests;
