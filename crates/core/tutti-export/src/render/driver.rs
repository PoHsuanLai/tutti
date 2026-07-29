//! The render source: a `Net` presented as an [`AudioIn`].
//!
//! [`NetSource`] block-renders a `tutti_core::dsp::Net` into `[f32; CH]` frames
//! and advances the caller's [`RenderClock`] in lockstep. Everything downstream
//! — gating, dither, the encoder — pulls from it, so the graph is stepped
//! exactly once per block no matter which encoder is driving.
//!
//! The whole path is generic over the frame width `CH`. `NetSource` **folds**
//! the net's real output width onto `CH` (see [`fold_net_frame`]), so a graph
//! wider than the file is downmixed rather than truncated, and a narrower one is
//! zero-filled rather than duplicated.

use crate::render::BlockCursor;
use tutti_core::io::{AudioIn, OnEmpty};
use tutti_core::transport::RenderClock;
use tutti_core::{AudioUnit, BufferMut, BufferRef, BufferVec, MAX_BUFFER_SIZE};
use tutti_types::Samples;

/// Widest net output the fold handles (mono … 7.1.4). Channels past this are
/// dropped by the gather; the export dispatch only admits 1/2/4/6/8/12 anyway.
const MAX_NET_CHANNELS: usize = 12;

/// Map one net output frame (`n_out` planar channels, read at frame `i`) onto a
/// `CH`-wide destination.
///
/// This is the entire up/down-mix policy, and it is [`tutti_types::fold_frame`]
/// for every width — no special cases. That matters: this function used to
/// short-circuit `n_out == 1` with `dst.fill(s)`, which put a full-level copy of
/// a mono graph into *every* destination channel. For a 5.1 file that meant
/// program material in the LFE and both surrounds, +6 dB on any downmix, and
/// correlated-mono comb filtering — none of which any DAW does, and none of
/// which `fold_frame` does either (beyond stereo it copies and zero-fills).
#[inline]
fn fold_net_frame<const CH: usize>(
    net: &BufferMut<'_>,
    n_out: usize,
    i: usize,
    dst: &mut [f32; CH],
) {
    let mut src = [0.0f32; MAX_NET_CHANNELS];
    let w = n_out.min(MAX_NET_CHANNELS);
    for (c, s) in src.iter_mut().enumerate().take(w) {
        *s = net.channel_f32(c)[i];
    }
    tutti_types::fold_frame(&src[..w], dst);
}

/// The render source as an [`AudioIn<f32, CH>`].
///
/// Each [`poll_into`](AudioIn::poll_into) renders one block and hands back
/// `CH`-wide frames, then advances the clock by exactly the frames produced.
pub(crate) struct NetSource<'a, const CH: usize> {
    net: &'a mut tutti_core::dsp::Net,
    clock: &'a dyn RenderClock,
    scratch: BufferVec,
    n_out: usize,
    /// Frames handed out so far, for the gate the caller applies.
    produced: Samples,
}

impl<'a, const CH: usize> NetSource<'a, CH> {
    pub(crate) fn new(
        net: &'a mut tutti_core::dsp::Net,
        sample_rate: tutti_core::SampleRate,
        clock: &'a dyn RenderClock,
    ) -> Self {
        net.set_sample_rate(sample_rate);
        let n_out = net.outputs();
        // Exactly one plane per REAL output channel. Sizing this to `CH` when
        // the file is wider than the graph made `Net::process` walk
        // `0..output.channels()` while indexing `output_edge[channel]`, which is
        // only `net.outputs()` long — a hard panic on every upmix export.
        let scratch = BufferVec::new(n_out.max(1));
        Self {
            net,
            clock,
            scratch,
            n_out,
            produced: Samples(0),
        }
    }

    /// Frames produced so far.
    pub(crate) fn produced(&self) -> Samples {
        self.produced
    }
}

