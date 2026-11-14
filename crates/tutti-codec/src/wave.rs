//! [`Wave`]: a resident, planar, multichannel sample buffer.
//!
//! A buffer: build it, index it, decode a file into it, write it to a file.
//! Decoding a file *into* a `Wave` is `Wave::read`, behind the `decode-*`
//! features, and writing one is `Wave::write`, behind the `encode-*`
//! features; the buffer itself needs none.

#[cfg(encode)]
use crate::{Format, Spec};
#[cfg(encode)]
use std::path::Path;
#[cfg(encode)]
use tutti_buffers::Interleaved;
use tutti_types::Samples;
use tutti_types::{ChannelLayout, ChannelTopology, SampleRate};

/// Frames written to the encoder at a time by [`Wave::write`].
#[cfg(encode)]
const WRITE_CHUNK: usize = 8192;

/// Multichannel audio held in memory, one `Vec<f32>` per channel.
///
/// What a sampler voice plays and what `Wave::read` decodes a file into
/// (with a decode feature). Planar rather than interleaved because its
/// consumers index it by `(channel, frame)`. Every channel has the same
/// length, [`len`](Self::len). The sample rate is carried as data; nothing
/// here resamples.
///
/// Indexing methods ([`channel`](Self::channel), [`at`](Self::at),
/// [`set`](Self::set), …) panic on an out-of-range channel or frame, like
/// slice indexing.
///
/// # Examples
///
/// ```
/// use tutti_codec::Wave;
/// use tutti_types::SampleRate;
///
/// let mut wave = Wave::new(2, SampleRate::SR_48K);
/// wave.push_frame(&[0.5, -0.5]);
/// wave.push_frame(&[0.25, -0.25]);
/// assert_eq!(wave.len(), tutti_types::Samples(2));
/// assert_eq!(wave.channel(1), &[-0.5, -0.25]);
/// ```
#[derive(Clone, PartialEq)]
pub struct Wave {
    /// One vector per channel.
    vec: Vec<Vec<f32>>,
    /// The rate the samples were recorded at. Nothing here resamples.
    sample_rate: SampleRate,
    /// Length in frames. 0 if there are no channels.
    len: Samples,
    /// Which speaker each channel feeds; its length is the channel count.
    topology: ChannelTopology,
}

/// What a bare channel count is taken to mean: the engine's own order where it
/// has one, else channels that feed no speaker.
fn assumed(channels: usize) -> ChannelTopology {
    let layout = ChannelLayout::try_from(channels).expect("a wave has at most u16::MAX channels");
    ChannelTopology::smpte(layout).unwrap_or_else(|| ChannelTopology::unassigned(layout))
}

impl std::fmt::Debug for Wave {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Wave")
            .field("sample_rate", &self.sample_rate)
            .field("len", &self.len)
            .finish_non_exhaustive()
    }
}

impl Wave {
    /// Creates an empty wave with `channels` channels.
    ///
    /// # Panics
    ///
    /// If `channels` is above `u16::MAX`, which no layout holds.
    pub fn new(channels: usize, sample_rate: SampleRate) -> Self {
        Self::with_capacity(channels, sample_rate, Samples::ZERO)
    }

    /// Creates an empty wave with `channels` channels, each with room for
    /// `capacity` frames.
    ///
    /// # Panics
    ///
    /// If `channels` is above `u16::MAX`, which no layout holds.
    pub fn with_capacity(channels: usize, sample_rate: SampleRate, capacity: Samples) -> Self {
        Self {
            vec: (0..channels)
                .map(|_| Vec::with_capacity(capacity.get()))
                .collect(),
            sample_rate,
            len: Samples::ZERO,
            topology: assumed(channels),
        }
    }

    /// Creates an empty wave whose channels feed the speakers of `topology`,
    /// one channel per speaker.
    ///
    /// [`new`](Self::new) takes a bare count, and a count is not a placement:
    /// it assumes the engine's own order ([`ChannelTopology::smpte`]) for the
    /// widths that have one (1, 2, 6, 8 and 12) and channels that feed no
    /// speaker ([`ChannelTopology::unassigned`]) for the rest. Use this when the
    /// source says more.
    pub fn with_topology(topology: ChannelTopology, sample_rate: SampleRate) -> Self {
        Self {
            vec: vec![Vec::new(); topology.positions().len()],
            sample_rate,
            len: Samples::ZERO,
            topology,
        }
    }

    /// Returns which speaker each channel feeds.
    ///
    /// A decoded file states its own (see `Wave::read`); a wave built from a
    /// count carries the assumption [`new`](Self::new) describes. Its length is
    /// [`channels`](Self::channels).
    #[inline]
    pub fn topology(&self) -> &ChannelTopology {
        &self.topology
    }

    /// Returns the width, as a [`ChannelLayout`]: the length of the
    /// [`topology`](Self::topology).
    #[inline]
    pub fn layout(&self) -> ChannelLayout {
        self.topology.layout()
    }

    /// Creates an all-zero wave `duration` seconds long, rounded to the
    /// nearest frame.
    ///
    /// A [`Seconds`](tutti_types::Seconds) is an `f64`, so a frame count in a
    /// long file stays exact.
    ///
    /// # Panics
    ///
    /// If `channels` is 0 and the duration rounds to at least one frame.
    pub fn zero(
        channels: usize,
        sample_rate: SampleRate,
        duration: impl Into<tutti_types::Seconds>,
    ) -> Self {
        let length = (duration.into().get() * sample_rate.get()).round() as usize;
        assert!(channels > 0 || length == 0);
        Self {
            vec: vec![vec![0.0; length]; channels],
            sample_rate,
            len: Samples(length),
            topology: assumed(channels),
        }
    }

