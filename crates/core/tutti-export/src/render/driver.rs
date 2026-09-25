//! The render source: a graph presented as a block-at-a-time frame source.
//!
//! [`NetSource`] block-renders a `tutti_core::dsp::Net` into interleaved frames
//! and advances the caller's [`RenderClock`] in lockstep; [`GraphSource`] does
//! the same for a native `tutti_graph` executor (doc 013 Phase 3 PR 7).
//! Everything downstream — gating, dither, the encoder — pulls from one of
//! them, so the graph is stepped exactly once per block no matter which encoder
//! is driving, and the two backends share every stage after the render.
//!
//! The frame width is a **runtime** value carried by [`Frames`], never a
//! `const CH`. A width in the type can only carry one that is a property of the
//! *code*; the destination width here is a property of the caller's config, so
//! any positive count renders — 3- and 5-wide masters included.
//!
//! `NetSource` **folds** the net's real output width onto the requested one (see
//! [`fold_net_frame`]), so a graph wider than the file is downmixed rather than
//! truncated, and a narrower one is zero-filled rather than duplicated.
//! Reached only through the codec-gated encode arms, so a build with no format
//! feature on compiles this with no consumers. Scoped to that configuration
//! rather than allowed outright, so real dead code is still caught elsewhere.
#![cfg_attr(
    not(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg")),
    allow(dead_code)
)]

use crate::render::BlockCursor;
use crate::RenderGraph;
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

    /// Kept even where nothing calls it: it is `len`'s conventional companion
    /// (clippy's `len_without_is_empty`), and Ogg is currently its only caller
    /// — so any build without that feature sees no consumer.
    #[cfg_attr(not(feature = "ogg"), allow(dead_code))]
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
/// for every width — **no special cases, and the mono one is the trap.** A
/// `n_out == 1` short-circuit spraying `dst.fill(s)` puts a full-level copy of a
/// mono graph into every destination channel: for a 5.1 file that is program
/// material in the LFE and both surrounds, +6 dB on any downmix, and
/// correlated-mono comb filtering. No DAW does that, and neither does
/// `fold_frame` — beyond stereo it copies and zero-fills.
#[inline]
fn fold_net_frame(net: &BufferMut<'_>, n_out: usize, i: usize, dst: &mut [f32]) {
    fold_gathered(n_out, |c| net.channel_f32(c)[i], dst);
}

/// [`fold_net_frame`] for the native graph's planes: frame `i` of `planes`.
///
/// The same gather and the same fold, so a graph and a `Net` rendering equal
/// samples write equal frames at every destination width.
#[inline]
fn fold_graph_frame(planes: &[Vec<f32>], i: usize, dst: &mut [f32]) {
    fold_gathered(planes.len(), |c| planes[c][i], dst);
}

