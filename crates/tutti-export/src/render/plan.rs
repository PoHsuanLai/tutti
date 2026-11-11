//! Frame-count arithmetic for one offline render.
//!
//! Derived once from (duration, rate, latency), and drives both how many frames
//! the graph must produce and how many leading frames the sink drops.

use crate::config::RenderConfig;
use crate::{ConfigError, Job, JobError};
use tutti_types::{BeatDuration, Bpm, Samples};
use tutti_types::{SampleRate, Seconds};

/// Fixed scheduling parameters for one render.
///
/// Every field is [`Samples`] — a discrete frame count, which is what the type
/// exists for (its own doc names the compensation-delay path). Note that
/// `Samples` has no `Sub`: use `remaining_after` / `align_to`, which is why the
/// driver reads the way it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RenderPlan {
    /// Frames the graph must produce, including latency slack when trimming.
    pub total: Samples,
    /// Frames the sink keeps: the requested span plus the tail.
    pub output_length: Samples,
    /// Leading frames the sink drops.
    pub latency: Samples,
}

impl RenderPlan {
    /// Derive the plan from a config. Pure — arithmetic over three FRAME
    /// counts, and no graph in sight.
    ///
    /// That purity is the point: an "ask the graph" latency mode would make a
    /// frame-count calculation require a mutable audio graph, so it could not be
    /// tested, reused, or reasoned about without building one first. The caller
    /// resolves the latency (see [`RenderGraph::reported_latency`](crate::RenderGraph::reported_latency))
    /// and passes a number.
    ///
    /// # Errors
    ///
    /// [`ConfigError::InvalidDuration`] for a duration that is NaN, infinite
    /// or negative, and [`ConfigError::EmptyRender`] for a plan that keeps no
    /// frame at all: neither is a render anyone meant, and a silent empty file
    /// reported as success hides which one it was. The rest is what
    /// [`Job::new`] refuses ([`ConfigError::Job`]): a rate that is not finite
    /// and positive, or a span past [`MAX_FRAMES`](crate::MAX_FRAMES).
    pub fn new(config: &RenderConfig) -> Result<Self, ConfigError> {
        if !config.duration_seconds.get().is_finite() || config.duration_seconds.get() < 0.0 {
            return Err(ConfigError::InvalidDuration);
        }
        let duration = duration_to_frames(config.duration_seconds, config.sample_rate);
        // A tail alone is a render (the audible span is nothing and the tail is
        // all of it), which `Job` refuses as a zero length: its length is then
        // the tail, which keeps the same frames and trims the same head.
        let (length, tail) = if duration == Samples(0) {
            (config.tail, Samples(0))
        } else {
            (duration, config.tail)
        };
        let job =
            Job::new(config.sample_rate, length, config.latency, tail).map_err(|e| match e {
                JobError::ZeroLength => ConfigError::EmptyRender,
                other => ConfigError::Job(other),
            })?;
        // The tail extends what the sink KEEPS, not just what the graph produces.
        // Two gates truncate independently — `total` bounds the pull loop and
        // `output_length` caps the kept frames — so extending only the first
        // would render the decay and then discard it. The job's counts are
        // both: the kept span plus the tail, and that plus the trimmed head.
        Ok(Self {
            total: job.total(),
            output_length: job.output_length(),
            latency: job.latency(),
        })
    }
}

/// Frames a `seconds` span covers at `rate`.
///
/// The one duration conversion in the crate, so the rounding happens once. A
/// non-finite or negative span is no frames rather than a panic or a wrapped
/// length; a render refuses such a span before it gets here
/// ([`ConfigError::InvalidDuration`]), so this is for a caller sizing one.
///
/// A [`Seconds`] is an `f64`, so a *derived* length keeps
/// its frame: 2000 beats at 93 bpm and 8000 at 111 bpm land on the frame they
/// name, where an `f32` span landed 2 and 6 frames off.
pub fn duration_to_frames(seconds: Seconds, rate: SampleRate) -> Samples {
    let seconds = seconds.get();
    if !seconds.is_finite() || seconds <= 0.0 {
        return Samples(0);
    }
    Samples((seconds * rate.get()).round() as usize)
}

/// Seconds covering `len` beats at `tempo` — the musical-vocabulary form.
///
/// Routed through [`beats_per_sample`](tutti_types::beats_per_sample)
/// rather than `BeatDuration::to_seconds`, which groups the arithmetic
/// differently and would round a frame differently. `(tempo / 60) / rate` is
/// the one spelling of the rate — see that function.
pub fn beats_to_seconds(len: BeatDuration, tempo: Bpm, rate: SampleRate) -> Seconds {
    let bps = tutti_types::beats_per_sample(tempo, rate).get();
    if bps <= 0.0 {
        return Seconds(0.0);
    }
    Seconds((len.get() / bps) / rate.get())
}

#[cfg(test)]
mod tests;
