//! Fluent builders for streaming playback ([`PlayBuilder`]) and recording
//! capture ([`RecordBuilder`] → [`CaptureSession`]).

use super::system::Sampler;
use crate::butler::{ButlerCommand, CaptureBuffer, CaptureId, CaptureWriter, PlayDirection};
use std::path::{Path, PathBuf};

/// Configures and starts streaming playback on a channel.
///
/// Created by [`Channel::play`](super::channel::Channel::play).
/// All setters are optional; defaults match the most common usage:
/// forward, full speed, no offset, no loop.
///
/// # Example
///
/// ```no_run
/// # use tutti_sampler::Sampler;
/// # let sampler = Sampler::builder(48_000.0).build().unwrap();
/// sampler.channel(0)
///     .play("clip.wav")
///     .offset_samples(44_100)
///     .loop_samples(0, 88_200)
///     .crossfade_samples(256)
///     .speed(1.5)
///     .start();
/// ```
pub struct PlayBuilder<'a> {
    sampler: &'a Sampler,
    file_path: PathBuf,
    channel: usize,
    offset_samples: usize,
    loop_range: Option<(u64, u64)>,
    crossfade_samples: usize,
    direction: PlayDirection,
    speed: f32,
}

impl<'a> PlayBuilder<'a> {
    pub(crate) fn new(sampler: &'a Sampler, channel: usize, file_path: impl Into<PathBuf>) -> Self {
        Self {
            sampler,
            file_path: file_path.into(),
            channel,
            offset_samples: 0,
            loop_range: None,
            crossfade_samples: 0,
            direction: PlayDirection::Forward,
            speed: 1.0,
        }
    }

    /// Begin playback at this sample offset rather than the file's start.
    pub fn offset_samples(mut self, offset: usize) -> Self {
        self.offset_samples = offset;
        self
    }

    /// Configure a loop range, in samples. Without crossfade unless paired
    /// with [`crossfade_samples`](Self::crossfade_samples).
    pub fn loop_samples(mut self, start: u64, end: u64) -> Self {
        self.loop_range = Some((start, end));
        self
    }

    /// Crossfade length applied at the loop boundary. Default: `0`.
    pub fn crossfade_samples(mut self, samples: usize) -> Self {
        self.crossfade_samples = samples;
        self
    }

    /// Play the file in reverse.
    pub fn reverse(mut self) -> Self {
        self.direction = PlayDirection::Reverse;
        self
    }

    /// Set playback speed.
    ///
    /// `1.0` is normal, `0.5` half, `2.0` double. Negative values play
    /// in reverse — equivalent to [`reverse`](Self::reverse) plus
    /// `.speed(speed.abs())`.
    pub fn speed(mut self, speed: f32) -> Self {
        if speed < 0.0 {
            self.direction = PlayDirection::Reverse;
            self.speed = speed.abs();
        } else {
            self.speed = speed;
        }
        self
    }

    /// Start streaming with the configured options. Returns the
    /// per-channel handle so post-start tweaks (`seek`, `speed`,
    /// `set_loop`, …) can chain.
    pub fn start(self) -> super::channel::Channel<'a> {
        self.sampler.send(ButlerCommand::StreamAudioFile {
            channel_index: self.channel,
            file_path: self.file_path,
            offset_samples: self.offset_samples,
        });

        if let Some((start, end)) = self.loop_range {
            self.sampler.send(ButlerCommand::SetLoopRange {
                channel_index: self.channel,
                start_samples: start,
                end_samples: end,
                crossfade_samples: self.crossfade_samples,
            });
        }

        // Only emit a varispeed command when the user actually changed
        // direction or speed; the butler defaults to forward / 1.0.
        let needs_varispeed = self.direction != PlayDirection::Forward || self.speed != 1.0;
        if needs_varispeed {
            self.sampler.send(ButlerCommand::SetVarispeed {
                channel_index: self.channel,
                direction: self.direction,
                speed: self.speed,
            });
        }

        self.sampler.channel(self.channel)
    }
}

/// Configures and starts a capture session.
///
/// Created by [`Sampler::record`](super::system::Sampler::record).
/// All setters are optional; defaults are stereo, 5-second ring buffer,
/// system sample rate.
///
/// # Example
///
/// ```no_run
/// # use tutti_sampler::Sampler;
/// # let sampler = Sampler::builder(48_000.0).build().unwrap();
/// let mut session = sampler.record("out.wav")
///     .channels(2)
///     .buffer_seconds(5.0)
///     .start();
///
/// // ... audio callback writes via session.producer_mut() ...
///
/// session.stop();
/// ```
pub struct RecordBuilder<'a> {
    sampler: &'a Sampler,
    file_path: PathBuf,
    channels: usize,
    buffer_seconds: Option<f64>,
    sample_rate: Option<f64>,
}