/// Gather `n_out` source channels (`sample(c)`) and fold them onto `dst`.
#[inline]
fn fold_gathered(n_out: usize, sample: impl Fn(usize) -> f32, dst: &mut [f32]) {
    // Stack, sized by the fixed ceiling rather than by the (runtime)
    // destination width: this is the *source* gather, and the graph's output
    // width is what it must hold. Heap-allocating it would put a `Vec` in a
    // per-frame loop.
    let mut src = [0.0f32; MAX_NET_CHANNELS];
    let w = n_out.min(MAX_NET_CHANNELS);
    for (c, s) in src.iter_mut().enumerate().take(w) {
        *s = sample(c);
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
        // Exactly one plane per REAL output channel, never the file width.
        // `Net::process` walks `0..output.channels()` while indexing
        // `output_edge[channel]`, which is only `net.outputs()` long — so a
        // scratch sized to a wider file panics on every upmix export.
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
/// Three implementations: [`NetSource`] and [`GraphSource`] render a graph, and
/// [`PlaneSource`] replays PCM a caller already holds. All feed the same
/// encoders, so a normalized export (render → measure → apply → write) shares
/// every codec path with a streamed one instead of growing a second writer per
/// format, and the two graph backends share it with each other.
pub(crate) trait FrameSource {
    /// Fill `out` — an interleaved buffer `ch` samples per frame — returning how
    /// many **frames** were written.
    fn fill(&mut self, out: &mut [f32], ch: usize) -> usize;
    /// Frames handed out so far.
    fn produced(&self) -> Samples;
    /// The most frames one [`fill`](Self::fill) produces — what [`drive`]
    /// sizes its block by.
    ///
    /// Per source rather than one crate constant, because the backends differ:
    /// fundsp's `Net` renders at most `MAX_BUFFER_SIZE` (64) frames a call,
    /// and the native graph renders whatever `MaxBlock` it was prepared for.
    /// Pulling the graph 64 frames at a time would run its per-block walk
    /// sixteen times as often as it needs to.
    fn max_block(&self) -> usize;
}

impl FrameSource for NetSource<'_> {
    /// A render never starves: the net produces a full block on demand, so the
    /// only `0` this returns is for a zero-length request.
    ///
    /// That is the offline/live split in one line. An `AudioIn`'s zero-frame
    /// poll is ambiguous — "not yet" for a live source, "never again" for a
    /// finite one — which is why that trait carries `ON_EMPTY`. An offline
    /// render is driven to a known frame count rather than polled until dry, so
    /// the ambiguity never arises and this is not an `AudioIn`.
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
        // AutomationLaneNode) while this one feeds clip readers and samplers. A
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

    fn max_block(&self) -> usize {
        MAX_BUFFER_SIZE
    }
}

/// The render source for the native graph: a `tutti_graph::Executor` (a
/// fork, or a pair built for the render) rendered one block at a time.
///
/// The counterpart of [`NetSource`], and deliberately the same shape: one
/// block per [`fill`](FrameSource::fill), folded onto the caller's width by
/// the same gather, the clock advanced by exactly the frames produced. What
/// differs is who carries time. A `Net` holds its clock as a node and only
/// needs the caller's clock advanced; the executor reads the transport from
/// each block's `Env`, so this hands it one — through
/// [`RenderClock::render_graph`], which reads the clock's snapshot before the
/// block and advances it after, chunk by chunk (64 frames across every node)
/// when the graph holds `Legacy` units (`OfflineTimeline::render_graph` is
/// that call for an offline timeline).
pub(crate) struct GraphSource<'a> {
    editor: &'a mut tutti_graph::Editor,
    executor: &'a mut tutti_graph::Executor,
    clock: &'a dyn RenderClock,
    /// Silence for every global input the graph declares: an export renders
    /// with nothing plugged in, as `NetSource`'s empty input buffer does.
    silence: Vec<f32>,
    global_inputs: usize,
    /// One plane per global output, `max_block` long.
    planes: Vec<Vec<f32>>,
    max_block: usize,
    produced: Samples,
}

impl<'a> GraphSource<'a> {
    /// Refuses a pair it cannot render **as configured** rather than rendering
    /// something else: an editor that does not feed this executor (its
    /// retirees would never be collected), or an executor prepared at a rate
    /// other than the render's. `NetSource` re-rates its net instead; an
    /// executor's units are prepared for one rate on the control side, which
    /// is what `RenderGraph::prepare` and `RenderGraph::fork` are for.
    pub(crate) fn new(
        editor: &'a mut tutti_graph::Editor,
        executor: &'a mut tutti_graph::Executor,
        sample_rate: tutti_core::SampleRate,
        clock: &'a dyn RenderClock,
    ) -> crate::Result<Self> {
        if !editor.is_paired_with(executor) {
            return Err(crate::Error::InvalidConfig(
                "the graph's editor does not feed its executor; pass the pair built together"
                    .into(),
            ));
        }
        let prepared = executor.prepare().sample_rate();
        if prepared.get() != sample_rate.get() {
            return Err(crate::Error::InvalidConfig(format!(
                "the graph is prepared at {} Hz and the render is at {} Hz; \
                 prepare it with `RenderGraph::prepare` or fork it with `RenderGraph::fork`",
                prepared.get(),
                sample_rate.get()
            )));
        }
        let max_block = executor.prepare().max_block().get();
        let topology = &editor.spec().topology;
        let global_inputs = topology.inputs.count() as usize;
        let planes = vec![vec![0.0f32; max_block]; topology.outputs.len()];
        Ok(Self {
            editor,
            executor,
            clock,
            silence: vec![0.0; max_block],
            global_inputs,
            planes,
            max_block,
            produced: Samples(0),
        })
    }
}

