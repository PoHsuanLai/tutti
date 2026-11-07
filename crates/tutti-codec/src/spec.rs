//! What a file is written as: the [`Spec`].

use crate::error::{Result, SpecError};
use crate::format::Format;
use std::num::NonZeroU32;
use tutti_types::{ChannelLayout, ChannelTopology, SampleRate};

/// Everything an [`Encoder`](crate::Encoder) needs to start a file.
///
/// Plain data, built by [`new`](Self::new) and `..` struct update. The format
/// carries its own settings, so nothing here is read by a format that cannot
/// use it, except the [`topology`](Self::topology), which only WAV states.
#[derive(Debug, Clone, PartialEq)]
pub struct Spec {
    /// The container and its settings.
    pub format: Format,
    /// The width of the file. Blocks written to the encoder have this width.
    pub layout: ChannelLayout,
    /// The rate the file declares. The encoder converts nothing: the samples
    /// handed to it are already at this rate.
    pub rate: SampleRate,
    /// Which speaker each file channel feeds, for the WAV speaker mask: `None`
    /// is the engine's SMPTE order for `layout` (mono, stereo and 5.1 named; a
    /// bare 4 or 8 unassigned), and a caller that knows it is writing quad
    /// passes [`ChannelTopology::quad`]. Its width must be `layout`. Samples
    /// are never reordered; the other formats ignore it.
    pub topology: Option<ChannelTopology>,
}

impl Spec {
    /// A file of `layout` at `rate`, with the default speaker order.
    #[must_use]
    pub fn new(format: Format, layout: ChannelLayout, rate: SampleRate) -> Self {
        Self {
            format,
            layout,
            rate,
            topology: None,
        }
    }

    /// The rate as the integer every codec header wants.
    ///
    /// The single narrowing point: hound's header, flacenc, vorbis all take an
    /// integer rate. (AIFF states an extended float and takes
    /// [`rate`](Self::rate) as it is.)
    ///
    /// # Errors
    ///
    /// [`SpecError::ZeroSampleRate`] when the rate rounds to zero or is not
    /// finite.
    pub fn header_rate(&self) -> Result<NonZeroU32> {
        let hz = self.rate.get().round();
        // A float to int `as` saturates, and sends NaN to zero.
        NonZeroU32::new(hz as u32).ok_or_else(|| SpecError::ZeroSampleRate.into())
    }

    /// The width as a non-zero channel count.
    ///
    /// # Errors
    ///
    /// [`SpecError::ZeroChannels`] for a zero width.
    pub fn channels(&self) -> Result<u16> {
        match self.layout.count() {
            0 => Err(SpecError::ZeroChannels.into()),
            n => Ok(n),
        }
    }
}
