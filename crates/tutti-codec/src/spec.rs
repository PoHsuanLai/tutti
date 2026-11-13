//! What a file is written as: the [`Spec`].

use crate::error::{Result, SpecError};
use crate::format::Format;
use std::num::NonZeroU32;
use tutti_types::{ChannelLayout, ChannelTopology, SampleRate};

/// Everything an [`Encoder`](crate::Encoder) needs to start a file.
///
/// The speakers are the one statement of the file's width: the
/// [`topology`](Self::topology) says which speaker each file channel feeds, and
/// [`layout`](Self::layout) is its length, so the two cannot disagree. The fields
/// are private (a [`new`](Self::new) and getters), and the format carries its own
/// settings, so nothing here is read by a format that cannot use it, except the
/// topology, which WAV (the speaker mask) and Ogg Vorbis (the channel order) read.
/// WAV writes the samples as given. Ogg Vorbis permutes 3 to 8 channels into the
/// order its specification fixes for the width, and refuses a width whose
/// speakers it cannot place (see `VorbisWriter`); the other formats ignore it.
///
/// A spec has no width of its own to disagree with its topology, and its
/// fields are not written outside the crate:
///
/// ```compile_fail,E0451
/// # use tutti_codec::{Format, Pcm, Spec};
/// # use tutti_types::{ChannelTopology, SampleRate};
/// let _ = Spec {
///     format: Format::Wav(Pcm::Int16),
///     topology: ChannelTopology::quad(),
///     rate: SampleRate::SR_48K,
/// };
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct Spec {
    format: Format,
    topology: ChannelTopology,
    rate: SampleRate,
}

impl Spec {
    /// A file of `format` whose channels feed the speakers of `topology`, at
    /// `rate`. The encoder converts nothing: the samples handed to it are
    /// already at this rate.
    #[must_use]
    pub fn new(format: Format, topology: ChannelTopology, rate: SampleRate) -> Self {
        Self {
            format,
            topology,
            rate,
        }
    }

    /// A file of `layout` at `rate` whose speakers are the engine's order for
    /// the width ([`ChannelTopology::smpte`]: mono, stereo, 5.1, 7.1 and 7.1.4
    /// named), and unassigned channels for every other width, 4 included (quad
    /// and B-format are both four channels, so a caller that knows it is writing
    /// quad passes [`ChannelTopology::quad`] to [`new`](Self::new)).
    #[must_use]
    pub fn of_width(format: Format, layout: ChannelLayout, rate: SampleRate) -> Self {
        let topology =
            ChannelTopology::smpte(layout).unwrap_or_else(|| ChannelTopology::unassigned(layout));
        Self::new(format, topology, rate)
    }

    /// The container and its settings.
    #[must_use]
    pub fn format(&self) -> Format {
        self.format
    }

    /// Which speaker each file channel feeds, for the WAV speaker mask and Ogg
    /// Vorbis's channel order.
    #[must_use]
    pub fn topology(&self) -> &ChannelTopology {
        &self.topology
    }

    /// The width of the file: the topology's length. Blocks written to the
    /// encoder have this width.
    #[must_use]
    pub fn layout(&self) -> ChannelLayout {
        self.topology.layout()
    }

    /// The rate the file declares.
    #[must_use]
    pub fn rate(&self) -> SampleRate {
        self.rate
    }

    /// The rate as the integer every codec header wants.
    ///
    /// The one place a rate is fitted to a header, for every format: a header
    /// cannot hold a rate that is not a whole number of Hz from 1 to
    /// `u32::MAX`, and an out-of-range float would otherwise saturate or
    /// (AIFF) be written raw.
    /// (AIFF states an extended float and writes [`rate`](Self::rate) as it is,
    /// once it has passed this check.)
    ///
    /// # Errors
    ///
    /// [`SpecError::SampleRate`] when the rate rounds to zero, exceeds
    /// `u32::MAX` Hz.
    pub fn header_rate(&self) -> Result<NonZeroU32> {
        let hz = self.rate.get().round();
        if hz <= f64::from(u32::MAX) {
            // In range: the cast is exact.
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
        match self.layout().count() {
            0 => Err(SpecError::ZeroChannels.into()),
            n => Ok(n),
        }
    }
}
