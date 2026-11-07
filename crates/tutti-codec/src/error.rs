//! What an encoder can fail with.

use std::io;
use thiserror::Error;
use tutti_types::ChannelLayout;

/// Anything that can go wrong creating, writing or finishing an encode.
///
/// `#[non_exhaustive]`: the codecs behind the format features each bring their
/// own failure modes, so a caller keeps a wildcard arm.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum Error {
    /// The sink failed: a full disk, a closed file, a failed seek. Also the
    /// codec libraries' own I/O faults.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    /// A [`Spec`](crate::Spec) the format cannot be written for.
    #[error("Invalid spec: {0}")]
    Spec(#[from] SpecError),

    /// No format is written for this file extension (lowercased, without its
    /// dot, or `"(none)"` when the path has none), or its feature is off.
    #[error("Unsupported format {extension}: the extension is not recognised")]
    UnknownExtension {
        /// The extension.
        extension: String,
    },

    /// A codec rejected a block or failed to finalize its container.
    /// `detail` is the library's own report.
    #[error("{codec} encoding error: {detail}")]
    Encoding {
        /// The codec that failed.
        codec: Codec,
        /// The library's report.
        detail: String,
    },

    /// The WAV file would exceed what its 32-bit sizes can describe (just under
    /// 4 GiB; RF64 is not written). `limit_frames` is the most frames a file of
    /// this width and depth holds. Choose another format or a shorter span.
    #[error(
        "A WAV file cannot hold more than {limit_frames} frames (32-bit sizes, just under 4 GiB)"
    )]
    WavTooLarge {
        /// The frames a WAV of this width and depth can hold.
        limit_frames: u64,
    },

    /// A block whose width is not the encoder's.
    #[error("A block of {got:?} was written to an encoder of {expected:?}")]
    BlockWidth {
        /// The width the encoder was created for.
        expected: ChannelLayout,
        /// The block's width.
        got: ChannelLayout,
    },
}

impl Clone for Error {
    /// A copy that keeps an I/O error's kind and message, not its source.
    fn clone(&self) -> Self {
        match self {
            Self::Io(e) => Self::Io(io::Error::new(e.kind(), e.to_string())),
            Self::Spec(e) => Self::Spec(e.clone()),
            Self::UnknownExtension { extension } => Self::UnknownExtension {
                extension: extension.clone(),
            },
            Self::Encoding { codec, detail } => Self::Encoding {
                codec: *codec,
                detail: detail.clone(),
            },
            Self::WavTooLarge { limit_frames } => Self::WavTooLarge {
                limit_frames: *limit_frames,
            },
            Self::BlockWidth { expected, got } => Self::BlockWidth {
                expected: *expected,
                got: *got,
            },
        }
    }
}

impl From<Error> for io::Error {
    /// The `io::Error` itself for [`Error::Io`], else the error wrapped, for a
    /// sink that speaks `io::Result`.
    fn from(error: Error) -> Self {
        match error {
            Error::Io(e) => e,
            other => io::Error::other(other),
        }
    }
}

/// A [`Spec`](crate::Spec) the format cannot be written for
/// ([`Error::Spec`]).
#[derive(Error, Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum SpecError {
    /// A sample rate no header can hold: it rounds to zero, exceeds
    /// `u32::MAX` Hz or is not finite.
    #[error("a sample rate of {0} Hz does not fit a file header (1 Hz to 4294967295 Hz)")]
    SampleRate(f64),
    /// A channel count of zero.
    #[error("channel count must be non-zero")]
    ZeroChannels,
    /// More channels than the container's header can count.
    #[error("{channels} channels exceed the format's limit of {max}")]
    TooManyChannels {
        /// The requested channel count.
        channels: u16,
        /// The most the format holds.
        max: u16,
    },
    /// The channel topology and the file differ in width.
    #[error("the topology is {topology:?} wide but the file has {file:?}")]
    TopologyWidth {
        /// The topology's layout.
        topology: ChannelLayout,
        /// The file's layout.
        file: ChannelLayout,
    },
}

/// The codec behind an [`Error::Encoding`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Codec {
    /// FLAC.
    Flac,
    /// AIFF and AIFF-C.
    Aiff,
    /// Ogg Vorbis.
    Ogg,
}

impl std::fmt::Display for Codec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Flac => "FLAC",
            Self::Aiff => "AIFF",
            Self::Ogg => "Ogg",
        })
    }
}

impl Error {
    #[cfg_attr(
        not(any(
            feature = "encode-flac",
            feature = "encode-aiff",
            feature = "encode-vorbis"
        )),
        expect(
            dead_code,
            reason = "only the flac, aiff and vorbis encoders report this"
        )
    )]
    pub(crate) fn encoding(codec: Codec, detail: impl Into<String>) -> Self {
        Self::Encoding {
            codec,
            detail: detail.into(),
        }
    }
}

/// `std::result::Result` with this crate's [`Error`](enum@Error).
pub type Result<T> = std::result::Result<T, Error>;
