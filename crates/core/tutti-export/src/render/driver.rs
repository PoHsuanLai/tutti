//! Block-loop driver for offline rendering.
//!
//! The render source is a [`NetSource`] — an [`AudioIn<f32, CH>`] that
//! block-renders a `tutti_core::dsp::Net` into `[f32; CH]` frames. The driver is
//! then the [`pump`](tutti_core::io::pump) loop from the engine's I/O
//! vocabulary: poll a block from the source, *gate* it through a [`BlockCursor`]
//! (latency-trim + output-length cap) — off the sink trait, since the gate needs
//! cross-block counters — then push the kept frames into the [`AudioOut`] sink.
//! Progress is reported via the supplied [`ProgressEmitter`].
//!
//! The whole loop is generic over the frame width `CH`: a stereo export is
//! `CH = 2`, a mono file `CH = 1`, a quad/surround render `CH = 4`/`6`/`8`. The
//! caller picks the width once (from the requested [`ChannelLayout`]) and the
//! source, gate, and sink are all monomorphized at it. `NetSource` **folds** the
//! net's actual output width onto the requested `CH` (see [`fold_net_frame`]) —
//! a graph wider than the file is downmixed, not truncated.

use crate::progress::ProgressEmitter;
use crate::render::{BlockCursor, RenderPlan};
use crate::Result;
use std::sync::Arc;
use tutti_core::io::{AudioIn, AudioOut};
use tutti_core::transport::OfflineTimeline;
use tutti_core::{AudioUnit, BufferMut, BufferRef, BufferVec, MAX_BUFFER_SIZE};

/// Widest net output the downmix gather handles (mono … 7.1.4 Atmos). Wider nets
/// have their channels beyond this dropped by the gather; the export dispatch
/// only admits 1/2/4/6/8/12 anyway.
const MAX_NET_CHANNELS: usize = 12;

/// Map one net output frame (`n_out` planar channels, read at sample `i`) onto a
/// `CH`-wide destination frame. This is the whole up/down-mix policy, in one
/// place — and it is a real fold, not a channel pick, so a graph WIDER than the
/// requested file is **downmixed**, never truncated:
///
/// - `n_out == 1` (a mono net): duplicate channel 0 into every destination, so a
///   mono net fills a stereo/quad/… frame with its one channel.
/// - `CH < n_out` (**downmix**): the ITU/Dolby matrix via
///   [`tutti_types::fold_frame`] — surround → stereo folds C + surrounds in at
///   −3 dB (dropping LFE), surround → mono sums that further. Without this the
///   center (dialogue) and surrounds (ambience) would be dropped.
/// - `CH >= n_out` (equal / upmix): channels `0..n_out` straight through, extra
///   destination channels zero-filled — no synthetic upmix.
///
/// The fold runs here, at the render → frame boundary, so every downstream stage
/// (mastering, dither, the encoder) already operates at the final `CH` width.
#[inline]
fn fold_net_frame<const CH: usize>(
    net: &BufferMut<'_>,
    n_out: usize,
    i: usize,
    dst: &mut [f32; CH],
) {
    if n_out == 1 {
        let s = net.channel_f32(0)[i];
        dst.fill(s);
        return;
    }
    let mut src = [0.0f32; MAX_NET_CHANNELS];
    let w = n_out.min(MAX_NET_CHANNELS);
    for (c, s) in src.iter_mut().enumerate().take(w) {
        *s = net.channel_f32(c)[i];
    }
    tutti_types::fold_frame(&src[..w], dst);
}

/// The render source as an [`AudioIn<f32, CH>`]: each
/// [`poll_into`](AudioIn::poll_into) block-renders the net and hands back
/// `CH`-wide frames, mapping the net's output channels onto the frame via
/// [`map_channel`]. When a timeline is supplied it advances in lockstep *after*
/// each block, matching the net clock's emit-then-advance convention (see the
/// no-priming note below).
pub(crate) struct NetSource<'a, const CH: usize> {
    net: &'a mut tutti_core::dsp::Net,
    timeline: Option<&'a Arc<OfflineTimeline>>,
    scratch: BufferVec,
    n_out: usize,
}

impl<'a, const CH: usize> NetSource<'a, CH> {
    pub(crate) fn new(
        net: &'a mut tutti_core::dsp::Net,
        sample_rate: f64,
        timeline: Option<&'a Arc<OfflineTimeline>>,
    ) -> Self {
        net.set_sample_rate(tutti_core::SampleRate(sample_rate));
        let n_out = net.outputs();
        // The scratch net-output buffer needs a slot per real output channel;
        // never zero (fundsp wants a valid plane) and at least the frame width so
        // `channel_f32` reads stay in bounds when the net is narrower than `CH`.
        let scratch = BufferVec::new(n_out.max(CH).max(1));
        Self {
            net,
            timeline,
            scratch,
            n_out,
        }
    }
}

impl<const CH: usize> AudioIn<f32, CH> for NetSource<'_, CH> {
    fn poll_into(&mut self, out: &mut [[f32; CH]]) -> usize {
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

        // Fold each net output frame onto the `CH`-wide destination frame —
        // downmix when the net is wider than the file, upmix (zero-fill) when
        // narrower, mono-duplicate for a mono net (see `fold_net_frame`).
        for (i, frame) in out[..block_size].iter_mut().enumerate() {
            fold_net_frame(&buffer_mut, self.n_out, i, frame);
        }
        block_size
    }
}

/// Drive `net` for `plan.total_samples` samples, gating each block and pushing
/// the kept `CH`-wide frames into `sink`, emitting progress via `progress`. If
/// `timeline` is provided, advance it in lockstep with the net so
/// transport-aware nodes receive correct beat positions. `CH` is the output
/// frame width — the caller picks it from the requested channel layout.
pub(crate) fn drive<const CH: usize>(
    net: &mut tutti_core::dsp::Net,
    sample_rate: f64,
    plan: &RenderPlan,
    timeline: Option<&Arc<OfflineTimeline>>,
    sink: &mut dyn AudioOut<f32, CH>,
    progress: &mut ProgressEmitter<'_>,
) -> Result<()> {
    let mut source = NetSource::<CH>::new(net, sample_rate, timeline);
    // Heap, not a stack array: at CH=8 this block is 64 KiB, past a comfortable
    // stack frame, and it's reused across the whole render.
    #[allow(clippy::useless_vec)]
    let mut block = vec![[0.0f32; CH]; MAX_BUFFER_SIZE];
    let mut kept_frames: Vec<[f32; CH]> = Vec::with_capacity(MAX_BUFFER_SIZE);

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
                steady_time: None,
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
