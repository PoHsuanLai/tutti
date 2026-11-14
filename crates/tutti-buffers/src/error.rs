//! [`BufferError`], the one refusal a view's construction or a conversion gives.

use tutti_types::{ChannelLayout, Samples};

/// Why a view was not built, or a conversion was not run.
///
/// Nothing is truncated or zero-filled to make a mismatch fit: the call that
/// meets one returns the error and touches nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BufferError {
    /// The width is zero channels, so a frame count is undefined.
    #[error("a buffer needs at least one channel")]
    ZeroWidth,
    /// The width does not fit a [`ChannelLayout`] (above `u16::MAX` channels).
    #[error("{channels} channels do not fit a channel layout")]
    TooWide {
        /// The refused channel count.
        channels: usize,
    },
    /// An interleaved slice ends mid-frame.
    #[error("{len} samples are not a whole number of {width:?} frames")]
    PartialFrame {
        /// The slice length, in samples.
        len: usize,
        /// The frame width.
        width: ChannelLayout,
    },
    /// A plane is not as long as the first one.
    #[error("plane {plane} holds {found} frames where plane 0 holds {expected}")]
    RaggedPlane {
        /// The index of the offending plane.
        plane: usize,
        /// The first plane's length.
        expected: Samples,
        /// The offending plane's length.
        found: Samples,
    },
    /// Source and destination hold different frame counts.
    #[error("the source holds {src} frames and the destination {dst}")]
    LengthMismatch {
        /// The source's frame count.
        src: Samples,
        /// The destination's frame count.
        dst: Samples,
    },
    /// Source and destination have different widths.
    #[error("the source is {src:?} and the destination {dst:?}")]
    WidthMismatch {
        /// The source's width.
        src: ChannelLayout,
        /// The destination's width.
        dst: ChannelLayout,
    },
}