    /// Creates a mono wave holding a copy of `samples`.
    pub fn from_samples(sample_rate: SampleRate, samples: &[f32]) -> Self {
        Self {
            vec: vec![samples.to_vec()],
            sample_rate,
            len: Samples(samples.len()),
            topology: assumed(1),
        }
    }

    /// Returns the rate the samples were recorded at.
    #[inline]
    pub fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }

    /// Returns the number of channels.
    #[inline]
    pub fn channels(&self) -> usize {
        self.vec.len()
    }

    /// Returns one channel's samples.
    #[inline]
    pub fn channel(&self, channel: usize) -> &[f32] {
        &self.vec[channel]
    }

    /// Returns one channel's samples, mutably. The length cannot change
    /// through this.
    #[inline]
    pub fn channel_mut(&mut self, channel: usize) -> &mut [f32] {
        &mut self.vec[channel]
    }

    /// Returns the sample at frame `index` of `channel`.
    #[inline]
    pub fn at(&self, channel: usize, index: usize) -> f32 {
        self.vec[channel][index]
    }

    /// Overwrites the sample at frame `index` of `channel`.
    #[inline]
    pub fn set(&mut self, channel: usize, index: usize, value: f32) {
        self.vec[channel][index] = value;
    }

    /// Appends one frame; `frame` holds one sample per channel.
    ///
    /// # Panics
    ///
    /// If `frame.len()` differs from [`channels`](Self::channels).
    pub fn push_frame(&mut self, frame: &[f32]) {
        assert_eq!(frame.len(), self.channels(), "frame width != wave width");
        for (channel, &s) in self.vec.iter_mut().zip(frame) {
            channel.push(s);
        }
        if !self.vec.is_empty() {
            self.len += Samples(1);
        }
    }

    /// Returns the length in frames.
    #[inline]
    #[must_use]
    pub fn len(&self) -> Samples {
        self.len
    }

    /// Returns whether the wave holds no frames.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == Samples::ZERO
    }

    /// Returns the duration, exact to the frame for the reason
    /// [`zero`](Self::zero) gives.
    #[inline]
    pub fn duration(&self) -> tutti_types::Seconds {
        tutti_types::Seconds(self.len.get() as f64 / self.sample_rate.get())
    }

    /// Returns every sample frame by frame: channel 0 to the last of frame 0,
    /// then frame 1, and so on, the order of an
    /// [interleaved](tutti_buffers::Interleaved) block.
    ///
    /// An iterator and not a `Vec`: a wave is already held whole, and the
    /// caller that wants flat samples (an encoder, a meter) reads them in
    /// chunks instead of doubling the memory.
    pub fn interleaved(&self) -> impl Iterator<Item = f32> + '_ {
        (0..self.len.get()).flat_map(|frame| self.vec.iter().map(move |channel| channel[frame]))
    }

    /// Writes the wave as a file of `format` at `path`, at its own rate and
    /// with its own topology (the speaker mask of a WAV, the channel order of
    /// an Ogg Vorbis file).
    ///
    /// The file is written beside `path` and renamed over it when it is
    /// whole, as [`Encoder::create_file`](crate::Encoder::create_file) does,
    /// so a failure leaves what was at `path` as it was. Nothing is
    /// resampled, dithered or normalized: the samples go as they are, and an
    /// integer width quantizes them with the format's own rounding.
    ///
    /// # Errors
    ///
    /// [`Error::Spec`](crate::Error::Spec) for a wave the format cannot hold
    /// (no channels, a rate no header states, a width past the format's
    /// limit), [`Error::WavTooLarge`](crate::Error::WavTooLarge) for a WAV too
    /// long for its 32-bit sizes, and the encoder's other errors.
    ///
    /// # Examples
    ///
    /// ```
    /// # #[cfg(all(feature = "encode-wav", feature = "decode-wav"))] {
    /// use tutti_codec::{Format, Pcm, Wave};
    /// use tutti_types::SampleRate;
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let dir = tempfile::tempdir()?;
    /// let path = dir.path().join("ramp.wav");
    /// let wave = Wave::from_samples(SampleRate::SR_48K, &[0.0, 0.25, 0.5, 0.75]);
    /// wave.write(&path, Format::Wav(Pcm::Float32))?;
    /// assert_eq!(Wave::read(&path)?.channel(0), wave.channel(0));
    /// # Ok(())
    /// # }
    /// # }
    /// ```
    #[cfg(encode)]
    pub fn write(&self, path: impl AsRef<Path>, format: Format) -> crate::Result<()> {
        let spec = Spec::new(format, self.topology.clone(), self.sample_rate);
        let mut encoder = crate::Encoder::create_file_planned(&spec, path, self.len)?;
        let layout = self.layout();
        let mut flat = Vec::with_capacity(WRITE_CHUNK * usize::from(layout.count()));
        let mut samples = self.interleaved();
        loop {
            flat.clear();
            flat.extend(
                samples
                    .by_ref()
                    .take(WRITE_CHUNK * usize::from(layout.count())),
            );
            if flat.is_empty() {
                break;
            }
            encoder.write(Interleaved::new(&flat, layout)?)?;
        }
        encoder.finish().map(drop)
    }

    /// The backing vector of one channel, for the decoder's batch append.
    /// The caller must follow with [`set_len`](Self::set_len).
    #[cfg(decode)]
    #[inline]
    pub(crate) fn channel_vec_mut(&mut self, channel: usize) -> &mut Vec<f32> {
        &mut self.vec[channel]
    }

    /// Set the length in frames after a batch append. Every channel must
    /// already hold at least this many samples.
    #[cfg(decode)]
    #[inline]
    pub(crate) fn set_len(&mut self, len: Samples) {
        self.len = len;
    }
}

#[cfg(test)]
mod tests;
