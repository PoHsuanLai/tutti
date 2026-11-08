//! The render source: a graph presented as a block-at-a-time frame source.
//!
//! [`GraphSource`] block-renders a `tutti_graph` executor into
//! interleaved frames and advances the caller's [`RenderClock`] in lockstep.
//! Everything downstream — gating, dither, the encoder — pulls from it (or
//! from [`PlaneSource`], replaying a render already held), so the graph is
//! stepped exactly once per block no matter which encoder is driving.
//!
//! The frame width is a runtime value carried by
//! [`Interleaved`], never a `const CH`. A width in the type can only carry one that is a property of the
//! *code*; the destination width here is a property of the caller's config, so
//! any positive count renders — 3- and 5-wide masters included.
//!
//! `GraphSource` folds the graph's real output width onto the requested one
//! ([`tutti_types::Fold`], built once in [`GraphSource::new`]), so a graph wider
//! than the file is downmixed rather than truncated, and a narrower one is
//! zero-filled rather than duplicated.

use crate::control::{Control, Flow, Outcome};
use crate::render::BlockCursor;
use crate::{ConfigError, RenderGraph};
use tutti_transport::RenderClock;
use tutti_types::{ChannelLayout, ChannelTopology, Fold, Interleaved, InterleavedMut, Samples};

/// What the file's channels are, for the fold from the graph's outputs: its
/// width, and the speakers they feed when the config states them.
pub(crate) struct FileChannels<'a> {
    pub(crate) layout: ChannelLayout,
    pub(crate) topology: Option<&'a ChannelTopology>,
}

impl FileChannels<'_> {
    /// The fold from a graph of `outputs` channels. The graph is what it
    /// declared (`root`), or else the engine's order for its width (a bare 4 is
    /// quad); the file the topology it states or else the same assumption for
    /// its width; where either has no order the channels go by index
    /// ([`Fold::widths`], [`Fold::declared`]).
    ///
    /// # Errors
    ///
    /// [`ConfigError::Placement`] when the file's topology is not as wide as
    /// the file or cannot take the graph's channels, or the declared root is
    /// not as wide as the graph, or the graph has more channels than a layout
    /// holds ([`FoldError::TooWide`](tutti_types::FoldError::TooWide)).
    fn fold_from(&self, outputs: usize, root: Option<&ChannelTopology>) -> crate::Result<Fold> {
        let graph = ChannelLayout::try_from(outputs)
            .map_err(|_| ConfigError::Placement(tutti_types::FoldError::TooWide(outputs)))?;
        if let Some(root) = root
            && root.layout() != graph
        {
            return Err(ConfigError::Placement(tutti_types::FoldError::RootWidth {
                declared: root.layout(),
                requested: graph,
            })
            .into());
        }
        let Some(topology) = self.topology else {
            let fold = match root {
                Some(root) => Fold::declared(root, self.layout),
                None => Fold::widths(graph, self.layout),
            };
            return fold.map_err(|e| ConfigError::Placement(e).into());
        };
        if topology.layout() != self.layout {
            return Err(ConfigError::Placement(tutti_types::FoldError::Discrete {
                src: graph,
                dst: topology.layout(),
            })
            .into());
        }
        let from = match root {
            Some(root) => root.clone(),
            None => {
                ChannelTopology::assumed(graph).unwrap_or_else(|| ChannelTopology::discrete(graph))
            }
        };
        Fold::between(&from, topology).map_err(|e| ConfigError::Placement(e).into())
    }
}

/// Anything that can hand out frames one block at a time.
///
/// Two implementations: [`GraphSource`] renders a graph, and [`PlaneSource`]
/// replays PCM a caller already holds. Both feed the same encoders, so a
/// normalized export (render → measure → apply → write) shares every codec
/// path with a streamed one instead of growing a second writer per format.
pub(crate) trait FrameSource {
    /// Fill the leading frames of `out`, returning how many frames were
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
    /// Events the graph lost since the render began (`Executor::lost_event_count`):
    /// not a note-off it refused and sent again. A replay loses none.
    fn lost_events(&self) -> u64 {
        0
    }
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
    /// The graph's outputs onto the file's channels, built in [`new`](Self::new).
    fold: Fold,
    max_block: usize,
    produced: Samples,
    /// The executor's dropped and recovered counts when the render began;
    /// kept apart, because a recovery in the render may be of a refusal
    /// before it.
    before: (u64, u64),
}

