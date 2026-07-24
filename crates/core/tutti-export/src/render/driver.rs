//! Block-loop driver for offline rendering.
//!
//! The render source is a [`NetSource`] — an [`AudioIn`] that block-renders a
//! `tutti_core::dsp::Net` into `[f32; 2]` frames. The driver is then the
//! [`pump`](tutti_core::io::pump) loop from the engine's I/O vocabulary: poll a
//! block from the source, *gate* it through a [`BlockCursor`] (latency-trim +
//! output-length cap) — off the sink trait, since the gate needs cross-block
//! counters — then push the kept frames into the [`AudioOut`] sink. Progress is
//! reported via the supplied [`ProgressEmitter`].

use crate::progress::ProgressEmitter;
use crate::render::{BlockCursor, RenderPlan};
use crate::Result;
use std::sync::Arc;
use tutti_core::io::{AudioIn, AudioOut};
use tutti_core::transport::OfflineTimeline;
use tutti_core::{AudioUnit, BufferRef, BufferVec, MAX_BUFFER_SIZE};

/// The render source as an [`AudioIn`]: each [`poll_into`](AudioIn::poll_into)
/// block-renders the net and hands back stereo frames. Mono nets have their one
/// channel duplicated here — channel-order policy is the source's business, per
/// the I/O vocabulary — so every sink downstream sees `[f32; 2]`. When a
/// timeline is supplied it advances in lockstep *after* each block, matching the
/// net clock's emit-then-advance convention (see the no-priming note below).
pub(crate) struct NetSource<'a> {
    net: &'a mut tutti_core::dsp::Net,
    timeline: Option<&'a Arc<OfflineTimeline>>,
    scratch: BufferVec,
    stereo: bool,
}

impl<'a> NetSource<'a> {
    pub(crate) fn new(
        net: &'a mut tutti_core::dsp::Net,
        sample_rate: f64,
        timeline: Option<&'a Arc<OfflineTimeline>>,
    ) -> Self {
        net.set_sample_rate(tutti_core::SampleRate(sample_rate));
        let stereo = net.outputs() >= 2;
        let scratch = BufferVec::new(net.outputs().max(2));
        Self {
            net,
            timeline,
            scratch,
            stereo,
        }
    }
}

impl AudioIn for NetSource<'_> {
    fn poll_into(&mut self, out: &mut [[f32; 2]]) -> usize {
        let block_size = out.len().min(MAX_BUFFER_SIZE);
        if block_size == 0 {
            return 0;
        }

        let empty_input = BufferRef::new(&[]);
        let mut buffer_mut = self.scratch.buffer_mut();
        self.net.process(block_size, &empty_input, &mut buffer_mut);

        // Advance AFTER processing, never before: `TransportClock` is
        // emit-then-advance (`transport/clock.rs`) — sample 0 of a block carries
        // the block's start beat, and only then does the beat increment. The
        // timeline must follow the same convention, because a region render
        // drives BOTH: the net's clock feeds beat-input nodes (LFO,
        // AutomationLane) while this timeline feeds clip readers and samplers. A
        // priming advance put those two exactly one `beats_per_sample` apart for
        // the whole render.
        if let Some(t) = self.timeline {
            t.advance(block_size);
        }

        let left = &buffer_mut.channel_f32(0)[..block_size];
        // Mono nets duplicate their one channel so the sink always sees stereo.
        if self.stereo {
            let right = &buffer_mut.channel_f32(1)[..block_size];
            for (frame, (&l, &r)) in out[..block_size].iter_mut().zip(left.iter().zip(right)) {
                *frame = [l, r];
            }
        } else {
            for (frame, &l) in out[..block_size].iter_mut().zip(left) {
                *frame = [l, l];
            }
        }
        block_size
    }
}

/// Drive `net` for `plan.total_samples` samples, gating each block and pushing
/// the kept frames into `sink`, emitting progress via `progress`. If `timeline`
/// is provided, advance it in lockstep with the net so transport-aware nodes
/// receive correct beat positions.
pub(crate) fn drive(
    net: &mut tutti_core::dsp::Net,
    sample_rate: f64,
    plan: &RenderPlan,
    timeline: Option<&Arc<OfflineTimeline>>,
    sink: &mut dyn AudioOut,
    progress: &mut ProgressEmitter<'_>,
) -> Result<()> {
    let mut source = NetSource::new(net, sample_rate, timeline);
    let mut block = vec![[0.0f32; 2]; MAX_BUFFER_SIZE];
    let mut kept_frames: Vec<[f32; 2]> = Vec::with_capacity(MAX_BUFFER_SIZE);

    progress.start();

    let mut produced = 0usize;
    let mut kept = 0usize;
    while produced < plan.total_samples {
        let want = (plan.total_samples - produced).min(MAX_BUFFER_SIZE);
        let n = source.poll_into(&mut block[..want]);
        if n == 0 {
            break;
        }

        // Pre-sink gate: keep only the windowed span (latency-trim + cap). It
        // needs the running block/kept counters, so it lives here in the loop
        // rather than on the `AudioOut` trait.
        let cursor = BlockCursor {
            block_start_sample: produced,
            latency_samples: plan.latency_samples,
            samples_kept_so_far: kept,
            output_length: plan.output_length,
        };
        let window = cursor.window(n);
        let kept_now = window.end - window.start;
        if kept_now > 0 {
            kept_frames.clear();
            kept_frames.extend_from_slice(&block[window]);
            sink.write(&kept_frames);
            kept += kept_now;
        }

        produced += n;
        progress.tick(produced);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::Phase;
    use tutti_core::transport::{OfflineTimelineConfig, TransportClock};
    use tutti_core::{AtomicBool, AtomicF64, Bpm, SampleRate as Sr};

    /// Captures the beat the net's `TransportClock` emitted on the very first
    /// rendered sample.
    struct FirstBeatSink {
        first_left: Option<f32>,
        first_right: Option<f32>,
    }

    impl AudioOut for FirstBeatSink {
        fn write(&mut self, frames: &[[f32; 2]]) {
            if self.first_left.is_none() {
                if let Some(&[l, r]) = frames.first() {
                    self.first_left = Some(l);
                    self.first_right = Some(r);
                }
            }
        }

        fn finalize(self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// `NetSource` must NOT prime the timeline: the net's clock is
    /// emit-then-advance, so the first rendered sample carries the start beat. A
    /// priming `advance(1)` desyncs the timeline (clip readers, samplers) from
    /// the clock (LFO, AutomationLane) by one `beats_per_sample` for a whole
    /// render. The gate stays exercised through `drive`.
    #[test]
    fn drive_does_not_prime_the_timeline_ahead_of_the_clock() {
        let sample_rate = 44100.0;
        let start_beat = 4.0;

        // A net that just emits the clock's two beat ports as its output.
        let mut net = tutti_core::dsp::Net::new(0, 2);
        let clock = TransportClock::new(
            tutti_core::transport::ClockLinks {
                tempo: Arc::new(AtomicF64::new(120.0)),
                paused: Arc::new(AtomicBool::new(false)),
                seek: Default::default(),
                loop_span: None,
                position_writeback: None,
            },
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
        let expected = start_beat + (timeline.beats_per_sample() * 512.0).get();
        assert!(
            (timeline.beat().get() - expected).abs() < 1e-9,
            "timeline advanced by {} samples' worth, expected exactly 512 \
             (a priming advance() desyncs it from the net's clock)",
            (timeline.beat().get() - start_beat) / timeline.beats_per_sample().get()
        );
    }
}
