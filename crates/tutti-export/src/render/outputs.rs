//! One render pass, N outputs (doc 019 section 4).
//!
//! The graph is rendered once. Every block goes to each [`Output`]'s lane: the
//! lane reads its feed (the graph's planes, or a tap's lanes drained from its
//! ring), folds it onto its target's layout, and hands the gated frames to its
//! sink. A streamed file output goes through its [`Chain`] (resample, dither,
//! encode) as the blocks arrive; a normalizing one keeps them in a [`Spill`]
//! and is measured and written after the render, from the spill, so the graph
//! never renders twice; planes are appended to memory.
//!
//! The stages after the render run over the outputs in order, and the files
//! are put in place last: every output's encoder is finalized into its
//! temporary first, then the token is checked once more, then the renames
//! (doc 019 section 6.2).

use std::path::PathBuf;

use tutti_graph::TapReader;
use tutti_measure::Loudness;
use tutti_transport::RenderClock;
use tutti_types::{ChannelLayout, Db, Fold, Interleaved, InterleavedMut, SampleRate, Samples};

use crate::control::{Control, Outcome, Stage, Weights};
use crate::encode::{
    Chain, ClippedFile, Resampler, open_file, output_rate, replay_through, resampler_for, written,
};
use crate::normalize::{Measurer, Normalize, chunk_frames};
use crate::output::{FileTarget, GainSource, NormalizeSpec, Output, Produced, Target, TargetError};
use crate::render::{
    FileChannels, REPLAY_BLOCK, RenderPlan, Replay, check_loss, drive, with_source,
};
use crate::spill::{Spill, SpillBudget};
use crate::{ConfigError, Error, Job, RenderGraph, Rendered, Result, Unmeasured};

/// A tap an output reads: the reader, and the planes one block of its lanes
/// is drained into.
struct TapFeed {
    reader: TapReader,
    planes: Vec<Vec<f32>>,
}

/// A file that waits for its gain: what the graph rendered for it, and what
/// decides the gain.
struct Waiting {
    target: FileTarget,
    spill: Spill,
    spec: NormalizeSpec,
}

/// Where a lane's gated frames go.
enum Sink {
    /// Straight into the file's chain as the blocks arrive.
    Stream { chain: Box<Chain> },
    /// Into a spill, to be measured and written once the render is over.
    Spill {
        chain: Box<Chain>,
        waiting: Box<Waiting>,
    },
    /// Onto planes in memory.
    Buffers { planes: Vec<Vec<f32>> },
}

/// One output of the pass.
struct Lane {
    /// The path of a file output.
    path: Option<PathBuf>,
    /// The feed's channels onto the target's.
    fold: Fold,
    /// Set for a tap feed; the graph's planes otherwise.
    tap: Option<TapFeed>,
    layout: ChannelLayout,
    /// The interleaved frames of one block's window, at the target's layout.
    scratch: Vec<f32>,
    sink: Sink,
}

/// Renders `graph` once into every output of `outputs`.
///
/// The body of [`render`](crate::render()); `spill_budget` is the memory the
/// normalizing outputs may keep before they move to files
/// ([`SPILL_MEMORY_BYTES`](crate::spill::SPILL_MEMORY_BYTES) in production; the
/// tests of the spill pass a small one).
pub(crate) fn render_outputs(
    graph: &mut RenderGraph,
    job: &Job,
    clock: &dyn RenderClock,
    outputs: Vec<Output>,
    control: &mut Control<'_>,
    spill_budget: usize,
) -> Result<Outcome<Vec<Produced>>> {
    check_outputs(&outputs)?;
    control.weigh(Weights::of(&outputs));
    let plan = RenderPlan::of(job);
    let (done, _lost) = with_source(graph, job.rate(), clock, |src| {
        let mut lanes = open_lanes(
            outputs,
            job,
            &plan,
            src.outputs(),
            src.root(),
            src.max_block(),
        )?;
        let mut budget = SpillBudget::new(spill_budget);

        let driven = drive(
            src,
            &plan,
            Stage::Rendering,
            control,
            job.on_event_loss(),
            |src, n, window| {
                let graph_planes = src.planes();
                for (i, lane) in lanes.iter_mut().enumerate() {
                    let planes: &[Vec<f32>] = match &mut lane.tap {
                        Some(tap) => {
                            drain_tap(i, tap, n)?;
                            &tap.planes
                        }
                        None => graph_planes,
                    };
                    if window.is_empty() {
                        continue;
                    }
                    let frames = window.len();
                    let samples = Samples(frames).interleaved_len(lane.layout);
                    let block = &mut lane.scratch[..samples];
                    lane.fold.planar_to_interleaved(
                        |c| &planes[c][window.clone()],
                        InterleavedMut::new(block, lane.layout),
                    );
                    let block = Interleaved::new(block, lane.layout);
                    match &mut lane.sink {
                        Sink::Stream { chain } => chain.block(block)?,
                        Sink::Spill { waiting, .. } => {
                            waiting.spill.push(block.samples(), &mut budget)?;
                        }
                        Sink::Buffers { planes } => block.append_to_planes(planes),
                    }
                }
                Ok(())
            },
        )?;
        if driven == Outcome::Cancelled {
            return Ok(Outcome::Cancelled);
        }
        // Before anything is measured or put in place: a unit that failed
        // rendered silence into what the outputs hold, and a render that lost
        // events is wrong.
        src.check_health()?;
        let lost = src.lost_events();
        check_loss(job.on_event_loss(), lost)?;

        finish(lanes, job, lost, control)
    })?;
    Ok(done)
}

