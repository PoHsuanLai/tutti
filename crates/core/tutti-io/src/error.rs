//! What the live I/O edge can fail with.
//!
//! One [`Error`] for the crate. The variant that earns it a type of its own is
//! [`ChannelWidthMismatch`](Error::ChannelWidthMismatch): [`Recorder::start`]'s
//! refusal is the release-build half of the width-agreement invariant (the
//! debug half is [`pump`]'s `debug_assert_eq!`), and a safety net a caller may
//! want to branch on cannot be a formatted string inside an `io::Error` — it
//! carries both layouts as values, not prose.
//!
//! [`Recorder::start`]: crate::Recorder::start
//! [`pump`]: crate::pump

use thiserror::Error;
use tutti_core::ChannelLayout;

/// Anything recording can fail with.
#[derive(Debug, Error)]
pub enum Error {
    /// The source and sink disagree on channel width.
    ///
    /// The runtime replacement for the compile error the const-width I/O traits
    /// gave up — the `recorder` module header explains the trade. The message
    /// `Debug`-formats both layouts because `ChannelLayout` names the common
    /// widths ("Stereo", not "ChannelLayout(2)"), and this line is the one an
    /// author has to act on.
    #[error(
        "recorder source is {src:?} but the sink is {sink:?}; \
         recording a mismatched pair rotates the file's channels every frame"
    )]
    ChannelWidthMismatch {
        /// What the [`AudioIn`](crate::AudioIn) source reports.
        src: ChannelLayout,
        /// What the [`AudioOut`](crate::AudioOut) sink was opened with.
        sink: ChannelLayout,
    },

    /// File I/O failed — opening the sink, writing frames, or the header
    /// back-patch in [`finalize`](crate::AudioOut::finalize).
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// `std::result::Result` with this crate's [`Error`](enum@Error) as the error
/// type.
pub type Result<T> = std::result::Result<T, Error>;
