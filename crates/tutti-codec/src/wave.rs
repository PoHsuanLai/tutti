//! [`Wave`]: a resident, planar, multichannel sample buffer.
//!
//! A buffer only: build it, index it, decode a file into it. Decoding a file
//! *into* a `Wave` is `Wave::read`, behind the `decode-*` features; the buffer
//! itself needs none.

use tutti_types::{ChannelLayout, ChannelTopology};

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
///
/// let mut wave = Wave::new(2, 48_000.0);
/// wave.push_frame(&[0.5, -0.5]);
/// wave.push_frame(&[0.25, -0.25]);
/// assert_eq!(wave.len(), 2);
/// assert_eq!(wave.channel(1), &[-0.5, -0.25]);
/// ```
#[derive(Clone)]
pub struct Wave {
    /// One vector per channel.
    vec: Vec<Vec<f32>>,
    /// The rate the samples were recorded at. Nothing here resamples.
    sample_rate: tutti_types::SampleRate,
    /// Length in frames. 0 if there are no channels.
    len: usize,
    /// Which speaker each channel feeds; its length is the channel count.
    topology: ChannelTopology,
}

/// What a bare channel count is taken to mean: the engine's own order where it
/// has one, else channels that feed no speaker.
fn assumed(channels: usize) -> ChannelTopology {
    let layout = ChannelLayout::try_from(channels).expect("a wave has at most u16::MAX channels");
    ChannelTopology::smpte(layout).unwrap_or_else(|| ChannelTopology::discrete(layout))
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
    pub fn new(channels: usize, sample_rate: impl Into<tutti_types::SampleRate>) -> Self {
        Self::with_capacity(channels, sample_rate, 0)
    }

    /// Creates an empty wave with `channels` channels, each with room for
    /// `capacity` frames.
    ///
    /// # Panics
    ///
    /// If `channels` is above `u16::MAX`, which no layout holds.
    pub fn with_capacity(
        channels: usize,
        sample_rate: impl Into<tutti_types::SampleRate>,
        capacity: usize,
    ) -> Self {
        Self {
            vec: (0..channels)
                .map(|_| Vec::with_capacity(capacity))
                .collect(),
            sample_rate: sample_rate.into(),
            len: 0,
            topology: assumed(channels),
        }
    }

    /// Creates an empty wave whose channels feed the speakers of `topology`,
    /// one channel per speaker.
    ///
    /// [`new`](Self::new) takes a bare count, and a count is not a placement:
    /// it assumes the engine's own order ([`ChannelTopology::smpte`]) for the
    /// widths that have one (1, 2, 6, 8 and 12) and channels that feed no
    /// speaker ([`ChannelTopology::discrete`]) for the rest. Use this when the
    /// source says more.
    pub fn with_topology(
        topology: ChannelTopology,
        sample_rate: impl Into<tutti_types::SampleRate>,
    ) -> Self {
        Self {
            vec: vec![Vec::new(); topology.positions().len()],
            sample_rate: sample_rate.into(),
            len: 0,
            topology,
        }
    }

    /// Returns which speaker each channel feeds.
    ///
    /// A decoded file states its own (see [`Wave::read`]); a wave built from a
    /// count carries the assumption [`new`](Self::new) describes. Its length is
    /// [`channels`](Self::channels).
    #[inline]
    pub fn topology(&self) -> &ChannelTopology {
        &self.topology
    }

    /// Creates an all-zero wave `duration` seconds long, rounded to the
    /// nearest frame.
    ///
    /// `duration` is `f64` rather than [`Seconds`](tutti_types::Seconds)
    /// (an `f32`) so a frame count in a long file stays exact.
    ///
    /// # Panics
    ///
    /// If `channels` is 0 and the duration rounds to at least one frame.
    pub fn zero(
        channels: usize,
        sample_rate: impl Into<tutti_types::SampleRate>,
        duration: f64,
    ) -> Self {
        let sample_rate = sample_rate.into();
        let length = (duration * sample_rate.get()).round() as usize;
        assert!(channels > 0 || length == 0);
        Self {
            vec: vec![vec![0.0; length]; channels],
            sample_rate,
            len: length,
            topology: assumed(channels),
        }
    }

    /// Creates a mono wave holding a copy of `samples`.
    pub fn from_samples(sample_rate: impl Into<tutti_types::SampleRate>, samples: &[f32]) -> Self {
        Self {
            vec: vec![samples.to_vec()],
            sample_rate: sample_rate.into(),
            len: samples.len(),
            topology: assumed(1),
        }
    }

    /// Returns the rate the samples were recorded at.
    #[inline]
    pub fn sample_rate(&self) -> tutti_types::SampleRate {
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
            self.len += 1;
        }
    }

    /// Returns the length in frames.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns whether the wave holds no frames.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns the duration in seconds. `f64` for the reason
    /// [`zero`](Self::zero) gives.
    #[inline]
    pub fn duration(&self) -> f64 {
        self.len as f64 / self.sample_rate.get()
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
    pub(crate) fn set_len(&mut self, len: usize) {
        self.len = len;
    }
}

#[cfg(test)]
mod tests;
