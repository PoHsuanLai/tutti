//! The render source: a `Net` presented as a block-at-a-time frame source.
//!
//! [`NetSource`] block-renders a `tutti_core::dsp::Net` into interleaved frames
//! and advances the caller's [`RenderClock`] in lockstep. Everything downstream
//! — gating, dither, the encoder — pulls from it, so the graph is stepped
//! exactly once per block no matter which encoder is driving.
//!
//! The frame width is a **runtime** value carried by [`Frames`], not a
//! `const CH`. It used to be const-generic, which forced the public entry points
//! through a `dispatch_channels!` macro that enumerated 1/2/4/6/8/12 and refused
//! everything else — so a 3- or 5-wide master could not be exported at all.
//! `NetSource` **folds** the net's real output width onto the requested one (see
//! [`fold_net_frame`]), so a graph wider than the file is downmixed rather than
//! truncated, and a narrower one is zero-filled rather than duplicated.

use crate::render::BlockCursor;
use tutti_core::transport::RenderClock;
use tutti_core::{AudioUnit, BufferMut, BufferRef, BufferVec, MAX_BUFFER_SIZE};
use tutti_types::Samples;

/// Widest net output the fold handles (mono … 7.1.4). Channels past this are
/// dropped by the gather.
///
/// It is a **stack** ceiling for the per-frame gather scratch, not a limit on
/// what can be exported: the destination width is dynamic, only the *source*
/// gather is capped. Note this is 12 while `tutti_core`'s `MAX_ROOT_CHANNELS` is
/// 8 — deliberately not unified here, since an offline render has no RT root's
/// constraints.
const MAX_NET_CHANNELS: usize = 12;

/// A borrowed run of interleaved frames, `ch` samples wide.
///
/// The width travels **with** the buffer rather than beside it. A flat
/// `&[f32]` plus a separate `ch: usize` reads identically whether an index is a
/// frame index or a sample index, and the driver's gate windows in *frames*
/// while the buffer is in *samples* — the exact confusion this newtype makes
/// unrepresentable. [`Frames::window`] is the only way to take a frame range,
/// and it does the `* ch` itself.
#[derive(Clone, Copy)]
pub(crate) struct Frames<'a> {
    data: &'a [f32],
    ch: usize,
}

impl<'a> Frames<'a> {
    /// Wrap `data` as `ch`-wide frames.
    ///
    /// # Panics
    /// If `ch` is 0, or `data` is not a whole number of frames.
    pub(crate) fn new(data: &'a [f32], ch: usize) -> Self {
        assert!(ch > 0, "frame width must be non-zero");
        debug_assert_eq!(data.len() % ch, 0, "buffer is not a whole number of frames");
        Self { data, ch }
    }

    /// Frame count — *not* sample count.
    pub(crate) fn len(&self) -> usize {
        self.data.len() / self.ch
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// The interleaved samples, as the encoders want them.
    pub(crate) fn samples(&self) -> &'a [f32] {
        self.data
    }

    /// The sub-run covering a **frame** range.
    pub(crate) fn window(&self, frames: std::ops::Range<usize>) -> Frames<'a> {
        Frames {
            data: &self.data[frames.start * self.ch..frames.end * self.ch],
            ch: self.ch,
        }
    }

    /// One frame, by frame index. Only the tests index a single frame; the
    /// encoders all want either the whole run or a frame iterator.
    #[cfg(test)]
    pub(crate) fn frame(&self, i: usize) -> &'a [f32] {
        &self.data[i * self.ch..(i + 1) * self.ch]
    }

    /// Iterate frame by frame.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &'a [f32]> + '_ {
        self.data.chunks_exact(self.ch)
    }
}

/// Map one net output frame (`n_out` planar channels, read at frame `i`) onto a
/// destination frame of whatever width `dst` is.
///
/// This is the entire up/down-mix policy, and it is [`tutti_types::fold_frame`]
/// for every width — no special cases. That matters: this function used to
/// short-circuit `n_out == 1` with `dst.fill(s)`, which put a full-level copy of
/// a mono graph into *every* destination channel. For a 5.1 file that meant
/// program material in the LFE and both surrounds, +6 dB on any downmix, and
/// correlated-mono comb filtering — none of which any DAW does, and none of
/// which `fold_frame` does either (beyond stereo it copies and zero-fills).
#[inline]
fn fold_net_frame(net: &BufferMut<'_>, n_out: usize, i: usize, dst: &mut [f32]) {
    // Stack, sized by the fixed ceiling rather than by the (runtime)
    // destination width: this is the *source* gather, and the net's output
    // width is what it must hold. Heap-allocating it would put a `Vec` in a
    // per-frame loop.
    let mut src = [0.0f32; MAX_NET_CHANNELS];
    let w = n_out.min(MAX_NET_CHANNELS);
    for (c, s) in src.iter_mut().enumerate().take(w) {
        *s = net.channel_f32(c)[i];
    }
    tutti_types::fold_frame(&src[..w], dst);
}

