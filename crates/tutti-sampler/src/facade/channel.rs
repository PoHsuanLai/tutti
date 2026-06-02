//! Per-channel playback handle. See [`Channel`].

use super::builders::PlayBuilder;
use super::system::Sampler;
use crate::butler::{ButlerCommand, PlayDirection, Varispeed};
use std::ops::Range;
use std::path::PathBuf;

/// Operations on a single streaming channel.
///
/// Obtained from [`Sampler::channel`]. The handle borrows the sampler
/// and is `Copy`, and most methods return `Self` so calls chain
/// fluently. Both [`play`](Self::play) and the per-channel mutators
/// ([`seek`](Self::seek), [`speed`](Self::speed), [`set_loop`](Self::set_loop), …)
/// can be linked in one expression. Only [`stop`](Self::stop) is a
/// terminator — it consumes the handle without returning.
///
/// # Example
///
/// ```no_run
/// # use tutti_sampler::Sampler;
/// # let sampler = Sampler::builder(48_000.0).build().unwrap();
/// // Configure and start in one chain
/// sampler.channel(0)
///     .play("clip.wav")
///     .speed(1.5)
///     .start()                  // returns the Channel for further tweaks
///     .set_loop(0..88_200)
///     .seek(44_100);
///
/// // Tear down
/// sampler.channel(0).stop();
/// ```
#[derive(Copy, Clone)]
pub struct Channel<'a> {
    sampler: &'a Sampler,
    index: usize,
}

impl<'a> Channel<'a> {
    pub(crate) fn new(sampler: &'a Sampler, index: usize) -> Self {
        Self { sampler, index }
    }

    /// The channel index this handle targets.
    pub fn index(&self) -> usize {
        self.index
    }

    /// Begin a playback configuration for this channel.
    ///
    /// Returns a [`PlayBuilder`] — chain configuration calls and finish
    /// with [`PlayBuilder::start`].
    ///
    /// ```no_run
    /// # use tutti_sampler::Sampler;
    /// # let sampler = Sampler::builder(48_000.0).build().unwrap();
    /// sampler.channel(0)
    ///     .play("clip.wav")
    ///     .offset_samples(44_100)
    ///     .speed(1.25)
    ///     .start();
    /// ```
    pub fn play(self, file_path: impl Into<PathBuf>) -> PlayBuilder<'a> {
        PlayBuilder::new(self.sampler, self.index, file_path)
    }

    /// Stop streaming on this channel. Terminator — does not chain.
    pub fn stop(self) {
        self.sampler.send(ButlerCommand::StopStreaming {
            channel_index: self.index,
        });
    }

    /// Seek to an absolute sample position. Returns the channel handle
    /// so further operations can chain.
    pub fn seek(self, position_samples: u64) -> Self {
        self.sampler.send(ButlerCommand::SeekStream {
            channel_index: self.index,
            position_samples,
        });
        self
    }

    /// Set playback speed.
    ///
    /// `1.0` is normal, `0.5` is half-speed, `2.0` is double-speed.
    /// Negative values play in reverse — equivalent to a forward
    /// [`varispeed`](Self::varispeed) with [`PlayDirection::Reverse`].
    pub fn speed(self, factor: f32) -> Self {
        let direction = if factor < 0.0 {
            PlayDirection::Reverse
        } else {
            PlayDirection::Forward
        };
        self.sampler.send(ButlerCommand::SetVarispeed {
            channel_index: self.index,
            direction,
            speed: factor.abs(),
        });
        self
    }

    /// Set both direction and speed in one call.
    ///
    /// Use this when you have an existing [`Varispeed`] value; otherwise
    /// [`speed`](Self::speed) is usually shorter.
    pub fn varispeed(self, varispeed: Varispeed) -> Self {
        self.sampler.send(ButlerCommand::SetVarispeed {
            channel_index: self.index,
            direction: varispeed.direction,
            speed: varispeed.speed,
        });
        self
    }

    /// Loop the given sample range, no crossfade.
    ///
    /// Equivalent to `set_loop_with_crossfade(range, 0)`.
    ///
    /// ```no_run
    /// # use tutti_sampler::Sampler;
    /// # let sampler = Sampler::builder(48_000.0).build().unwrap();
    /// sampler.channel(0).set_loop(0..88_200);
    /// ```
    pub fn set_loop(self, samples: Range<u64>) -> Self {
        self.set_loop_with_crossfade(samples, 0)
    }

    /// Loop the given sample range with a sample-level crossfade across
    /// the wrap point.
    ///
    /// `crossfade_samples` controls the fade length; pass `0` for a hard
    /// loop boundary.
    pub fn set_loop_with_crossfade(self, samples: Range<u64>, crossfade_samples: usize) -> Self {
        self.sampler.send(ButlerCommand::SetLoopRange {
            channel_index: self.index,
            start_samples: samples.start,
            end_samples: samples.end,
            crossfade_samples,
        });
        self
    }

    /// Disable looping on this channel.
    pub fn clear_loop(self) -> Self {
        self.sampler.send(ButlerCommand::ClearLoopRange {
            channel_index: self.index,
        });
        self
    }

    /// `true` if this channel is currently streaming a file.
    pub fn is_streaming(&self) -> bool {
        self.sampler
            .butler_plans()
            .get(&self.index)
            .is_some_and(|s| s.link.is_some())
    }

    /// Get a [`StreamingSamplerUnit`](crate::StreamingSamplerUnit) for
    /// inserting this channel's audio into a FunDSP graph.
    ///
    /// Returns `None` if the channel isn't currently streaming. The unit
    /// automatically receives varispeed and seek updates from the butler.
    pub fn streaming_unit(&self) -> Option<crate::StreamingSamplerUnit> {
        let plans = self.sampler.butler_plans();
        let stream_state = plans.get(&self.index)?;
        let consumer = stream_state.link.as_ref().map(|l| l.consumer.clone())?;
        Some(crate::StreamingSamplerUnit::new(
            consumer,
            stream_state.rt_state(),
        ))
    }
}
