//! Fluent builder for clip-style [`SamplerUnit`] playback from a file path.
//!
//! The builder decodes the audio file (WAV / FLAC / MP3 / OGG Vorbis) into
//! a [`Wave`](tutti_core::Wave), wraps it in an `Arc`, and configures a
//! [`SamplerUnit`]. Optionally binds the unit to a
//! [`TransportHandle`](tutti_core::TransportHandle) so clips play only
//! while the playhead is inside a beat range.

use crate::error::{Error, Result};
use crate::units::SamplerUnit;
use std::path::PathBuf;
use std::sync::Arc;
use tutti_core::{TransportHandle, Wave};

/// Starts a [`SampleBuilder`] for a WAV file.
///
/// ```ignore
/// let unit = tutti_sampler::wav("kick.wav")
///     .gain(0.8)
///     .build()?;
/// ```
#[cfg(feature = "wav")]
pub fn wav(path: impl Into<PathBuf>) -> SampleBuilder<'static> {
    SampleBuilder::new(path.into())
}

/// Starts a [`SampleBuilder`] for a FLAC file.
#[cfg(feature = "flac")]
pub fn flac(path: impl Into<PathBuf>) -> SampleBuilder<'static> {
    SampleBuilder::new(path.into())
}

/// Starts a [`SampleBuilder`] for an MP3 file.
#[cfg(feature = "mp3")]
pub fn mp3(path: impl Into<PathBuf>) -> SampleBuilder<'static> {
    SampleBuilder::new(path.into())
}

/// Starts a [`SampleBuilder`] for an Ogg Vorbis file.
#[cfg(feature = "ogg")]
pub fn ogg(path: impl Into<PathBuf>) -> SampleBuilder<'static> {
    SampleBuilder::new(path.into())
}

/// Fluent builder for clip-style sample playback.
///
/// Construct via [`wav`], [`flac`], [`mp3`], or [`ogg`]. Bind to a
/// [`TransportHandle`] with [`SampleBuilder::on_transport`] to gate
/// playback by beat range; otherwise the clip plays free-running.
pub struct SampleBuilder<'a> {
    path: PathBuf,
    transport: Option<&'a TransportHandle>,
    gain: f32,
    speed: f32,
    looping: bool,
    start_beat: Option<f64>,
    duration_beats: Option<f64>,
}

impl<'a> SampleBuilder<'a> {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            transport: None,
            gain: 1.0,
            speed: 1.0,
            looping: false,
            start_beat: None,
            duration_beats: None,
        }
    }

    /// Playback gain as a linear multiplier.
    ///
    /// Default: `1.0`.
    pub fn gain(mut self, gain: f32) -> Self {
        self.gain = gain;
        self
    }

    /// Playback speed multiplier; also transposes pitch.
    ///
    /// Default: `1.0`.
    pub fn speed(mut self, speed: f32) -> Self {
        self.speed = speed;
        self
    }

    /// Loops the sample when it reaches the end instead of stopping.
    ///
    /// Default: `false`.
    pub fn looping(mut self, looping: bool) -> Self {
        self.looping = looping;
        self
    }

    /// Binds the clip to a transport so it only plays while the playhead is
    /// inside the configured beat range. Required before
    /// [`Self::start_beat`] or [`Self::duration_beats`] take effect.
    pub fn on_transport(mut self, transport: &'a TransportHandle) -> Self {
        self.transport = Some(transport);
        self
    }

    /// Places the clip at the given beat position on the bound transport.
    /// Has no effect unless [`Self::on_transport`] was also set.
    pub fn start_beat(mut self, beat: f64) -> Self {
        self.start_beat = Some(beat);
        self
    }

    /// Limits transport-aware playback to this many beats.
    ///
    /// Default: the clip's full length.
    pub fn duration_beats(mut self, beats: f64) -> Self {
        self.duration_beats = Some(beats);
        self
    }

    /// Decodes the audio file synchronously and builds a [`SamplerUnit`].
    ///
    /// No decoded-buffer caching is performed — re-use is the caller's
    /// responsibility. Returns [`Error::SampleNotFound`] wrapping the
    /// underlying decode error if the file can't be loaded.
    pub fn build(self) -> Result<SamplerUnit> {
        let wave = Wave::load_with_progress(&self.path, |_| {})
            .map_err(|e| Error::SampleNotFound(format!("{}: {}", self.path.display(), e)))?;
        let wave = Arc::new(wave);
        let mut unit = SamplerUnit::with_settings(wave, self.gain, self.speed, self.looping);
        if let (Some(start), Some(transport)) = (self.start_beat, self.transport) {
            unit.set_transport(Arc::new(transport.clone()), start, self.duration_beats);
        }
        Ok(unit)
    }
}
