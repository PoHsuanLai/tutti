//! The render source: a graph presented as a block-at-a-time source of planes.
//!
//! [`GraphSource`] block-renders a `tutti_graph` executor into one plane per
//! global output and advances the caller's [`RenderClock`] in lockstep.
//! [`drive`] pulls it block by block, applying the latency and length gate,
//! and hands each block to the outputs (the fold onto each one's layout is
//! theirs, [`FileChannels::fold_from`]), so the graph is stepped exactly once
//! per block however many outputs there are. [`PlaneSource`] replays planes a
//! caller already holds, through the same [`Replay`] the spilled audio of a
//! normalized output is read back by.
//!
//! The frame width is a runtime value carried by
//! `Interleaved`, never a `const CH`. A width in the type can only carry one that is a property of the
//! *code*; the destination width here is a property of the caller's target, so
//! any positive count renders — 3- and 5-wide masters included.
//!
//! A fold from the graph's real output width onto a target's
//! ([`tutti_types::Fold`], built once, before the first block) downmixes a
//! wider graph rather than truncating it, and zero-fills a narrower one
//! rather than duplicating.

use crate::control::{Control, Outcome, Stage};
use crate::render::BlockCursor;
use crate::{ConfigError, RenderGraph};
use std::ops::Range;
use tutti_transport::RenderClock;
use tutti_types::{ChannelLayout, ChannelTopology, Fold, InterleavedMut, Samples};

/// What a target's channels are, for the fold from the graph's outputs or a
/// tap's lanes: its width, and the speakers they feed when it states them.
pub(crate) struct FileChannels<'a> {
    pub(crate) layout: ChannelLayout,
    pub(crate) topology: Option<&'a ChannelTopology>,
}

impl FileChannels<'_> {
    /// The fold from a source of `outputs` channels. The source is what it
    /// declared (`root`: the graph's root, or a tap's placement), or else the engine's order for its width (a bare 4 is
    /// quad); the file the topology it states or else the same assumption for
    /// its width; where either has no order, or the file states discrete
    /// channels, the channels go by index ([`Fold::widths`],
    /// [`Fold::declared`], [`Fold::by_index`]).
    ///
    /// # Errors
    ///
    /// [`ConfigError::Placement`] when the file's topology is not as wide as
    /// the file or cannot take the graph's channels, or the declared root is
    /// not as wide as the graph, or the graph has more channels than a layout
    /// holds ([`FoldError::TooWide`](tutti_types::FoldError::TooWide)).
    pub(crate) fn fold_from(
        &self,
        outputs: usize,
        root: Option<&ChannelTopology>,
    ) -> crate::Result<Fold> {
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
            None => ChannelTopology::for_width(graph),
        };
        // A file that names no speaker takes the graph's channels by index,
        // as it says; `between` never pads by a guess.
        if topology.is_discrete() {
            return Fold::by_index(from.layout(), topology.layout())
                .map_err(|e| ConfigError::Placement(e).into());
        }
        Fold::between(&from, topology).map_err(|e| ConfigError::Placement(e).into())
    }
}

/// The render source for the graph: a `tutti_graph::Executor` (a
/// fork, or a pair built for the render) rendered one block at a time.
///
/// One block per [`render_block`](Self::render_block), the clock advanced by
/// exactly the frames produced. The executor reads the transport from each
/// block's `Env`, so this hands it one — through
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
    /// What the graph's output channels are, when it declared it.
    root: Option<ChannelTopology>,
    max_block: usize,
    produced: Samples,
    /// The executor's dropped and recovered counts the render counts from
    /// ([`RenderGraph`]'s settled figures: where the last render ended, or
    /// where the pair was wrapped); kept apart, because a recovery in the
    /// render may be of a refusal before it.
    before: (u64, u64),
}

impl<'a> GraphSource<'a> {
    /// Refuses a pair it cannot render as configured rather than rendering
    /// something else: an executor prepared at a rate other than the
    /// render's. (The pairing is `RenderGraph::assemble`'s check.) An executor's units are prepared for one rate
    /// on the control side, which is what `RenderGraph::prepare` and
    /// the session's fork are for; re-rating them here would be preparing
    /// them on the render thread.
    pub(crate) fn new(
        editor: &'a mut tutti_graph::Editor,
        executor: &'a mut tutti_graph::Executor,
        sample_rate: tutti_types::SampleRate,
        clock: &'a dyn RenderClock,
        root: Option<ChannelTopology>,
        before: (u64, u64),
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
        //
        // Installing a commit can itself drop events (a note-off closing into
        // a full delay), so the loss is counted from `before`, which predates
        // this call, and not from here.
        executor.apply_pending();
        let (global_inputs, outputs) = executor.plan().map_or_else(
            || {
                let topology = &editor.spec().topology;
                (usize::from(topology.inputs.count()), topology.outputs.len())
            },
            |p| (p.global_inputs() as usize, p.global_outputs()),
        );
        let planes = vec![vec![0.0f32; max_block]; outputs];
        Ok(Self {
            editor,
            executor,
            clock,
            silence: vec![0.0; max_block],
            global_inputs,
            planes,
            root,
            max_block,
            produced: Samples(0),
            before,
        })
    }
}