/// What the outputs are refused for before any of them is opened: no outputs, a
/// zero width, a tap that is not as wide as its placement, a gain taken from an
/// output that does not measure, and two files with one path.
fn check_outputs(outputs: &[Output]) -> Result<()> {
    if outputs.is_empty() {
        return Err(Error::NoOutputs);
    }
    for (i, output) in outputs.iter().enumerate() {
        let target_error = |why| Error::Target { output: i, why };
        let (width, file) = match output.target() {
            Target::File(f) => (f.channels(), Some(f)),
            Target::Buffers(b) => (b.channels(), None),
        };
        if width.count() == 0 {
            return Err(Error::UnsupportedChannels);
        }
        if let crate::Feed::Tap { reader, lanes } = output.feed()
            && reader.layout() != lanes.topology().layout()
        {
            return Err(target_error(TargetError::Lanes {
                lanes: reader.layout(),
                placement: lanes.topology().layout(),
            }));
        }
        let Some(file) = file else { continue };
        if let Some(normalize) = file.normalize()
            && let GainSource::FromOutput(j) = normalize.gain()
        {
            let measures = outputs.get(j).is_some_and(|other| {
                j != i
                    && matches!(other.target(), Target::File(f)
                        if f.normalize().is_some_and(|n| n.gain() == GainSource::Own))
            });
            if !measures {
                return Err(target_error(TargetError::GainSource { index: j }));
            }
        }
        for (j, other) in outputs.iter().enumerate().take(i) {
            if let Target::File(f) = other.target()
                && f.path() == file.path()
            {
                return Err(target_error(TargetError::SamePath { other: j }));
            }
        }
    }
    Ok(())
}

/// Opens every output: its fold, its file (refused before it is touched when
/// the format cannot hold the render) or its planes.
fn open_lanes(
    outputs: Vec<Output>,
    job: &Job,
    plan: &RenderPlan,
    graph_outputs: usize,
    root: Option<&tutti_types::ChannelTopology>,
    max_block: usize,
) -> Result<Vec<Lane>> {
    let mut lanes = Vec::with_capacity(outputs.len());
    for (i, output) in outputs.into_iter().enumerate() {
        let (feed, target) = output.into_parts();
        let (layout, topology) = match &target {
            Target::File(f) => (f.channels(), f.topology()),
            Target::Buffers(b) => (b.channels(), b.topology()),
        };
        let channels = FileChannels { layout, topology };
        let (fold, tap) = match feed {
            crate::Feed::Master => (channels.fold_from(graph_outputs, root)?, None),
            crate::Feed::Tap { reader, lanes } => {
                let width = lanes.topology().layout();
                let fold = channels
                    .fold_from(usize::from(width.count()), Some(lanes.topology()))
                    .map_err(|e| match e {
                        Error::InvalidConfig(ConfigError::Placement(why)) => Error::Target {
                            output: i,
                            why: TargetError::Placement(why),
                        },
                        other => other,
                    })?;
                let planes = vec![vec![0.0; max_block]; reader.width()];
                (fold, Some(TapFeed { reader, planes }))
            }
        };
        let (sink, path) = match target {
            Target::File(file) => {
                let chain = Box::new(open_file(&file, plan, job.rate())?);
                let path = file.path().to_path_buf();
                match file.normalize() {
                    None => (Sink::Stream { chain }, Some(path)),
                    Some(spec) => (
                        Sink::Spill {
                            chain,
                            waiting: Box::new(Waiting {
                                spill: Spill::new(layout, &path),
                                target: file,
                                spec,
                            }),
                        },
                        Some(path),
                    ),
                }
            }
            Target::Buffers(_) => (
                Sink::Buffers {
                    planes: reserve_planes(plan, layout)?,
                },
                None,
            ),
        };
        lanes.push(Lane {
            path,
            fold,
            tap,
            layout,
            scratch: vec![0.0; max_block * usize::from(layout.count())],
            sink,
        });
    }
    Ok(lanes)
}