impl<'a> GraphSource<'a> {
    /// Refuses a pair it cannot render as configured rather than rendering
    /// something else: an executor prepared at a rate other than the
    /// render's. (The pairing is `RenderGraph`'s check, made before this is
    /// built.) An executor's units are prepared for one rate
    /// on the control side, which is what `RenderGraph::prepare` and
    /// `RenderGraph::fork` are for; re-rating them here would be preparing
    /// them on the render thread.
    pub(crate) fn new(
        editor: &'a mut tutti_graph::Editor,
        executor: &'a mut tutti_graph::Executor,
        sample_rate: tutti_types::SampleRate,
        clock: &'a dyn RenderClock,
        file: &FileChannels<'_>,
        root: Option<&ChannelTopology>,
    ) -> crate::Result<Self> {
        let prepared = executor.prepare().sample_rate();
        if prepared.get() != sample_rate.get() {
            return Err(ConfigError::RateMismatch {
                prepared: prepared.get(),
                render: sample_rate.get(),
            }
            .into());
        }
        let max_block = executor.prepare().max_block().get();
        // Sized from the plan the first block renders, not from the editor's
        // spec: the spec may hold edits that were never committed, and the
        // executor renders only what was. (Nothing commits during the render,
        // so the plan applied here is the one every block runs.)
        executor.apply_pending();
        let before = (
            executor.dropped_event_count(),
            executor.recovered_event_count(),
        );
        let (global_inputs, outputs) = executor.plan().map_or_else(
            || {
                let topology = &editor.spec().topology;
                (usize::from(topology.inputs.count()), topology.outputs.len())
            },
            |p| (p.global_inputs() as usize, p.global_outputs()),
        );
        let planes = vec![vec![0.0f32; max_block]; outputs];
        let fold = file.fold_from(outputs, root)?;
        Ok(Self {
            editor,
            executor,
            clock,
            silence: vec![0.0; max_block],
            global_inputs,
            planes,
            fold,
            max_block,
            produced: Samples(0),
            before,
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

        // The fold was built for the file's width; a block of another is a
        // caller's bug, and `planar_to_interleaved` checks it in every build.
        let planes = &self.planes;
        let block = &mut out.samples_mut()[..block_size * ch];
        self.fold.planar_to_interleaved(
            |c| &planes[c][..block_size],
            InterleavedMut::new(block, self.fold.dst_layout()),
        );
        self.produced = Samples(self.produced.get() + block_size);
        block_size
    }

    fn produced(&self) -> Samples {
        self.produced
    }

    fn max_block(&self) -> usize {
        self.max_block
    }

    fn lost_events(&self) -> u64 {
        lost_over(
            self.before,
            (
                self.executor.dropped_event_count(),
                self.executor.recovered_event_count(),
            ),
        )
    }
}

/// Run `f` over the frame source for `graph`, then check the fork's health.
/// Returns what `f` did and the events the graph lost over the render; what
/// to do about them is the caller's ([`check_loss`]), made before a file is
/// committed and not for a cancelled render.
///
/// The one place a [`RenderGraph`] becomes a [`FrameSource`], so every entry
/// point renders and checks identically.
pub(crate) fn with_source<R>(
    graph: &mut RenderGraph,
    sample_rate: tutti_types::SampleRate,
    clock: &dyn RenderClock,
    file: &FileChannels<'_>,
    f: impl FnOnce(&mut dyn FrameSource) -> crate::Result<R>,
) -> crate::Result<(R, u64)> {
    // Again at render, as a real error: `editor_mut` can swap the editor
    // after `RenderGraph::new` checked it (see `RenderGraph`, "The pair").
    graph.check_paired()?;
    let root = graph.root_topology().cloned();
    let (editor, executor) = graph.parts_mut();
    let mut src = GraphSource::new(editor, executor, sample_rate, clock, file, root.as_ref())?;
    let rendered = f(&mut src)?;
    let lost = src.lost_events();
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
    Ok((rendered, lost))
}

/// The events lost between two readings of the executor's `(dropped,
/// recovered)` counts. A recovery can be of a refusal from before the first
/// reading, so the refusals still owed then (`dropped - recovered`) are
/// settled first: only recoveries past them took back a refusal made in the
/// span.
pub(crate) fn lost_over(before: (u64, u64), after: (u64, u64)) -> u64 {
    let owed = before.0.saturating_sub(before.1);
    let dropped = after.0.saturating_sub(before.0);
    let recovered = after.1.saturating_sub(before.1).saturating_sub(owed);
    dropped.saturating_sub(recovered)
}

/// Fails a render that lost events under `EventLossPolicy::Fail`
pub(crate) fn check_loss(policy: crate::EventLossPolicy, lost: u64) -> crate::Result<()> {
    if lost > 0 && policy == crate::EventLossPolicy::Fail {
        return Err(crate::Error::EventsDropped { count: lost });
    }
    Ok(())
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
            &mut out.samples_mut()[..n * usize::from(layout.count())],
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
///
/// After each block `control` is told how far the render is; if it answers
/// [`Flow::Stop`] the render ends there as [`Outcome::Cancelled`].
pub(crate) fn drive<F>(
    src: &mut dyn FrameSource,
    layout: ChannelLayout,
    plan: &crate::render::RenderPlan,
    control: &mut Control<'_>,
    mut consume: F,
) -> crate::Result<Outcome<()>>
where
    F: FnMut(Interleaved<'_>) -> crate::Result<()>,
{
    let ch = usize::from(layout.count());
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
        if control.report(src.produced(), plan.total) == Flow::Stop {
            return Ok(Outcome::Cancelled);
        }
    }

    Ok(Outcome::Finished(()))
}

#[cfg(test)]
mod tests;