impl GraphSource<'_> {
    /// The executor's `(dropped, recovered)` event counts now.
    fn counts(&self) -> (u64, u64) {
        (
            self.executor.dropped_event_count(),
            self.executor.recovered_event_count(),
        )
    }

    /// The graph's global output channels: the planes of a block.
    pub(crate) fn outputs(&self) -> usize {
        self.planes.len()
    }

    /// What the graph's outputs are, when it declared it.
    pub(crate) fn root(&self) -> Option<&ChannelTopology> {
        self.root.as_ref()
    }

    /// The planes of the block last rendered: one per global output, the first
    /// frames of each valid.
    pub(crate) fn planes(&self) -> &[Vec<f32>] {
        &self.planes
    }

    /// Frames handed out so far.
    pub(crate) fn produced(&self) -> Samples {
        self.produced
    }

    /// The most frames one [`render_block`](Self::render_block) produces: the
    /// block the graph was prepared for. Pulling it 64 frames at a time would
    /// run its per-block walk sixteen times as often as it needs to.
    pub(crate) fn max_block(&self) -> usize {
        self.max_block
    }

    /// Renders up to `want` frames (never more than [`max_block`](Self::max_block))
    /// into [`planes`](Self::planes) and returns how many. A render never
    /// starves: the graph produces a full block on demand, so the only `0`
    /// this returns is for a zero-length request.
    ///
    /// That is the offline/live split in one line. A `Source`'s pull answers
    /// starved, ended or failed, because it is polled until dry on a clock of
    /// its own. An offline render is driven to a known frame count, so no
    /// verdict is asked for and this is not a `Source` (doc 025, §5.4).
    pub(crate) fn render_block(&mut self, want: usize) -> usize {
        let block_size = want.min(self.max_block);
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
        self.produced = Samples(self.produced.get() + block_size);
        block_size
    }

    /// Events the graph lost since the render began (`Executor::lost_event_count`):
    /// not a note-off it refused and sent again.
    pub(crate) fn lost_events(&self) -> u64 {
        lost_over(self.before, self.counts())
    }

    /// Whether the units behind this source are still healthy: a forked unit
    /// (a hosted plugin) that failed mid-render rendered silence from then
    /// on, and `process` has no error channel to say so.
    ///
    /// Asked before a file is put in place, so a failed render never replaces
    /// the user's file.
    ///
    /// # Errors
    ///
    /// [`Error::ForkFailed`](crate::Error::ForkFailed) naming the first unit
    /// that failed.
    pub(crate) fn check_health(&self) -> crate::Result<()> {
        self.editor
            .fork_health()
            .map_err(|fault| crate::Error::ForkFailed {
                key: fault.key,
                kind: fault.kind,
                cause: fault.cause,
            })
    }
}