/// Empty planes with room for what the plan keeps.
///
/// Reserved fallibly: the duration is the caller's, and an hour of 7.1.4 (or a
/// typo) is a plain `Err` here where `with_capacity` would abort the process.
/// (`vec![Vec::with_capacity(n); ch]` would clone ONE empty Vec `ch` times,
/// and a clone does not carry capacity.)
fn reserve_planes(plan: &RenderPlan, layout: ChannelLayout) -> Result<Vec<Vec<f32>>> {
    let too_large = || Error::RenderTooLarge {
        frames: plan.output_length,
        channels: layout,
    };
    let ch = usize::from(layout.count());
    let mut planes: Vec<Vec<f32>> = Vec::new();
    planes.try_reserve_exact(ch).map_err(|_| too_large())?;
    for _ in 0..ch {
        let mut plane = Vec::new();
        plane
            .try_reserve_exact(plan.output_length.get())
            .map_err(|_| too_large())?;
        planes.push(plane);
    }
    Ok(planes)
}

/// Drains the `n` frames the block just rendered from a tap's rings into its
/// planes, every block, whether or not the gate keeps them.
fn drain_tap(output: usize, tap: &mut TapFeed, n: usize) -> Result<()> {
    let got = tap
        .reader
        .pop_planes(tap.planes.iter_mut().map(|p| &mut p[..n]), Samples(n));
    let lost = tap.reader.take_lost();
    if lost.get() > 0 {
        return Err(Error::TapOverrun { output, lost });
    }
    if got.get() < n {
        return Err(Error::TapStarved { output });
    }
    Ok(())
}

/// What a normalizing output was given: the gain, and the reading it came from
/// where this output measured itself.
#[derive(Clone, Copy)]
struct Chosen {
    gain: Db,
    measured: Option<Loudness>,
}