impl<'a> RecordBuilder<'a> {
    pub(crate) fn new(sampler: &'a Sampler, file_path: impl Into<PathBuf>) -> Self {
        Self {
            sampler,
            file_path: file_path.into(),
            channels: 2,
            buffer_seconds: None,
            sample_rate: None,
        }
    }

    /// Number of channels in the captured file. Default: `2`.
    pub fn channels(mut self, channels: usize) -> Self {
        self.channels = channels;
        self
    }

    /// Ring-buffer length in seconds. Default: `5.0`.
    ///
    /// Larger buffers tolerate more disk-write jitter at the cost of memory.
    pub fn buffer_seconds(mut self, seconds: f64) -> Self {
        self.buffer_seconds = Some(seconds);
        self
    }

    /// Override the capture sample rate. Default: the system sample rate
    /// passed to [`Sampler::builder`](super::system::Sampler::builder).
    pub fn sample_rate(mut self, rate: f64) -> Self {
        self.sample_rate = Some(rate);
        self
    }

    /// Begin recording and return a live [`CaptureSession`].
    ///
    /// Sends a blocking `RegisterCapture` command so the capture is live
    /// before this returns. Audio data flows through the session's
    /// [`producer_mut`](CaptureSession::producer_mut); the butler thread
    /// drains the ring buffer and writes to disk asynchronously.
    pub fn start(self) -> CaptureSession<'a> {
        let sample_rate = self
            .sample_rate
            .unwrap_or_else(|| self.sampler.sample_rate());
        let buffer_ms = self.buffer_seconds.unwrap_or(5.0) * 1000.0;
        let id = self.sampler.mint_capture_id();
        let (producer, consumer) =
            CaptureBuffer::new(self.file_path.clone(), sample_rate, buffer_ms as f32);

        // Register with butler — blocking send so the capture is live before we return.
        self.sampler.send(ButlerCommand::RegisterCapture {
            capture_id: id,
            consumer,
            file_path: self.file_path.clone(),
            sample_rate,
            channels: self.channels,
        });

        CaptureSession {
            id,
            producer,
            sampler: self.sampler,
            file_path: self.file_path,
            sample_rate,
            channels: self.channels,
            stopped: false,
        }
    }
}

/// A live recording session.
///
/// Yielded by [`RecordBuilder::start`]. The audio callback writes samples
/// via [`producer_mut`](Self::producer_mut); the butler thread drains the
/// ring buffer and writes the file to disk.
///
/// Calling [`stop`](Self::stop) — or letting the session drop — flushes
/// remaining bytes and closes the file. Both are idempotent.
pub struct CaptureSession<'a> {
    /// Identifier butler uses to route commands for this session.
    pub id: CaptureId,
    producer: CaptureWriter,
    sampler: &'a Sampler,
    file_path: PathBuf,
    sample_rate: f64,
    channels: usize,
    stopped: bool,
}

impl<'a> CaptureSession<'a> {
    /// Path the recorded WAV is being written to.
    pub fn file_path(&self) -> &Path {
        &self.file_path
    }

    /// Sample rate the session was configured with.
    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    /// Channel count the session was configured with.
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Read-only producer (for fullness inspection, etc.).
    pub fn producer(&self) -> &CaptureWriter {
        &self.producer
    }

    /// Mutable producer the audio callback writes samples into.
    pub fn producer_mut(&mut self) -> &mut CaptureWriter {
        &mut self.producer
    }

    /// Manually flush pending bytes to disk.
    ///
    /// Not normally needed — butler flushes automatically when the ring
    /// buffer reaches its configured threshold. Useful right before
    /// [`stop`](Self::stop) if you want the file on disk before
    /// inspecting it; chains naturally as `session.flush().stop()`.
    pub fn flush(&self) -> &Self {
        self.sampler.send(ButlerCommand::Flush(self.id));
        self
    }

    /// Finalize the session: flush remaining bytes, remove the capture
    /// from butler, and close the file. Idempotent.
    pub fn stop(mut self) {
        self.stop_inner();
    }

    fn stop_inner(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        self.sampler.send(ButlerCommand::Flush(self.id));
        self.sampler.send(ButlerCommand::RemoveCapture(self.id));
    }
}

impl<'a> Drop for CaptureSession<'a> {
    /// Equivalent to calling [`stop`](Self::stop) — finalizes the file
    /// even if the user forgot.
    fn drop(&mut self) {
        self.stop_inner();
    }
}
