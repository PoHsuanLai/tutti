//! Frame-count arithmetic for one offline render.
//!
//! Derived once from (duration, rate, latency), and drives both how many frames
//! the graph must produce and how many leading frames the sink drops.

use crate::ConfigError;
use crate::config::RenderConfig;
use tutti_types::SampleRate;
use tutti_types::{BeatDuration, Bpm, Samples};

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
    /// reported as success hides which one it was.
    pub fn new(config: &RenderConfig) -> Result<Self, ConfigError> {
        if !config.duration_seconds.is_finite() || config.duration_seconds < 0.0 {
            return Err(ConfigError::InvalidDuration);
        }
        let duration = duration_to_frames(config.duration_seconds, config.sample_rate);
        // The tail extends what the sink KEEPS, not just what the graph produces.
        // Two gates truncate independently — `total` bounds the pull loop and
        // `output_length` caps the kept frames — so extending only the first
        // would render the decay and then discard it.
        let output_length = duration + config.tail;
        // Render the kept span PLUS the trimmed head, so the output is still
        // `output_length` frames long after the drop.
        // Both are frame counts, so this is `Samples`' own saturating `Add`
        // rather than an unwrapped `usize` one.
        let total = output_length + config.latency;
        if output_length == Samples(0) {
            return Err(ConfigError::EmptyRender);
        }

        Ok(Self {
            total,
            output_length,
            latency: config.latency,
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
/// # Why `f64` and not [`Seconds`](tutti_types::Seconds)
///
/// The crate's one stop short of a unit type, and CLAUDE.md names the case:
/// *"`Seconds` is f32, so SMPTE timecode and hour-long render durations stay
/// f64."* Concretely, `Seconds` resolves individual frames only to ~256 s at
/// 48 kHz. Round lengths survive anyway (`3600.0` is exact), but a *derived* one
/// does not — 2000 beats at 93 bpm lands 2 frames off, 8000 at 111 bpm lands 6.
/// Small, silent, and every hand-written test would use a round number and pass.
pub fn duration_to_frames(seconds: f64, rate: SampleRate) -> Samples {
    if !seconds.is_finite() || seconds <= 0.0 {
        return Samples(0);
    }
    Samples((seconds * rate.get()).round() as usize)
}

/// Seconds covering `len` beats at `tempo` — the musical-vocabulary form.
///
/// Routed through [`beats_per_sample`](tutti_types::beats_per_sample)
/// rather than `BeatDuration::to_seconds`, which returns `f32` `Seconds` and
/// would reintroduce the narrowing above. `(tempo / 60) / rate` is the one
/// spelling of the rate — see that function.
pub fn beats_to_seconds(len: BeatDuration, tempo: Bpm, rate: SampleRate) -> f64 {
    let bps = tutti_types::beats_per_sample(tempo, rate).get();
    if bps <= 0.0 {
        return 0.0;
    }
    (len.get() / bps) / rate.get()
}

#[cfg(test)]
mod tests;
