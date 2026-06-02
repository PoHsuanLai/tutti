//! Sample-count arithmetic for one offline render run.
//!
//! The plan is derived once from (duration, sample_rate, latency) and drives
//! both how many samples the net must produce and how many leading samples
//! the sink should drop.

use tutti_core::AudioUnit;

/// Fixed scheduling parameters for one render.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RenderPlan {
    /// Samples the net must produce, including latency slack when
    /// compensation is on.
    pub total_samples: usize,
    /// Samples the sink keeps (i.e. the audible duration).
    pub output_length: usize,
    /// Leading samples the sink drops to compensate for look-ahead latency.
    pub latency_samples: usize,
}

impl RenderPlan {
    /// `compensate_latency` adds `net.latency()` worth of extra samples to
    /// the render so the trimmed output starts at the intended position.
    pub fn new(
        net: &mut tutti_core::dsp::Net,
        sample_rate: f64,
        duration_seconds: f64,
        compensate_latency: bool,
    ) -> Self {
        let latency_samples = if compensate_latency {
            net.latency().unwrap_or(0.0).floor() as usize
        } else {
            0
        };
        let extra_duration = latency_samples as f64 / sample_rate;
        let total_samples = ((duration_seconds + extra_duration) * sample_rate).round() as usize;
        let output_length = (duration_seconds * sample_rate).round() as usize;

        Self {
            total_samples,
            output_length,
            latency_samples,
        }
    }
}
