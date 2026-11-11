//! Frame-count arithmetic for one offline render.
//!
//! Derived once from the job, and drives both how many frames the graph must
//! produce and how many leading frames the sink drops.

use crate::Job;
use tutti_types::Samples;

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
    /// The plan of a job. Pure: arithmetic over three FRAME counts, and no
    /// graph in sight.
    ///
    /// The tail extends what the sink KEEPS, not just what the graph produces.
    /// Two gates truncate independently (`total` bounds the pull loop and
    /// `output_length` caps the kept frames), so extending only the first
    /// would render the decay and then discard it. The job's counts are
    /// both: the kept span plus the tail, and that plus the trimmed head.
    pub fn of(job: &Job) -> Self {
        Self {
            total: job.total(),
            output_length: job.output_length(),
            latency: job.latency(),
        }
    }
}

#[cfg(test)]
mod tests;
