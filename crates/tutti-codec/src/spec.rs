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
    /// The one place a rate is checked, for every format: a header cannot hold
    /// a rate that is not a whole number of Hz from 1 to `u32::MAX`, and an
    /// out-of-range float would otherwise saturate or (AIFF) be written raw.
    /// (AIFF states an extended float and writes [`rate`](Self::rate) as it is,
    /// once it has passed this check.)
    ///
    /// # Errors
    ///
    /// [`SpecError::SampleRate`] when the rate rounds to zero, exceeds
    /// `u32::MAX` Hz or is not finite.
    pub fn header_rate(&self) -> Result<NonZeroU32> {
        let hz = self.rate.get().round();
        if hz.is_finite() && hz <= f64::from(u32::MAX) {
            // Finite and in range: the cast is exact.
            if let Some(rate) = NonZeroU32::new(hz as u32) {
                return Ok(rate);
            }
        }
        Err(SpecError::SampleRate(self.rate.get()).into())
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