/// The stages after the render: measure, write, finalize, commit.
fn finish(
    mut lanes: Vec<Lane>,
    job: &Job,
    lost: u64,
    control: &mut Control<'_>,
) -> Result<Outcome<Vec<Produced>>> {
    let rate = job.rate();
    let mut chosen: Vec<Option<Chosen>> = vec![None; lanes.len()];
    let waiting: Vec<usize> = (0..lanes.len())
        .filter(|&i| matches!(lanes[i].sink, Sink::Spill { .. }))
        .collect();
    for &i in &waiting {
        if let Sink::Spill { waiting, .. } = &mut lanes[i].sink {
            waiting.spill.finish()?;
        }
    }

    if !waiting.is_empty() {
        let own: Vec<usize> = waiting
            .iter()
            .copied()
            .filter(|&i| waiting_of(&lanes[i]).spec.gain() == GainSource::Own)
            .collect();
        // Measuring: the outputs that choose their own gain, a spill each.
        let total = Samples(
            own.iter()
                .map(|&i| waiting_of(&lanes[i]).spill.frames().get())
                .sum(),
        );
        control.set_output(own.first().copied().unwrap_or(0));
        control.begin(Stage::Measuring, Some(total));
        if control.is_cancelled() {
            return Ok(Outcome::Cancelled);
        }
        let mut done = Samples(0);
        for &i in &own {
            control.set_output(i);
            let w = waiting_of(&lanes[i]);
            let normalize = w.spec.mode().within(&w.target);
            let Outcome::Finished((reading, at)) =
                measure(w, lanes[i].layout, rate, normalize, done, total, control)?
            else {
                return Ok(Outcome::Cancelled);
            };
            done = at;
            chosen[i] = Some(Chosen {
                gain: normalize.gain_for(&reading),
                measured: Some(reading),
            });
        }
        for &i in &waiting {
            if let GainSource::FromOutput(j) = waiting_of(&lanes[i]).spec.gain() {
                let from = chosen[j].expect("an output measures before one takes its gain");
                chosen[i] = Some(Chosen {
                    gain: from.gain,
                    measured: None,
                });
            }
        }

        // Writing: every waiting output, read back through its chain with its
        // gain.
        let total = Samples(
            waiting
                .iter()
                .map(|&i| waiting_of(&lanes[i]).spill.frames().get())
                .sum(),
        );
        control.set_output(waiting[0]);
        control.begin(Stage::Writing, Some(total));
        if control.is_cancelled() {
            return Ok(Outcome::Cancelled);
        }
        let mut done = Samples(0);
        for &i in &waiting {
            control.set_output(i);
            let Sink::Spill { chain, waiting } = &mut lanes[i].sink else {
                unreachable!("listed as waiting");
            };
            chain.set_gain(chosen[i].expect("chosen above").gain);
            let block = if waiting.target.resample().is_some() {
                chunk_frames(rate)
            } else {
                REPLAY_BLOCK
            };
            let mut replay = waiting.spill.replay()?;
            let Outcome::Finished(at) = replay_through(
                &mut replay,
                chain,
                block,
                Stage::Writing,
                done,
                total,
                control,
            )?
            else {
                return Ok(Outcome::Cancelled);
            };
            done = at;
        }
    }

    // Finalizing.
    let mut produced: Vec<Option<Produced>> = Vec::with_capacity(lanes.len());
    let mut files: Vec<PendingFile> = Vec::new();
    for (i, lane) in lanes.into_iter().enumerate() {
        match lane.sink {
            Sink::Buffers { planes } => {
                produced.push(Some(Produced::Buffers(Rendered::from_render(
                    planes, rate, lost,
                ))));
            }
            Sink::Stream { chain } | Sink::Spill { chain, .. } => {
                files.push(PendingFile {
                    index: i,
                    path: lane.path.expect("a file lane has a path"),
                    chain: *chain,
                });
                produced.push(None);
            }
        }
    }
    let Outcome::Finished(landed) = finalize_files(files, control)? else {
        return Ok(Outcome::Cancelled);
    };
    for (index, path, file) in landed {
        let mut w = written(&path, file, lost);
        if let Some(c) = chosen[index] {
            w.gain = Some(c.gain);
            w.measured = c.measured;
        }
        produced[index] = Some(Produced::File(w));
    }
    Ok(Outcome::Finished(
        produced
            .into_iter()
            .map(|p| p.expect("every output produced"))
            .collect(),
    ))
}

fn waiting_of(lane: &Lane) -> &Waiting {
    match &lane.sink {
        Sink::Spill { waiting, .. } => waiting,
        _ => unreachable!("listed as waiting"),
    }
}

