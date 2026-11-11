//! What one render is, validated once: [`Job`].
//!
//! A job is the four figures every render is sized from (its rate, the
//! audible length, the latency trimmed off the head and the tail kept past the
//! end), in frames and nothing else: no seconds, no beats, no tempo. Whoever
//! knows the project's time converts at its own seam and hands a count; the
//! constructor then refuses what no render can be, so the code after it never
//! re-checks. Doc 019 section 4.

use crate::EventLossPolicy;
use tutti_types::{SampleRate, Samples};

/// The most frames one render may span, latency and tail included:
/// `u32::MAX`, which is 6.2 hours at 192 kHz.
///
/// A bound rather than a measure of memory: a render held in memory can fail
/// to allocate well under it ([`Error::RenderTooLarge`](crate::Error::RenderTooLarge)),
/// and one written to a file is under the WAV format's own size limit long
/// before it.
pub const MAX_FRAMES: Samples = Samples(u32::MAX as usize);

/// The frames a render covers and the rate it is rendered at ([`Job::new`]).
///
/// Every field is private; the getters are the reading. A `Job` that exists is
/// one a render can be sized from: the rate is finite and positive, the
/// audible length is not zero and the whole span fits in [`MAX_FRAMES`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Job {
    rate: SampleRate,
    length: Samples,
    latency: Samples,
    tail: Samples,
    on_event_loss: EventLossPolicy,
}

impl Job {
    /// A job of `length` audible frames at `rate`, which drops `latency`
    /// leading frames and keeps `tail` frames past the end.
    ///
    /// `latency` is trimmed from the head, so the graph is pulled for it as
    /// well and the file is `latency` shorter than what the graph produced;
    /// `tail` is kept, so the file is `tail` longer than the audible span.
    /// The event-loss policy is [`EventLossPolicy::Fail`]; see
    /// [`with_event_loss`](Self::with_event_loss).
    ///
    /// # Errors
    ///
    /// - [`JobError::InvalidRate`]: `rate` is NaN, infinite, zero or negative.
    /// - [`JobError::ZeroLength`]: `length` is no frames.
    /// - [`JobError::TooLong`]: `latency + length + tail` overflows, or is more
    ///   than [`MAX_FRAMES`].
    pub fn new(
        rate: SampleRate,
        length: Samples,
        latency: Samples,
        tail: Samples,
    ) -> Result<Self, JobError> {
        if !is_rate(rate) {
            return Err(JobError::InvalidRate { rate });
        }
        if length == Samples(0) {
            return Err(JobError::ZeroLength);
        }
        let total = latency
            .get()
            .checked_add(length.get())
            .and_then(|n| n.checked_add(tail.get()));
        match total.map(Samples) {
            Some(n) if n <= MAX_FRAMES => {}
            total => return Err(JobError::TooLong { total }),
        }
        Ok(Self {
            rate,
            length,
            latency,
            tail,
            on_event_loss: EventLossPolicy::default(),
        })
    }

    /// This job with another answer to a graph that dropped events.
    #[must_use]
    pub fn with_event_loss(self, on_event_loss: EventLossPolicy) -> Self {
        Self {
            on_event_loss,
            ..self
        }
    }

    /// The rate the graph is rendered at.
    pub fn rate(&self) -> SampleRate {
        self.rate
    }

    /// The audible span, in frames. Never zero.
    pub fn length(&self) -> Samples {
        self.length
    }

    /// Leading frames dropped from the render.
    pub fn latency(&self) -> Samples {
        self.latency
    }

    /// Frames kept past the audible span.
    pub fn tail(&self) -> Samples {
        self.tail
    }

    /// What the render does when the graph dropped an event.
    pub fn on_event_loss(&self) -> EventLossPolicy {
        self.on_event_loss
    }

    /// The frames the render keeps: the audible span plus the tail.
    pub fn output_length(&self) -> Samples {
        Samples(self.length.get() + self.tail.get())
    }

    /// The frames the graph is pulled for: what is kept plus the trimmed head.
    pub fn total(&self) -> Samples {
        Samples(self.latency.get() + self.length.get() + self.tail.get())
    }
}

/// Whether `rate` is one a render can run at: finite and positive.
pub(crate) fn is_rate(rate: SampleRate) -> bool {
    rate.get().is_finite() && rate.get() > 0.0
}

/// Why [`Job::new`] refused.
#[derive(thiserror::Error, Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum JobError {
    /// The rate is NaN, infinite, zero or negative.
    #[error("the render rate must be finite and positive, not {rate:?}")]
    InvalidRate {
        /// What was given.
        rate: SampleRate,
    },
    /// The audible span is no frames.
    #[error("the render's length is zero frames")]
    ZeroLength,
    /// Latency, length and tail together overflow a frame count or pass
    /// [`MAX_FRAMES`].
    #[error("the render spans {total:?} frames, more than the {MAX_FRAMES:?} a render may")]
    TooLong {
        /// The sum, or `None` where it overflowed a frame count.
        total: Option<Samples>,
    },
}

#[cfg(test)]
mod tests;
