//! The render source: a graph presented as a block-at-a-time frame source.
//!
//! [`GraphSource`] block-renders a `tutti_graph` executor into
//! interleaved frames and advances the caller's [`RenderClock`] in lockstep.
//! Everything downstream — gating, dither, the encoder — pulls from it (or
//! from [`PlaneSource`], replaying a render already held), so the graph is
//! stepped exactly once per block no matter which encoder is driving.
//!
//! The frame width is a **runtime** value carried by
//! [`Interleaved`](tutti_types::Interleaved), never a `const CH`. A width in the type can only carry one that is a property of the
//! *code*; the destination width here is a property of the caller's config, so
//! any positive count renders — 3- and 5-wide masters included.
//!
//! `GraphSource` **folds** the graph's real output width onto the requested one
//! (see [`fold_graph_frame`]), so a graph wider than the file is downmixed
//! rather than truncated, and a narrower one is zero-filled rather than
//! duplicated.
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
use tutti_types::{ChannelLayout, Interleaved, InterleavedMut, Samples};

/// Widest graph output the fold handles (mono … 7.1.4). Channels past this are
/// dropped by the gather.
///
/// It is a **stack** ceiling for the per-frame gather scratch, not a limit on
/// what can be exported: the destination width is dynamic, only the *source*
/// gather is capped. Note this is 12 while `tutti_core`'s `MAX_ROOT_CHANNELS` is
/// 8 — deliberately not unified here, since an offline render has no RT root's
/// constraints.
const MAX_SOURCE_CHANNELS: usize = 12;

/// Map frame `i` of the graph's output `planes` (one per global output) onto a
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
fn fold_graph_frame(planes: &[Vec<f32>], i: usize, dst: &mut [f32]) {
    // Stack, sized by the fixed ceiling rather than by the (runtime)
    // destination width: this is the *source* gather, and the graph's output
    // width is what it must hold. Heap-allocating it would put a `Vec` in a
    // per-frame loop.
    let mut src = [0.0f32; MAX_SOURCE_CHANNELS];
    let w = planes.len().min(MAX_SOURCE_CHANNELS);
    for (c, s) in src.iter_mut().enumerate().take(w) {
        *s = planes[c][i];
    }
    tutti_types::fold_frame(&src[..w], dst);
}

/// Anything that can hand out frames one block at a time.
///
/// Two implementations: [`GraphSource`] renders a graph, and [`PlaneSource`]
/// replays PCM a caller already holds. Both feed the same encoders, so a
/// normalized export (render → measure → apply → write) shares every codec
/// path with a streamed one instead of growing a second writer per format.
pub(crate) trait FrameSource {
    /// Fill the leading frames of `out`, returning how many **frames** were
    /// written.
    fn fill(&mut self, out: InterleavedMut<'_>) -> usize;
    /// Frames handed out so far.
    fn produced(&self) -> Samples;
    /// The most frames one [`fill`](Self::fill) produces — what [`drive`]
    /// sizes its block by.
    ///
    /// Per source rather than one crate constant, because the sources differ:
    /// the graph renders whatever `MaxBlock` it was prepared for, and a
    /// replay has no block of its own. Pulling the graph 64 frames at a time
    /// would run its per-block walk sixteen times as often as it needs to.
    fn max_block(&self) -> usize;
}

/// The render source for the graph: a `tutti_graph::Executor` (a
/// fork, or a pair built for the render) rendered one block at a time.
///
/// One block per [`fill`](FrameSource::fill), folded onto the caller's width,
/// the clock advanced by exactly the frames produced. The executor reads the
/// transport from each block's `Env`, so this hands it one — through
/// [`RenderClock::render_graph`], which reads the clock's snapshot before the
/// block and advances it after (`OfflineTimeline::render_graph` is that call
/// for an offline timeline).
pub(crate) struct GraphSource<'a> {
    editor: &'a mut tutti_graph::Editor,
    executor: &'a mut tutti_graph::Executor,
    clock: &'a dyn RenderClock,
    /// Silence for every global input the graph declares: an export renders
    /// with nothing plugged in.
    silence: Vec<f32>,
    global_inputs: usize,
    /// One plane per global output, `max_block` long.
    planes: Vec<Vec<f32>>,
    max_block: usize,
    produced: Samples,
}