/// Reads `waiting`'s spill at the file's rate through the meter, a chunk of at
/// most a second at a time, and returns the reading and how many frames of
/// `total` the measuring stage has now read.
///
/// The converter is the one the write pass uses, so what is measured is what
/// is written; the reading is refused when it is not a measurement of the
/// programme.
fn measure(
    waiting: &Waiting,
    layout: ChannelLayout,
    rate: SampleRate,
    normalize: Normalize,
    mut done: Samples,
    total: Samples,
    control: &mut Control<'_>,
) -> Result<Outcome<(Loudness, Samples)>> {
    let out_rate = output_rate(&waiting.target, rate);
    let unmeasurable = |reason: Unmeasured| Error::Unmeasurable {
        channels: layout,
        sample_rate: out_rate,
        reason,
    };
    let mut measurer =
        Measurer::new(out_rate, layout).ok_or_else(|| unmeasurable(Unmeasured::Format))?;
    let ch = usize::from(layout.count());
    let mut resampler: Option<Resampler> = resampler_for(&waiting.target, rate, ch)?;
    let mut staging: Vec<f32> = Vec::new();
    let mut replay = waiting.spill.replay()?;
    let chunk = chunk_frames(rate);
    let mut block = vec![0.0f32; chunk * ch];
    loop {
        if control.is_cancelled() {
            return Ok(Outcome::Cancelled);
        }
        let n = replay.fill(InterleavedMut::new(&mut block, layout))?;
        if n == 0 {
            break;
        }
        let frames = &block[..Samples(n).interleaved_len(layout)];
        match resampler.as_mut() {
            Some((rs, planes, out)) => {
                Interleaved::new(frames, layout).deinterleave_into(planes);
                rs.push(planes, out)?;
                feed_planes(&mut measurer, out, &mut staging, layout);
            }
            None => measurer.feed_interleaved(frames),
        }
        done = Samples(done.get() + n);
        control.advance(Stage::Measuring, done, total);
    }
    // The loop above ends on a read that found nothing left, after its check
    // at the top: a cancel on the last chunk's reading is seen there.
    if let Some((rs, _, out)) = resampler.as_mut() {
        rs.finish(out)?;
        feed_planes(&mut measurer, out, &mut staging, layout);
    }
    let reading = measurer.finish();
    normalize.check(&reading).map_err(unmeasurable)?;
    Ok(Outcome::Finished((reading, done)))
}

/// Interleaves what the converter produced into the meter, and clears it.
fn feed_planes(
    measurer: &mut Measurer,
    out: &mut [Vec<f32>],
    staging: &mut Vec<f32>,
    layout: ChannelLayout,
) {
    let frames = Samples(out.first().map_or(0, Vec::len));
    staging.clear();
    staging.resize(frames.interleaved_len(layout), 0.0);
    InterleavedMut::new(staging, layout).interleave_from(out, 0);
    measurer.feed_interleaved(staging);
    for p in out.iter_mut() {
        p.clear();
    }
}

/// A file output whose frames are all written: its chain, to be finalized.
pub(crate) struct PendingFile {
    /// The output's index in the list the render was given.
    pub(crate) index: usize,
    pub(crate) path: PathBuf,
    pub(crate) chain: Chain,
}

/// Finalizes every file into its temporary, then puts them in place
/// (doc 019 section 6.2): the token is checked before each output's finalize
/// (between outputs) and once more after the last, before the first rename.
/// The renames are the one step the token does not reach.
///
/// Returns each file that landed with what its chain counted, in the order
/// given. A rename that fails after others landed is
/// [`Error::NotCommitted`], naming what landed, which is kept: nothing is
/// rolled back. One that fails before any landed is the rename's own error.
/// Every file that did not land is removed.
pub(crate) fn finalize_files(
    files: Vec<PendingFile>,
    control: &mut Control<'_>,
) -> Result<Outcome<Vec<(usize, PathBuf, ClippedFile)>>> {
    let count = files.len();
    control.set_output(files.first().map_or(0, |f| f.index));
    control.begin(Stage::Finalizing, None);
    if control.is_cancelled() {
        return Ok(Outcome::Cancelled);
    }
    let mut closed = Vec::with_capacity(count);
    for (k, file) in files.into_iter().enumerate() {
        if k > 0 && control.is_cancelled() {
            return Ok(Outcome::Cancelled);
        }
        control.set_output(file.index);
        closed.push((file.index, file.path, file.chain.finalize()?));
        if k + 1 < count {
            control.at(Stage::Finalizing, 0.5 * (k + 1) as f32 / count as f32);
        }
    }
    if count > 0 {
        control.at(Stage::Finalizing, 0.5);
        if control.is_cancelled() {
            return Ok(Outcome::Cancelled);
        }
    }
    let mut landed = Vec::with_capacity(count);
    for (index, path, file) in closed {
        match file.commit() {
            Ok(clipped) => landed.push((index, path, clipped)),
            Err(cause) if landed.is_empty() => return Err(cause.into()),
            Err(cause) => {
                return Err(Error::NotCommitted {
                    landed: landed.into_iter().map(|(_, path, _)| path).collect(),
                    cause,
                });
            }
        }
    }
    // After the last rename: the only reading that is exactly 1.0.
    control.at(Stage::Finalizing, 1.0);
    Ok(Outcome::Finished(landed))
}

#[cfg(test)]
mod tests;