/// Run `f` over the frame source for `graph`, then check the fork's health.
/// Returns what `f` did and the events the graph lost over the render; what
/// to do about them is the caller's ([`check_loss`]), made before a file is
/// committed and not for a cancelled render.
///
/// The one place a [`RenderGraph`] becomes a [`GraphSource`], so every render
/// is made and checked identically.
pub(crate) fn with_source<T>(
    graph: &mut RenderGraph,
    sample_rate: tutti_types::SampleRate,
    clock: &dyn RenderClock,
    f: impl FnOnce(&mut GraphSource<'_>) -> crate::Result<Outcome<T>>,
) -> crate::Result<(Outcome<T>, u64)> {
    let root = graph.root_topology().cloned();
    let settled = graph.settled();
    let (editor, executor) = graph.parts_mut();
    let mut src = GraphSource::new(editor, executor, sample_rate, clock, root, settled)?;
    let rendered = f(&mut src);
    // The next render counts from here, whether this one finished or not.
    let counts = src.counts();
    let lost = src.lost_events();
    // The fallback: the render asks before it puts a file in place or returns
    // planes (`render`); this one covers any caller that did not.
    // A render the caller cancelled is not judged, so a fault that came
    // before the cancel does not turn it into a failure.
    let health = match &rendered {
        Ok(Outcome::Finished(_)) | Err(_) => src.check_health(),
        Ok(Outcome::Cancelled) => Ok(()),
    };
    graph.settle(counts);
    let rendered = rendered?;
    health?;
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

/// Audio read back in blocks: planes a caller holds, or what a normalized
/// output spilled while the graph rendered.
pub(crate) trait Replay {
    /// Fills the leading frames of `out`, returning how many frames were
    /// written; `0` is the end.
    fn fill(&mut self, out: InterleavedMut<'_>) -> crate::Result<usize>;
}

/// Replays already-rendered planes.
///
/// Planes of the file's own width replay as they are. Any other width is
/// folded onto the file's through the [`Fold`] the caller built
/// ([`FileChannels::fold_from`]), the way the graph's outputs are: a wider
/// render is placed by position and a narrower one by the engine's order,
/// never truncated or zero-padded.
pub(crate) struct PlaneSource<'a> {
    planes: &'a [Vec<f32>],
    fold: Option<Fold>,
    pos: Samples,
}

impl<'a> PlaneSource<'a> {
    pub(crate) fn new(planes: &'a [Vec<f32>], fold: Option<Fold>) -> Self {
        Self {
            planes,
            fold,
            pos: Samples(0),
        }
    }
}

impl Replay for PlaneSource<'_> {
    fn fill(&mut self, mut out: InterleavedMut<'_>) -> crate::Result<usize> {
        let available = self
            .planes
            .first()
            .map_or(0, |p| p.len().saturating_sub(self.pos.get()));
        let n = out.len().min(available);
        let layout = out.layout();
        let block = &mut out.samples_mut()[..n * usize::from(layout.count())];
        let pos = self.pos.get();
        match &self.fold {
            Some(fold) => {
                let planes = self.planes;
                fold.planar_to_interleaved(
                    |c| &planes[c][pos..pos + n],
                    InterleavedMut::new(block, layout),
                );
            }
            // `interleave_from` writes every sample of every frame (rather
            // than leaving whatever the buffer held), which matters because
            // the caller's block is reused across iterations.
            None => InterleavedMut::new(block, layout).interleave_from(self.planes, pos),
        }
        self.pos = Samples(self.pos.get() + n);
        Ok(n)
    }
}

/// The frames the replay of planes hands out per block: 64, which only paces
/// the encoder.
///
/// Not raised to the graph's block: Ogg Vorbis's bytes depend on how its
/// input is split into `encode_audio_block` calls, so this size is part of
/// what a replayed Ogg export writes, and a 1024-frame replay measured no
/// faster for the PCM formats.
pub(crate) const REPLAY_BLOCK: usize = 64;

/// Pull the whole render out of `src`, gating each block, handing every block
/// to `consume`.
///
/// The gate (latency trim + output-length cap) lives here rather than on a sink
/// because it needs counters that span blocks. `consume` is called for every
/// block, with the block's planes (the first `n` frames of each valid) and the
/// frame range of it the gate keeps: empty for a block before the latency or
/// past the length, which a tap still has to drain.
///
/// The token is checked before every block and after the progress a block
/// delivered (a callback may cancel; the last block's too), and a cancelled
/// render ends there as [`Outcome::Cancelled`]. `stage` is the stage the
/// frames count in.
///
/// Under [`EventLossPolicy::Fail`](crate::EventLossPolicy::Fail) the render
/// stops at the first block that has lost an event, after that block's
/// progress and cancel check (a render that was cancelled is not judged), with
/// [`Error::EventsDropped`](crate::Error::EventsDropped) counting what had
/// been lost by then: a render that is going to fail does not spend the rest
/// of its length first.
pub(crate) fn drive<F>(
    src: &mut GraphSource<'_>,
    plan: &crate::render::RenderPlan,
    stage: Stage,
    control: &mut Control<'_>,
    on_loss: crate::EventLossPolicy,
    mut consume: F,
) -> crate::Result<Outcome<()>>
where
    F: FnMut(&GraphSource<'_>, usize, Range<usize>) -> crate::Result<()>,
{
    let max_block = src.max_block().max(1);
    let mut kept = Samples(0);

    control.begin(stage, Some(plan.total));
    while src.produced() < plan.total {
        if control.is_cancelled() {
            return Ok(Outcome::Cancelled);
        }
        // `Samples` has no `Sub` on purpose — an unsigned count that wraps is a
        // buffer size that reads off the end of the world. `remaining_after` is
        // the named, saturating form.
        let want = plan
            .total
            .remaining_after(src.produced())
            .min(Samples(max_block));
        let block_start = src.produced();
        let n = src.render_block(want.get());
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
        let kept_now = window.len();
        consume(src, n, window)?;
        kept = Samples(kept.get() + kept_now);
        control.advance(stage, src.produced(), plan.total);
        if control.is_cancelled() {
            return Ok(Outcome::Cancelled);
        }
        // After the check, so a render the caller cancelled is not judged.
        check_loss(on_loss, src.lost_events())?;
    }

    Ok(Outcome::Finished(()))
}

#[cfg(test)]
mod tests;
