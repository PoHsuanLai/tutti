//! What decoding a file can fail with.

use std::borrow::Cow;

use crate::DecodeError;

use symphonia::core::errors::Error as Symphonia;

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