impl<const CH: usize> AudioIn<f32, CH> for NetSource<'_, CH> {
    /// A render never starves: the net produces a full block on demand, so the
    /// only `0` this returns is for a zero-length request. An offline render is
    /// driven to a known frame count, not polled until it runs dry.
    const ON_EMPTY: OnEmpty = OnEmpty::EndOfStream;

    fn poll_into(&mut self, out: &mut [[f32; CH]]) -> usize {
        let block_size = out.len().min(MAX_BUFFER_SIZE);
        if block_size == 0 {
            return 0;
        }

        let empty_input = BufferRef::new(&[]);
        let mut buffer_mut = self.scratch.buffer_mut();
        self.net.process(block_size, &empty_input, &mut buffer_mut);

        // Advance AFTER processing, never before: `TransportClock` is
        // emit-then-advance (`transport/clock.rs`) — frame 0 of a block carries
        // the block's start beat, and only then does the beat increment. The
        // caller's clock must follow the same convention, because a region
        // render drives BOTH: the net's clock feeds beat-input nodes (LFO,
        // AutomationLane) while this one feeds clip readers and samplers. A
        // priming advance puts those two exactly one `beats_per_sample` apart
        // for the whole render.
        self.clock.advance(Samples(block_size));

        for (i, frame) in out[..block_size].iter_mut().enumerate() {
            fold_net_frame(&buffer_mut, self.n_out, i, frame);
        }
        self.produced = Samples(self.produced.get() + block_size);
        block_size
    }
}

/// Anything that can hand out `CH`-wide frames one block at a time.
///
/// Two implementations: [`NetSource`] renders a graph, and [`PlaneSource`]
/// replays PCM a caller already holds. Both feed the same encoders, so a
/// normalized export (render → measure → apply → write) shares every codec path
/// with a streamed one instead of growing a second writer per format.
pub(crate) trait FrameSource<const CH: usize> {
    /// Fill `out`, returning how many frames were written.
    fn fill(&mut self, out: &mut [[f32; CH]]) -> usize;
    /// Frames handed out so far.
    fn produced(&self) -> Samples;
}

impl<const CH: usize> FrameSource<CH> for NetSource<'_, CH> {
    fn fill(&mut self, out: &mut [[f32; CH]]) -> usize {
        self.poll_into(out)
    }
    fn produced(&self) -> Samples {
        NetSource::produced(self)
    }
}

/// Replays already-rendered planes as a frame source.
pub(crate) struct PlaneSource<'a> {
    planes: &'a [Vec<f32>],
    pos: Samples,
}

impl<'a> PlaneSource<'a> {
    pub(crate) fn new(planes: &'a [Vec<f32>]) -> Self {
        Self {
            planes,
            pos: Samples(0),
        }
    }
}

impl<const CH: usize> FrameSource<CH> for PlaneSource<'_> {
    fn fill(&mut self, out: &mut [[f32; CH]]) -> usize {
        let available = self
            .planes
            .first()
            .map_or(0, |p| p.len().saturating_sub(self.pos.get()));
        let n = out.len().min(available);
        for (i, frame) in out[..n].iter_mut().enumerate() {
            let at = self.pos.get() + i;
            // A plane narrower than the frame zero-fills, matching the graph
            // path's upmix rule.
            *frame = std::array::from_fn(|c| {
                self.planes
                    .get(c)
                    .and_then(|p| p.get(at))
                    .copied()
                    .unwrap_or(0.0)
            });
        }
        self.pos = Samples(self.pos.get() + n);
        n
    }
    fn produced(&self) -> Samples {
        self.pos
    }
}

