//! Block-loop driver for offline rendering.
//!
//! Sets the net's sample rate, then pumps the net through [`MAX_BUFFER_SIZE`]
//! blocks until `plan.total_samples` have been produced. Each block is
//! handed to the [`RenderSink`] along with a [`BlockCursor`] describing its
//! position in the full render. The optional [`OfflineTimeline`] advances
//! in lockstep with the net so transport-aware nodes see the correct beat
//! position for each block. Progress is reported via the supplied
//! [`ProgressEmitter`].

use crate::progress::ProgressEmitter;
use crate::render::{BlockCursor, RenderPlan, RenderSink};
use crate::Result;
use std::sync::Arc;
use tutti_core::transport::OfflineTimeline;
use tutti_core::{AudioUnit, BufferRef, BufferVec, MAX_BUFFER_SIZE};

/// Drive `net` for `plan.total_samples` samples, pumping each block into
/// `sink` and emitting progress via `progress`. If `timeline` is provided,
/// advance it in lockstep with the net so transport-aware nodes receive
/// correct beat positions.
pub(crate) fn drive(
    net: &mut tutti_core::dsp::Net,
    sample_rate: f64,
    plan: &RenderPlan,
    timeline: Option<&Arc<OfflineTimeline>>,
    sink: &mut dyn RenderSink,
    progress: &mut ProgressEmitter<'_>,
) -> Result<()> {
    net.set_sample_rate(tutti_core::SampleRate(sample_rate));

    let mut buffer = BufferVec::new(net.outputs().max(2));
    let empty_input = BufferRef::new(&[]);
    let stereo = net.outputs() >= 2;

    progress.start();

    // No priming advance here: `TransportClock` is emit-then-advance
    // (`transport/clock.rs`) — sample 0 of a block carries the block's start
    // beat, and only then does the beat increment. The timeline must follow the
    // same convention, because a region render drives BOTH: the net's clock
    // feeds beat-input nodes (LFO, AutomationLane) while this timeline feeds
    // clip readers and samplers. Priming by one sample here put those two
    // exactly one `beats_per_sample` apart for the whole render.
    let mut produced = 0usize;
    let mut kept = 0usize;
    while produced < plan.total_samples {
        let block_size = (plan.total_samples - produced).min(MAX_BUFFER_SIZE);

        let mut buffer_mut = buffer.buffer_mut();
        net.process(block_size, &empty_input, &mut buffer_mut);

        if let Some(t) = timeline {
            t.advance(block_size);
        }

        let left = &buffer_mut.channel_f32(0)[..block_size];
        // Duplicate the left channel for mono nets so the sink always sees
        // stereo slices.
        let right_owned: Vec<f32>;
        let right: &[f32] = if stereo {
            &buffer_mut.channel_f32(1)[..block_size]
        } else {
            right_owned = left.to_vec();
            &right_owned
        };

        let cursor = BlockCursor {
            block_start_sample: produced,
            latency_samples: plan.latency_samples,
            samples_kept_so_far: kept,
            output_length: plan.output_length,
        };
        kept += sink.accept_block(left, right, cursor)?;

        produced += block_size;
        progress.tick(produced);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::Phase;
    use crate::render::BlockCursor;
    use tutti_core::transport::{OfflineTimelineConfig, TransportClock};
    use tutti_core::{AtomicBool, AtomicF64, Bpm, SampleRate as Sr};

    /// Captures the beat the net's `TransportClock` emitted on the very first
    /// rendered sample.
    struct FirstBeatSink {
        first_left: Option<f32>,
        first_right: Option<f32>,
    }

    impl RenderSink for FirstBeatSink {
        fn accept_block(
            &mut self,
            left: &[f32],
            right: &[f32],
            _cursor: BlockCursor,
        ) -> crate::Result<usize> {
            if self.first_left.is_none() && !left.is_empty() {
                self.first_left = Some(left[0]);
                self.first_right = Some(right[0]);
            }
            Ok(left.len())
        }
    }

    /// `drive` must NOT prime the timeline: the net's clock is emit-then-advance,
    /// so the first rendered sample carries the start beat. A priming
    /// `advance(1)` here desynced the timeline (clip readers, samplers) from the
    /// clock (LFO, AutomationLane) by one `beats_per_sample` for a whole render.
    #[test]
    fn drive_does_not_prime_the_timeline_ahead_of_the_clock() {
        let sample_rate = 44100.0;
        let start_beat = 4.0;

        // A net that just emits the clock's two beat ports as its output.
        let mut net = tutti_core::dsp::Net::new(0, 2);
        let clock = TransportClock::new(
            Arc::new(AtomicF64::new(120.0)),
            Arc::new(AtomicBool::new(false)),
            sample_rate,
        )
        .starting_at(start_beat);
        let clock_id = net.push(Box::new(clock));
        net.pipe_output(clock_id);

        let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
            start_beat,
            tempo: Bpm(120.0),
            sample_rate: Sr(sample_rate),
            loop_range: None,
        }));

        let plan = RenderPlan {
            total_samples: 512,
            output_length: 512,
            latency_samples: 0,
        };
        let mut sink = FirstBeatSink {
            first_left: None,
            first_right: None,
        };
        let noop = |_: Phase, _: f32| {};
        let mut progress = ProgressEmitter::new(&noop, Phase::Render, 512, sample_rate);

        drive(
            &mut net,
            sample_rate,
            &plan,
            Some(&timeline),
            &mut sink,
            &mut progress,
        )
        .expect("render");

        let emitted = sink.first_left.unwrap() as f64 + sink.first_right.unwrap() as f64;
        assert!(
            (emitted - start_beat).abs() < 1e-6,
            "first rendered sample should carry the start beat {start_beat}, got {emitted}"
        );

        // The load-bearing assertion: after rendering N samples the timeline
        // must have advanced by exactly N — no more. A priming `advance(1)`
        // shows up here as N+1, which is the desync this test exists to catch.
        let expected = start_beat + 512.0 * timeline.beats_per_sample();
        assert!(
            (timeline.beat().get() - expected).abs() < 1e-9,
            "timeline advanced by {} samples' worth, expected exactly 512 \
             (a priming advance() desyncs it from the net's clock)",
            (timeline.beat().get() - start_beat) / timeline.beats_per_sample()
        );
    }
}
