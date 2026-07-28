//! Frame-count arithmetic for one offline render.
//!
//! Derived once from (duration, rate, latency), and drives both how many frames
//! the net must produce and how many leading frames the sink drops.

use crate::spec::{LatencyTrim, RenderSpec};
use tutti_core::AudioUnit;
use tutti_types::Samples;

/// Fixed scheduling parameters for one render.
///
/// Every field is [`Samples`] — a discrete frame count, which is what the type
/// exists for (its own doc names the compensation-delay path). Note that
/// `Samples` has no `Sub`: use `remaining_after` / `align_to`, which is why the
/// driver reads the way it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RenderPlan {
    /// Frames the net must produce, including latency slack when trimming.
    pub total: Samples,
    /// Frames the sink keeps — the audible duration.
    pub output_length: Samples,
    /// Leading frames the sink drops.
    pub latency: Samples,
}

impl RenderPlan {
    pub fn new(net: &mut tutti_core::dsp::Net, spec: &RenderSpec) -> Self {
        let latency = match spec.latency {
            LatencyTrim::None => Samples(0),
            // `Net::latency()` reports a fractional frame count; floor it, since
            // trimming a partial frame is not a thing a sink can do.
            LatencyTrim::Reported => {
                Samples(net.latency().unwrap_or(0.0).floor().max(0.0) as usize)
            }
            LatencyTrim::Exact(n) => n,
        };

        let output_length = spec.duration.to_frames(spec.sample_rate);
        // Render the audible span PLUS the trimmed head, so the output is still
        // `output_length` frames long after the drop.
        let total = Samples(output_length.get() + latency.get());

        Self {
            total,
            output_length,
            latency,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::RenderDuration;
    use tutti_core::SampleRate;

    fn spec(latency: LatencyTrim) -> RenderSpec {
        RenderSpec {
            sample_rate: SampleRate(48_000.0),
            duration: RenderDuration::Seconds(1.0),
            latency,
        }
    }

    fn silent_net() -> tutti_core::dsp::Net {
        let mut net = tutti_core::dsp::Net::new(0, 2);
        let id = net.push(Box::new(tutti_core::dsp::dc((0.0, 0.0))));
        net.pipe_output(id);
        net
    }

    #[test]
    fn no_trim_renders_exactly_the_audible_span() {
        let plan = RenderPlan::new(&mut silent_net(), &spec(LatencyTrim::None));
        assert_eq!(plan.output_length, Samples(48_000));
        assert_eq!(plan.total, Samples(48_000));
        assert_eq!(plan.latency, Samples(0));
    }

    /// The load-bearing property: trimming N frames means rendering N extra, or
    /// the file comes out short by exactly the trim.
    #[test]
    fn an_exact_trim_extends_the_render_by_that_much() {
        let plan = RenderPlan::new(&mut silent_net(), &spec(LatencyTrim::Exact(Samples(512))));
        assert_eq!(plan.output_length, Samples(48_000));
        assert_eq!(plan.total, Samples(48_512));
        assert_eq!(plan.latency, Samples(512));
    }
}
