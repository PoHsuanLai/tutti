//! [`Wave`]: a resident, planar, multichannel sample buffer.
//!
//! Moved here from the fundsp fork (`fundsp-tutti/src/wave.rs`) by design doc
//! 013, Phase 0. The engine used the fork's type only as a buffer — build it,
//! index it, decode a file into it — so what came across is that buffer and
//! nothing else. The fork's rendering and filtering (`render*`, `filter*`,
//! `multifilter*`, `resample_fir`), its editing helpers (`fade*`, `normalize`,
//! `retain`, `append`, `mix*`, channel insert/remove) and its typenum-frame
//! `push` had no caller outside the fork, and the fork keeps its own copy for
//! its internal nodes (`playwave`, `convolve`, its WAV writer).
//!
//! Decoding a file *into* a `Wave` is in [`decode`](crate::Wave::load), behind
//! the codec features; the buffer itself needs none.

/// Multichannel audio held in memory, one `Vec<f32>` per channel.
///
/// Planar rather than interleaved because its consumers index it by
/// `(channel, frame)` — a sampler voice reads one channel at a time, and a
/// decoder appends each channel's packet slice in one `extend_from_slice`.
/// Every channel has the same length, [`len`](Self::len).
#[derive(Clone)]
pub struct Wave {
    /// One vector per channel.
    vec: Vec<Vec<f32>>,
    /// The rate the samples were recorded at. Nothing here resamples.
    sample_rate: tutti_core::SampleRate,
    /// Length in frames. 0 if there are no channels.
    len: usize,
}

impl Wave {
    /// An empty wave with `channels` channels.
    pub fn new(channels: usize, sample_rate: impl Into<tutti_core::SampleRate>) -> Self {
        Self::with_capacity(channels, sample_rate, 0)
    }

    /// An empty wave with `channels` channels, each with room for `capacity`
    /// frames.
    pub fn with_capacity(
        channels: usize,
        sample_rate: impl Into<tutti_core::SampleRate>,
        capacity: usize,
    ) -> Self {
        Self {
            vec: (0..channels)
                .map(|_| Vec::with_capacity(capacity))
                .collect(),
            sample_rate: sample_rate.into(),
            len: 0,
        }
    }

    /// An all-zero wave `duration` seconds long, rounded to the nearest frame.
    ///
    /// `duration` stays `f64` rather than [`Seconds`](tutti_core::Seconds):
    /// `Seconds` is `f32`, which cannot place a frame exactly in a long file,
    /// and this rounds `duration * rate` to a frame count.
    pub fn zero(
        channels: usize,
        sample_rate: impl Into<tutti_core::SampleRate>,
        duration: f64,
    ) -> Self {
        let sample_rate = sample_rate.into();
        let length = (duration * sample_rate.get()).round() as usize;
        assert!(channels > 0 || length == 0);
        Self {
            vec: vec![vec![0.0; length]; channels],
            sample_rate,
            len: length,
        }
    }

    /// A mono wave holding `samples`.
    pub fn from_samples(sample_rate: impl Into<tutti_core::SampleRate>, samples: &[f32]) -> Self {
        Self {
            vec: vec![samples.to_vec()],
            sample_rate: sample_rate.into(),
            len: samples.len(),
        }
    }

    /// The rate the samples were recorded at.
    #[inline]
    pub fn sample_rate(&self) -> tutti_core::SampleRate {
        self.sample_rate
    }

    /// Number of channels.
    #[inline]
    pub fn channels(&self) -> usize {
        self.vec.len()
    }

    /// One channel's samples.
    #[inline]
    pub fn channel(&self, channel: usize) -> &[f32] {
        &self.vec[channel]
    }

    /// One channel's samples, mutably. The length cannot change through this.
    #[inline]
    pub fn channel_mut(&mut self, channel: usize) -> &mut [f32] {
        &mut self.vec[channel]
    }

    /// The sample at `(channel, index)`.
    #[inline]
    pub fn at(&self, channel: usize, index: usize) -> f32 {
        self.vec[channel][index]
    }

    /// Overwrite the sample at `(channel, index)`.
    #[inline]
    pub fn set(&mut self, channel: usize, index: usize, value: f32) {
        self.vec[channel][index] = value;
    }

    /// Append one frame. `frame` holds one sample per channel.
    ///
    /// Replaces the fork's `push<T: ConstantFrame>`, which took a typenum
    /// frame (a tuple, or a scalar broadcast to every channel). The width is
    /// a runtime property here, so the frame is a runtime-length slice and a
    /// mismatch is a panic rather than a type error.
    pub fn push_frame(&mut self, frame: &[f32]) {
        assert_eq!(frame.len(), self.channels(), "frame width != wave width");
        for (channel, &s) in self.vec.iter_mut().zip(frame) {
            channel.push(s);
        }
        if !self.vec.is_empty() {
            self.len += 1;
        }
    }

    /// Length in frames.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the wave holds no frames.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Duration in seconds. `f64` for the reason [`zero`](Self::zero) gives.
    #[inline]
    pub fn duration(&self) -> f64 {
        self.len as f64 / self.sample_rate.get()
    }

    /// The backing vector of one channel, for the decoder's batch append.
    /// The caller must follow with [`set_len`](Self::set_len).
    #[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
    #[inline]
    pub(crate) fn channel_vec_mut(&mut self, channel: usize) -> &mut Vec<f32> {
        &mut self.vec[channel]
    }

    /// Set the length in frames after a batch append. Every channel must
    /// already hold at least this many samples.
    #[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
    #[inline]
    pub(crate) fn set_len(&mut self, len: usize) {
        self.len = len;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `push_frame` appends to every channel and advances the length once.
    ///
    /// Mutation: advancing `len` per channel inside the loop makes `len()` 2
    /// here and fails.
    #[test]
    fn push_frame_appends_one_frame_across_channels() {
        let mut w = Wave::new(2, 48_000.0);
        w.push_frame(&[0.25, -0.5]);
        assert_eq!(w.len(), 1);
        assert_eq!((w.at(0, 0), w.at(1, 0)), (0.25, -0.5));
    }

    /// Mutation: dropping the width assert lets a mono frame into a stereo
    /// wave, leaving channel 1 one sample short, and this stops panicking.
    #[test]
    #[should_panic(expected = "frame width")]
    fn push_frame_rejects_a_frame_of_the_wrong_width() {
        Wave::new(2, 48_000.0).push_frame(&[0.5]);
    }

    /// Mutation: `round()` → `floor()` in `zero` makes 0.5 frames 0 here.
    #[test]
    fn zero_rounds_the_duration_to_the_nearest_frame() {
        let w = Wave::zero(1, 2.0, 0.75);
        assert_eq!(w.len(), 2);
        assert!(w.channel(0).iter().all(|&s| s == 0.0));
    }
}
