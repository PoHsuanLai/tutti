//! What decoding a file can fail with.

use std::borrow::Cow;
use std::io;

use symphonia::core::errors::Error as Symphonia;
use thiserror::Error;

/// Why a file could not be probed, decoded or sought.
///
/// `#[non_exhaustive]`: a decoder behind it may bring failure modes of its
/// own, so a caller keeps a wildcard arm. The messages are `Cow<'static, str>`:
/// a decoder with owned text builds them without a change to this type, and a
/// static one costs no allocation to build or clone.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum DecodeError {
    /// The file could not be opened or read.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    /// The container or codec is not one this build reads.
    #[error("unsupported: {0}")]
    Unsupported(Cow<'static, str>),

    /// The data is damaged or does not follow its format.
    #[error("malformed: {0}")]
    Malformed(Cow<'static, str>),

    /// The container cannot seek there.
    #[error("cannot seek: {0}")]
    Seek(Cow<'static, str>),

    /// The file exceeds a limit the decoder keeps.
    #[error("over a decoder limit: {0}")]
    Limit(Cow<'static, str>),

    /// The container holds no track this build can decode.
    #[error("the file holds no audio track this build can decode")]
    NoAudioTrack,
}

impl Clone for DecodeError {
    /// A copy that keeps an I/O error's kind and message, not its source.
    fn clone(&self) -> Self {
        match self {
            Self::Io(e) => Self::Io(io::Error::new(e.kind(), e.to_string())),
            Self::Unsupported(m) => Self::Unsupported(m.clone()),
            Self::Malformed(m) => Self::Malformed(m.clone()),
            Self::Seek(m) => Self::Seek(m.clone()),
            Self::Limit(m) => Self::Limit(m.clone()),
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
        Symphonia::DecodeError(m) => DecodeError::Malformed(m.into()),
        Symphonia::SeekError(kind) => DecodeError::Seek(Cow::Borrowed(match kind {
            SeekErrorKind::Unseekable => "the stream is not seekable",
            SeekErrorKind::ForwardOnly => "the stream only seeks forward",
            SeekErrorKind::OutOfRange => "the position is out of range",
            SeekErrorKind::InvalidTrack => "the track does not exist",
        })),
        Symphonia::Unsupported(m) => DecodeError::Unsupported(m.into()),
        Symphonia::LimitError(m) => DecodeError::Limit(m.into()),
        Symphonia::ResetRequired => DecodeError::Malformed("the decoder needs a reset".into()),
    }
}

#[cfg(test)]
mod tests;
