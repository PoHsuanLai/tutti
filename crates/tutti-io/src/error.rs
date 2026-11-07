//! What the live I/O edge can fail with.
//!
//! One [`Error`](enum@Error) for the crate. The variant that earns it a type of its own is
//! [`ChannelWidthMismatch`](Error::ChannelWidthMismatch): [`Recorder::start`]'s
//! refusal is the release-build half of the width-agreement invariant (the
//! debug half is [`pump`]'s `debug_assert_eq!`), and a safety net a caller may
//! want to branch on cannot be a formatted string inside an `io::Error` — it
//! carries both layouts as values, not prose.
//!
//! [`Recorder::start`]: crate::Recorder::start
//! [`pump`]: tutti_types::pump

use thiserror::Error;
use tutti_types::ChannelLayout;

/// An error from starting or finishing a recording.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// The source and sink disagree on channel width.
    ///
    /// Returned by [`Recorder::start`](crate::Recorder::start) before any frame
    /// moves: recording a mismatched pair would rotate the file's channels
    /// every frame.
    #[error(
        "recorder source is {src:?} but the sink is {sink:?}; \
         recording a mismatched pair rotates the file's channels every frame"
    )]
    ChannelWidthMismatch {
        /// What the [`AudioIn`](tutti_types::AudioIn) source reports.
        src: ChannelLayout,
        /// What the [`AudioOut`](tutti_types::AudioOut) sink was opened with.
        sink: ChannelLayout,
    },

    /// The source broke mid-take ([`AudioIn::is_failed`](tutti_types::AudioIn::is_failed)),
    /// for a microphone because its device went away.
    ///
    /// The frames delivered before the failure are in the sink, which was
    /// finalized: the file is valid and short.
    #[error("the recording source failed; the take holds only what it delivered before")]
    SourceFailed,

    /// File I/O failed — opening the sink, writing frames, or the header
    /// back-patch in [`finalize`](tutti_types::AudioOut::finalize).
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// `std::result::Result` with this crate's [`Error`](enum@Error) as the error
/// type.
pub type Result<T> = std::result::Result<T, Error>;

/// An [`Error`](enum@Error) as the `io::Error` a sink's
/// [`finalize`](tutti_types::AudioOut::finalize) speaks, for a host that reports
/// every way a take can end through one channel (`bevy-tutti`'s
/// `PumpFinished`).
///
/// [`Io`](Error::Io) unwraps to the error it carries.
/// [`ChannelWidthMismatch`](Error::ChannelWidthMismatch) becomes
/// [`InvalidInput`](std::io::ErrorKind::InvalidInput) with this error as its
/// source, so the layouts stay reachable through `get_ref`/`downcast`.
impl From<Error> for std::io::Error {
    fn from(error: Error) -> Self {
        match error {
            Error::Io(io) => io,
            Error::SourceFailed => std::io::Error::other(Error::SourceFailed),
            mismatch @ Error::ChannelWidthMismatch { .. } => {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, mismatch)
            }
        }
    }
}

impl Error {
    /// The error a take's `io::Error` result stands for: [`SourceFailed`](Self::SourceFailed)
    /// if that is what it carries, [`Io`](Self::Io) otherwise.
    pub(crate) fn from_take(error: std::io::Error) -> Self {
        match error.get_ref().and_then(|e| e.downcast_ref::<Self>()) {
            Some(Self::SourceFailed) => Self::SourceFailed,
            _ => Self::Io(error),
        }
    }
}