impl<'a> GraphSource<'a> {
    /// Refuses a pair it cannot render **as configured** rather than rendering
    /// something else: an executor prepared at a rate other than the
    /// render's. (The pairing is `RenderGraph`'s check, made before this is
    /// built.) An executor's units are prepared for one rate
    /// on the control side, which is what `RenderGraph::prepare` and
    /// `RenderGraph::fork` are for; re-rating them here would be preparing
    /// them on the render thread.
    pub(crate) fn new(
        editor: &'a mut tutti_graph::Editor,
        executor: &'a mut tutti_graph::Executor,
        sample_rate: tutti_core::SampleRate,
        clock: &'a dyn RenderClock,
    ) -> crate::Result<Self> {
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
    /// A render never starves: the graph produces a full block on demand, so
    /// the only `0` this returns is for a zero-length request.
    ///
    /// That is the offline/live split in one line. An `AudioIn`'s zero-frame
    /// poll is ambiguous — "not yet" for a live source, "never again" for a
    /// finite one — which is why that trait carries `ON_EMPTY`. An offline
    /// render is driven to a known frame count rather than polled until dry, so
    /// the ambiguity never arises and this is not an `AudioIn`.
    fn fill(&mut self, mut out: InterleavedMut<'_>) -> usize {
        let ch = out.stride();
        let block_size = out.len().min(self.max_block);
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
        // Snapshot, process, advance — in that order, never a priming
        // advance first: frame 0 of a block carries the block's start beat,
        // and only then does the beat move on. Every clock reader agrees on
        // it (every node reads the snapshot from its `Env`), so an advance
        // before the block would start the render one frame past its own
        // start beat.
        self.clock
            .render_graph(self.executor, block_size, &inputs, &mut outputs);
        // Retired units and returned commits are freed here, on this thread,
        // as a host's control loop would free them.
        self.editor.collect();

        for (i, frame) in out.samples_mut()[..block_size * ch]
            .chunks_exact_mut(ch)
            .enumerate()
        {
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

/// Run `f` over the frame source for `graph`, then check the fork's health.
///
/// The one place a [`RenderGraph`] becomes a [`FrameSource`], so every entry
/// point renders and checks identically.
pub(crate) fn with_source<R>(
    graph: &mut RenderGraph,
    sample_rate: tutti_core::SampleRate,
    clock: &dyn RenderClock,
    f: impl FnOnce(&mut dyn FrameSource) -> crate::Result<R>,
) -> crate::Result<R> {
    // Again at render, as a real error: `editor_mut` can swap the editor
    // after `RenderGraph::new` checked it (see `RenderGraph`, "The pair").
    graph.check_paired()?;
    let (editor, executor) = graph.parts_mut();
    let rendered = f(&mut GraphSource::new(editor, executor, sample_rate, clock)?)?;
    // A forked unit that failed mid-render (a plugin's server crashed or
    // hung) rendered silence from then on, and `process` has no error
    // channel: the probe is the only place the failure shows. A render that
    // passed it is a render of what the graph describes.
    editor
        .fork_health()
        .map_err(|fault| crate::Error::ForkFailed {
            key: fault.key,
            kind: fault.kind,
            cause: fault.cause,
        })?;
    Ok(rendered)
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
    fn fill(&mut self, mut out: InterleavedMut<'_>) -> usize {
        let available = self
            .planes
            .first()
            .map_or(0, |p| p.len().saturating_sub(self.pos.get()));
        let n = out.len().min(available);
        // A channel with no plane zero-fills, matching the graph path's
        // upmix rule. `interleave_from` writes every sample of every frame
        // (rather than leaving whatever the buffer held), which matters
        // because `drive`'s block is reused across iterations.
        let layout = out.layout();
        InterleavedMut::new(
            &mut out.samples_mut()[..n * layout.count() as usize],
            layout,
        )
        .interleave_from(self.planes, self.pos.get());
        self.pos = Samples(self.pos.get() + n);
        n
    }
    fn produced(&self) -> Samples {
        self.pos
    }

    /// The planes are already rendered, so the size only paces the encoder:
    /// 64 frames.
    ///
    /// Not raised to the graph's block: Ogg Vorbis's bytes depend on how its
    /// input is split into `encode_audio_block` calls, so this size is part of
    /// what a replayed Ogg export writes, and a 1024-frame replay measured no
    /// faster for the PCM formats.
    fn max_block(&self) -> usize {
        64
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
    layout: ChannelLayout,
    plan: &crate::render::RenderPlan,
    mut consume: F,
) -> crate::Result<()>
where
    F: FnMut(Interleaved<'_>) -> crate::Result<()>,
{
    let ch = layout.count() as usize;
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
        let n = src.fill(InterleavedMut::new(&mut block[..want.get() * ch], layout));
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
            // `window` is a frame range; `Interleaved::window` applies the
            // stride.
            let filled = Interleaved::new(&block[..n * ch], layout);
            consume(filled.window(window.clone()))?;
            kept = Samples(kept.get() + window.len());
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_core::transport::{FrozenClock, OfflineTimeline, OfflineTimelineConfig};
    use tutti_core::{Beat, Bpm, SampleRate};
    use tutti_graph::GraphBuilder;
    use tutti_types::ChannelLayout;

    const RATE: SampleRate = SampleRate(48_000.0);

    /// A mono graph whose one output carries a constant.
    fn mono_dc(v: f32) -> RenderGraph {
        let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
        let k = g.add(tutti_nodes::testing::Const::mono(v));
        g.pipe_output(k);
        let (editor, executor) = g.build(RenderGraph::prepare(RATE)).expect("builds");
        RenderGraph::new(editor, executor).expect("built together")
    }

    /// Render `frames` frames of `graph` at a runtime width under `clock`,
    /// returned as owned frames.
    fn render(
        graph: &mut RenderGraph,
        clock: &dyn RenderClock,
        ch: usize,
        frames: usize,
    ) -> Vec<Vec<f32>> {
        let plan = crate::render::RenderPlan {
            total: Samples(frames),
            output_length: Samples(frames),
            latency: Samples(0),
        };
        let mut out = Vec::new();
        with_source(graph, RATE, clock, |src| {
            drive(src, ChannelLayout::from(ch), &plan, |b| {
                out.extend(b.frames().map(<[f32]>::to_vec));
                Ok(())
            })
        })
        .unwrap();
        out
    }

    /// A mono graph rendered to a wider file must NOT be copied into every
    /// channel. `fold_frame`'s rule beyond stereo is copy-and-zero-fill, and a
    /// full-level mono copy in the LFE and surrounds is the bug this pins.
    ///
    /// Mutation (run): `fold_graph_frame` writing `dst.fill(src[0])` for a
    /// one-channel source → channel 1 reads 0.5.
    #[test]
    fn a_mono_graph_does_not_fill_every_surround_channel() {
        let frames = render(&mut mono_dc(0.5), &FrozenClock, 6, 64);
        assert!(!frames.is_empty());
        for f in &frames {
            assert_eq!(f.len(), 6);
            assert_eq!(f[0], 0.5, "channel 0 carries the mono signal");
            for (c, &s) in f.iter().enumerate().skip(1) {
                assert_eq!(s, 0.0, "channel {c} must be silent, got {s}");
            }
        }
    }

    /// Rendering a graph NARROWER than the file must not panic — a gather
    /// sized to the frame width rather than to the graph's outputs indexes
    /// past its planes on every upmix.
    ///
    /// Widths 3 and 5 are here on purpose: they are the ones a fixed enumeration
    /// of widths would refuse outright.
    ///
    /// Mutation (run): `fold_graph_frame` gathering `dst.len()` channels
    /// instead of `planes.len()` → it indexes a missing plane and panics.
    #[test]
    fn rendering_a_narrow_graph_to_a_wide_file_does_not_panic() {
        for ch in [2usize, 3, 4, 5, 12] {
            let frames = render(&mut mono_dc(0.25), &FrozenClock, ch, 32);
            assert_eq!(frames.len(), 32, "width {ch}");
            assert!(frames.iter().all(|f| f.len() == ch), "width {ch}");
        }
    }

    /// `GraphSource` must NOT prime the clock: the render is
    /// emit-then-advance, so the first rendered frame carries the start
    /// beat, and after N frames the clock stands exactly N frames on. A
    /// priming `advance` desyncs every clock reader from the render's start.
    ///
    /// The graph reads the beat from each block's transport, which this
    /// source hands it; here a test node calls `Env::for_each_beat`.
    ///
    /// Mutation (run): `self.clock.advance(Samples(1))` before `render_graph`
    /// in `GraphSource::fill` → the first frame reads a frame past the start
    /// beat.
    #[test]
    fn the_clock_advances_exactly_once_per_frame_and_never_ahead() {
        let start_beat = 4.0;

        /// Each frame's beat from its block's `Env`, whole beats then the
        /// fraction.
        struct BeatPorts;
        impl tutti_graph::Node for BeatPorts {
            fn shape(&self) -> tutti_graph::Shape {
                tutti_graph::Shape::audio(ChannelLayout::EMPTY, ChannelLayout::STEREO)
                    .with_tail(tutti_types::Tail::Unbounded)
            }
            fn prepare(&mut self, _: &tutti_graph::Prepare) {}
            fn process(
                &mut self,
                cx: &tutti_graph::Cx<'_>,
                mut io: tutti_graph::Io<'_>,
            ) -> tutti_graph::Status {
                cx.env.for_each_beat(|i, beat| {
                    io.output(0)[i] = beat.floor().get() as f32;
                    io.output(1)[i] = beat.fract().get() as f32;
                });
                tutti_graph::Status::Modified
            }
            fn reset(&mut self) {}
        }

        // A graph that emits the beat as its output.
        let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
        let clock = g.add(tutti_graph::Unforkable(BeatPorts));
        g.connect_output(clock, 0, 0).connect_output(clock, 1, 1);
        let (editor, executor) = g.build(RenderGraph::prepare(RATE)).expect("builds");
        let mut graph = RenderGraph::new(editor, executor).expect("built together");

        let timeline = OfflineTimeline::new(&OfflineTimelineConfig {
            start_beat: Beat(start_beat),
            tempo: Bpm(120.0),
            sample_rate: RATE,
            loop_range: None,
        });

        let frames = render(&mut graph, &timeline, 2, 512);
        let f = frames.first().expect("rendered at least one frame");
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

    /// A plane set narrower than the requested width zero-fills the tail —
    /// and does so on EVERY block, not just the first. `drive` reuses one
    /// buffer, so a `PlaneSource` that skipped the missing channels would leak
    /// the previous block's samples into them.
    #[test]
    fn plane_source_zero_fills_a_missing_channel_every_block() {
        let planes = vec![vec![1.0f32; 64]];
        let mut src = PlaneSource::new(&planes);
        let mut out = vec![9.9f32; 4 * 4];
        let quad = ChannelLayout::from(4u16);
        assert_eq!(src.fill(InterleavedMut::new(&mut out, quad)), 4);
        for f in out.as_chunks::<4>().0 {
            assert_eq!(f, &[1.0, 0.0, 0.0, 0.0]);
        }
        // Second pull into the same dirty buffer.
        out.fill(9.9);
        assert_eq!(src.fill(InterleavedMut::new(&mut out, quad)), 4);
        for f in out.as_chunks::<4>().0 {
            assert_eq!(f, &[1.0, 0.0, 0.0, 0.0]);
        }
    }
}
