//! What decoding a file can fail with. Ours, with no decoder type in it, so it
//! exists whatever features are on (only the conversion from the decoder's own
//! error lives in `decode`).

use std::borrow::Cow;
use std::io;

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

    /// The audio track states no sample rate. The rate is the file's to
    /// state: guessing one plays the file at the wrong pitch with no error
    /// (a valid file that plays at 0.92 of its speed), so a caller that knows
    /// the rate supplies it where it chooses to read.
    #[error("the audio track states no sample rate")]
    NoSampleRate,
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
            Self::NoSampleRate => Self::NoSampleRate,
        }
    }
}