impl FrameSource for GraphSource<'_> {
    /// Never starves, for [`NetSource`]'s reason: an offline render is driven
    /// to a known frame count, so the only `0` is for a zero-length request.
    fn fill(&mut self, out: &mut [f32], ch: usize) -> usize {
        debug_assert!(ch > 0, "frame width must be non-zero");
        let block_size = (out.len() / ch).min(self.max_block);
        if block_size == 0 {
            return 0;
        }

        // Per block, and allocating: the executor takes slice lists, and a
        // list of borrows cannot live beside the buffers it borrows. Two
        // short `Vec`s per `max_block` frames, on an offline worker — the
        // executor's own walk inside `render_graph` stays allocation-free.
        let inputs: Vec<&[f32]> = (0..self.global_inputs)
            .map(|_| &self.silence[..block_size])
            .collect();
        let mut outputs: Vec<&mut [f32]> = self
            .planes
            .iter_mut()
            .map(|p| &mut p[..block_size])
            .collect();
        // Snapshot, process, advance — in that order (emit-then-advance, as
        // `NetSource` documents for the `Net` path), per 64-frame chunk
        // while the graph holds a `Legacy` unit.
        self.clock
            .render_graph(self.executor, block_size, &inputs, &mut outputs);
        // Retired units and returned commits are freed here, on this thread,
        // as a host's control loop would free them.
        self.editor.collect();

        for (i, frame) in out[..block_size * ch].chunks_exact_mut(ch).enumerate() {
            fold_graph_frame(&self.planes, i, frame);
        }
        self.produced = Samples(self.produced.get() + block_size);
        block_size
    }

    fn produced(&self) -> Samples {
        self.produced
    }

    fn max_block(&self) -> usize {
        self.max_block
    }
}

/// Run `f` over the frame source for `graph`, whichever backend it is.
///
/// The one place a [`RenderGraph`] becomes a [`FrameSource`], so every entry
/// point dispatches identically and nothing past this point knows which graph
/// it is pulling.
pub(crate) fn with_source<R>(
    graph: &mut RenderGraph,
    sample_rate: tutti_core::SampleRate,
    clock: &dyn RenderClock,
    f: impl FnOnce(&mut dyn FrameSource) -> crate::Result<R>,
) -> crate::Result<R> {
    match graph {
        RenderGraph::Net(net) => f(&mut NetSource::new(net, sample_rate, clock)),
        RenderGraph::Graph { editor, executor } => {
            f(&mut GraphSource::new(editor, executor, sample_rate, clock)?)
        }
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

    /// The planes are already rendered, so the size only paces the encoder.
    fn max_block(&self) -> usize {
        MAX_BUFFER_SIZE
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
    // Heap, not a stack array: at 12 channels a 1024-frame graph block is
    // 48 KiB. Derived once, never inside the loop.
    let max_block = src.max_block().max(1);
    let mut block = vec![0.0f32; max_block * ch];
    let mut kept = Samples(0);

    while src.produced() < plan.total {
        // `Samples` has no `Sub` on purpose — an unsigned count that wraps is a
        // buffer size that reads off the end of the world. `remaining_after` is
        // the named, saturating form.
        let want = plan
            .total
            .remaining_after(src.produced())
            .min(Samples(max_block));
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
    use tutti_core::{AtomicBool, AtomicF64, Beat, Bpm, SampleRate};

    /// A mono net whose one channel carries a constant.
    fn mono_dc(v: f32) -> tutti_core::dsp::Net {
        let mut net = tutti_core::dsp::Net::new(0, 1);
        let id = net.push(Box::new(tutti_nodes::testing::Const::mono(v)));
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

    /// Rendering a graph NARROWER than the file must not panic — a scratch
    /// sized to the frame width rather than to `net.outputs()` indexes past
    /// `output_edge` on every upmix.
    ///
    /// Widths 3 and 5 are here on purpose: they are the ones a fixed enumeration
    /// of widths would refuse outright.
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
    /// from the net's (LFO, AutomationLaneNode) for the whole render.
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
                tempo_in_force: None,
            },
            sample_rate,
        )
        .starting_at(start_beat);
        let id = net.push(Box::new(clock_node));
        net.pipe_output(id);

        let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
            start_beat: Beat(start_beat),
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
        for f in out.as_chunks::<4>().0 {
            assert_eq!(f, &[1.0, 0.0, 0.0, 0.0]);
        }
        // Second pull into the same dirty buffer.
        out.fill(9.9);
        assert_eq!(src.fill(&mut out, 4), 4);
        for f in out.as_chunks::<4>().0 {
            assert_eq!(f, &[1.0, 0.0, 0.0, 0.0]);
        }
    }
}