/// Pull the whole render out of `src`, gating each block, handing the kept
/// frames to `consume`.
///
/// The gate (latency trim + output-length cap) lives here rather than on a sink
/// because it needs counters that span blocks. `consume` is called only with
/// frames that survive it.
pub(crate) fn drive<const CH: usize, F>(
    src: &mut dyn FrameSource<CH>,
    plan: &crate::render::RenderPlan,
    mut consume: F,
) -> crate::Result<()>
where
    F: FnMut(&[[f32; CH]]) -> crate::Result<()>,
{
    // Heap, not a stack array: at CH=12 a block is 96 KiB.
    #[allow(clippy::useless_vec)]
    let mut block = vec![[0.0f32; CH]; MAX_BUFFER_SIZE];
    let mut kept = Samples(0);

    while src.produced() < plan.total {
        // `Samples` has no `Sub` on purpose — an unsigned count that wraps is a
        // buffer size that reads off the end of the world. `remaining_after` is
        // the named, saturating form.
        let want = plan
            .total
            .remaining_after(src.produced())
            .min(Samples(MAX_BUFFER_SIZE));
        let block_start = src.produced();
        let n = src.fill(&mut block[..want.get()]);
        if n == 0 {
            break;
        }

        let cursor = BlockCursor {
            block_start,
            latency: plan.latency,
            kept_so_far: kept,
            output_length: plan.output_length,
        };
        let window = cursor.window(Samples(n));
        if !window.is_empty() {
            consume(&block[window.clone()])?;
            kept = Samples(kept.get() + window.len());
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tutti_core::transport::{OfflineTimeline, OfflineTimelineConfig, TransportClock};
    use tutti_core::{AtomicBool, AtomicF64, Bpm, SampleRate};

    /// A mono net whose one channel carries a constant.
    fn mono_dc(v: f32) -> tutti_core::dsp::Net {
        let mut net = tutti_core::dsp::Net::new(0, 1);
        let id = net.push(Box::new(tutti_core::dsp::dc(v)));
        net.pipe_output(id);
        net
    }

    fn render<const CH: usize>(net: &mut tutti_core::dsp::Net, frames: usize) -> Vec<[f32; CH]> {
        let clock = tutti_core::transport::FrozenClock;
        let mut src = NetSource::<CH>::new(net, SampleRate(48_000.0), &clock);
        let plan = crate::render::RenderPlan {
            total: Samples(frames),
            output_length: Samples(frames),
            latency: Samples(0),
        };
        let mut out = Vec::new();
        drive(&mut src, &plan, |b| {
            out.extend_from_slice(b);
            Ok(())
        })
        .unwrap();
        out
    }

    /// A mono graph rendered to a wider file must NOT be copied into every
    /// channel. `fold_frame`'s rule beyond stereo is copy-and-zero-fill, and a
    /// full-level mono copy in the LFE and surrounds is the bug this pins.
    #[test]
    fn a_mono_graph_does_not_fill_every_surround_channel() {
        let frames = render::<6>(&mut mono_dc(0.5), 64);
        assert!(!frames.is_empty());
        for f in &frames {
            assert_eq!(f[0], 0.5, "channel 0 carries the mono signal");
            for (c, &s) in f.iter().enumerate().skip(1) {
                assert_eq!(s, 0.0, "channel {c} must be silent, got {s}");
            }
        }
    }

    /// Rendering a graph NARROWER than the file must not panic. The scratch
    /// buffer used to be sized to the frame width, so `Net::process` indexed
    /// past `output_edge` and blew up on every upmix.
    #[test]
    fn rendering_a_narrow_graph_to_a_wide_file_does_not_panic() {
        for frames in [
            render::<2>(&mut mono_dc(0.25), 32).len(),
            render::<4>(&mut mono_dc(0.25), 32).len(),
            render::<12>(&mut mono_dc(0.25), 32).len(),
        ] {
            assert_eq!(frames, 32);
        }
    }

    /// `NetSource` must NOT prime the clock: the net's own clock is
    /// emit-then-advance, so the first rendered frame carries the start beat. A
    /// priming `advance` desyncs the caller's clock (clip readers, samplers)
    /// from the net's (LFO, AutomationLane) for the whole render.
    #[test]
    fn the_clock_advances_exactly_once_per_frame_and_never_ahead() {
        let sample_rate = 48_000.0;
        let start_beat = 4.0;

        // A net that emits the clock's two beat ports as its output.
        let mut net = tutti_core::dsp::Net::new(0, 2);
        let clock_node = TransportClock::new(
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
        let id = net.push(Box::new(clock_node));
        net.pipe_output(id);

        let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
            start_beat,
            tempo: Bpm(120.0),
            sample_rate: SampleRate(sample_rate),
            loop_range: None,
        }));

        let mut src = NetSource::<2>::new(&mut net, SampleRate(sample_rate), timeline.as_ref());
        let plan = crate::render::RenderPlan {
            total: Samples(512),
            output_length: Samples(512),
            latency: Samples(0),
        };
        let mut first = None;
        drive(&mut src, &plan, |b| {
            if first.is_none() {
                first = b.first().copied();
            }
            Ok(())
        })
        .unwrap();

        let [l, r] = first.expect("rendered at least one frame");
        let emitted = f64::from(l) + f64::from(r);
        assert!(
            (emitted - start_beat).abs() < 1e-6,
            "first frame should carry the start beat {start_beat}, got {emitted}"
        );

        // After N frames the clock must have advanced by exactly N. A priming
        // advance shows up here as N+1 — the desync this test exists to catch.
        let expected = start_beat + (timeline.beats_per_sample() * 512.0).get();
        assert!(
            (timeline.beat().get() - expected).abs() < 1e-9,
            "clock advanced {} frames' worth, expected exactly 512",
            (timeline.beat().get() - start_beat) / timeline.beats_per_sample().get()
        );
    }
}