/// The render source: a `Net` rendered one block at a time.
///
/// Each [`fill`](FrameSource::fill) renders one block and hands back frames at
/// the caller's width, then advances the clock by exactly the frames produced.
pub(crate) struct NetSource<'a> {
    net: &'a mut tutti_core::dsp::Net,
    clock: &'a dyn RenderClock,
    scratch: BufferVec,
    n_out: usize,
    /// Frames handed out so far, for the gate the caller applies.
    produced: Samples,
}

impl<'a> NetSource<'a> {
    pub(crate) fn new(
        net: &'a mut tutti_core::dsp::Net,
        sample_rate: tutti_core::SampleRate,
        clock: &'a dyn RenderClock,
    ) -> Self {
        net.set_sample_rate(sample_rate);
        let n_out = net.outputs();
        // Exactly one plane per REAL output channel. Sizing this to the file
        // width when the file is wider than the graph made `Net::process` walk
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
}

/// Anything that can hand out frames one block at a time.
///
/// Two implementations: [`NetSource`] renders a graph, and [`PlaneSource`]
/// replays PCM a caller already holds. Both feed the same encoders, so a
/// normalized export (render → measure → apply → write) shares every codec path
/// with a streamed one instead of growing a second writer per format.
pub(crate) trait FrameSource {
    /// Fill `out` — an interleaved buffer `ch` samples per frame — returning how
    /// many **frames** were written.
    fn fill(&mut self, out: &mut [f32], ch: usize) -> usize;
    /// Frames handed out so far.
    fn produced(&self) -> Samples;
}

impl FrameSource for NetSource<'_> {
    /// A render never starves: the net produces a full block on demand, so the
    /// only `0` this returns is for a zero-length request. An offline render is
    /// driven to a known frame count, not polled until it runs dry.
    ///
    /// This used to be a `tutti_core::io::AudioIn<f32, CH>` impl that `fill`
    /// delegated to. The trait was the crate's only genuine need for a const
    /// frame width, and nothing outside this module ever polled a `NetSource`
    /// through it — so the body lives here directly rather than keeping a
    /// const-generic alive for one vestigial impl.
    fn fill(&mut self, out: &mut [f32], ch: usize) -> usize {
        debug_assert!(ch > 0, "frame width must be non-zero");
        let block_size = (out.len() / ch).min(MAX_BUFFER_SIZE);
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

        for (i, frame) in out[..block_size * ch].chunks_exact_mut(ch).enumerate() {
            fold_net_frame(&buffer_mut, self.n_out, i, frame);
        }
        self.produced = Samples(self.produced.get() + block_size);
        block_size
    }

    fn produced(&self) -> Samples {
        self.produced
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

impl FrameSource for PlaneSource<'_> {
    fn fill(&mut self, out: &mut [f32], ch: usize) -> usize {
        debug_assert!(ch > 0, "frame width must be non-zero");
        let available = self
            .planes
            .first()
            .map_or(0, |p| p.len().saturating_sub(self.pos.get()));
        let n = (out.len() / ch).min(available);
        for (i, frame) in out[..n * ch].chunks_exact_mut(ch).enumerate() {
            let at = self.pos.get() + i;
            // A plane narrower than the frame zero-fills, matching the graph
            // path's upmix rule. Written explicitly (rather than left to
            // whatever the buffer held) because `drive`'s block is reused
            // across iterations.
            for (c, s) in frame.iter_mut().enumerate() {
                *s = self
                    .planes
                    .get(c)
                    .and_then(|p| p.get(at))
                    .copied()
                    .unwrap_or(0.0);
            }
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
pub(crate) fn drive<F>(
    src: &mut dyn FrameSource,
    ch: usize,
    plan: &crate::render::RenderPlan,
    mut consume: F,
) -> crate::Result<()>
where
    F: FnMut(Frames<'_>) -> crate::Result<()>,
{
    if ch == 0 {
        return Err(crate::Error::UnsupportedChannels(0));
    }
    // Heap, not a stack array: at 12 channels a block is 96 KiB. Derived once,
    // never inside the loop.
    let mut block = vec![0.0f32; MAX_BUFFER_SIZE * ch];
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
        // `want` is a FRAME count and `block` is flat samples, hence `* ch`.
        let n = src.fill(&mut block[..want.get() * ch], ch);
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
            // `window` is a frame range; `Frames::window` applies the stride.
            let filled = Frames::new(&block[..n * ch], ch);
            consume(filled.window(window.clone()))?;
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

    /// Render `frames` frames at a runtime width, returned as owned frames.
    fn render(net: &mut tutti_core::dsp::Net, ch: usize, frames: usize) -> Vec<Vec<f32>> {
        let clock = tutti_core::transport::FrozenClock;
        let mut src = NetSource::new(net, SampleRate(48_000.0), &clock);
        let plan = crate::render::RenderPlan {
            total: Samples(frames),
            output_length: Samples(frames),
            latency: Samples(0),
        };
        let mut out = Vec::new();
        drive(&mut src, ch, &plan, |b| {
            out.extend(b.iter().map(<[f32]>::to_vec));
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
        let frames = render(&mut mono_dc(0.5), 6, 64);
        assert!(!frames.is_empty());
        for f in &frames {
            assert_eq!(f.len(), 6);
            assert_eq!(f[0], 0.5, "channel 0 carries the mono signal");
            for (c, &s) in f.iter().enumerate().skip(1) {
                assert_eq!(s, 0.0, "channel {c} must be silent, got {s}");
            }
        }
    }

    /// Rendering a graph NARROWER than the file must not panic. The scratch
    /// buffer used to be sized to the frame width, so `Net::process` indexed
    /// past `output_edge` and blew up on every upmix.
    ///
    /// Widths 3 and 5 are here on purpose: they are the ones the old
    /// `dispatch_channels!` refused outright.
    #[test]
    fn rendering_a_narrow_graph_to_a_wide_file_does_not_panic() {
        for ch in [2usize, 3, 4, 5, 12] {
            let frames = render(&mut mono_dc(0.25), ch, 32);
            assert_eq!(frames.len(), 32, "width {ch}");
            assert!(frames.iter().all(|f| f.len() == ch), "width {ch}");
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

        let mut src = NetSource::new(&mut net, SampleRate(sample_rate), timeline.as_ref());
        let plan = crate::render::RenderPlan {
            total: Samples(512),
            output_length: Samples(512),
            latency: Samples(0),
        };
        let mut first: Option<Vec<f32>> = None;
        drive(&mut src, 2, &plan, |b| {
            if first.is_none() && !b.is_empty() {
                first = Some(b.frame(0).to_vec());
            }
            Ok(())
        })
        .unwrap();

        let f = first.expect("rendered at least one frame");
        let emitted = f64::from(f[0]) + f64::from(f[1]);
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

    /// `Frames` windows in FRAMES while carrying flat interleaved samples —
    /// the index confusion the newtype exists to prevent.
    #[test]
    fn frames_windows_by_frame_not_by_sample() {
        let data: Vec<f32> = (0..12).map(|i| i as f32).collect();
        let f = Frames::new(&data, 3);
        assert_eq!(f.len(), 4, "12 samples at width 3 is 4 frames");
        let w = f.window(1..3);
        assert_eq!(w.len(), 2);
        assert_eq!(w.samples(), &[3.0, 4.0, 5.0, 6.0, 7.0, 8.0]);
        assert_eq!(f.frame(2), &[6.0, 7.0, 8.0]);
    }

    /// A plane set narrower than the requested width zero-fills the tail —
    /// and does so on EVERY block, not just the first. `drive` reuses one
    /// buffer, so a `PlaneSource` that skipped the missing channels would leak
    /// the previous block's samples into them.
    #[test]
    fn plane_source_zero_fills_a_missing_channel_every_block() {
        let planes = vec![vec![1.0f32; 64]];
        let mut src = PlaneSource::new(&planes);
        let mut out = vec![9.9f32; 4 * 4];
        assert_eq!(src.fill(&mut out, 4), 4);
        for f in out.chunks_exact(4) {
            assert_eq!(f, &[1.0, 0.0, 0.0, 0.0]);
        }
        // Second pull into the same dirty buffer.
        out.fill(9.9);
        assert_eq!(src.fill(&mut out, 4), 4);
        for f in out.chunks_exact(4) {
            assert_eq!(f, &[1.0, 0.0, 0.0, 0.0]);
        }
    }
}
