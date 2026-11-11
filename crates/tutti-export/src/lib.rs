#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod error;
pub use error::{ConfigError, Error, Result, Unmeasured};
/// The container and its settings, and the sample widths, which are
/// `tutti-codec`'s. Re-exported so a caller configuring an export names them
/// from here.
pub use tutti_codec::{FlacLevel, Format, IntDepth, Pcm, Quality};
pub use tutti_types::ChannelLayout;

mod control;
use control::Weights;
pub use control::{CancelToken, Control, Outcome, Progress, Stage};

mod job;
pub use job::{Job, JobError, MAX_FRAMES};

mod rendered;
pub use rendered::{Rendered, RenderedError};

mod output;
pub use output::{
    BufferTarget, Feed, FileTarget, GainSource, NormalizeSpec, Output, Placement, Produced, Target,
    TargetError,
};

mod config;
pub use config::{EncodeConfig, EventLossPolicy, ExportConfig, RenderConfig, Resample};

mod normalize;
pub use normalize::Normalize;

mod spill;

mod graph;
pub use graph::{GRAPH_MAX_BLOCK, RenderGraph};
/// Frame-count arithmetic — pure, and public so a caller can size a render
/// before committing to one.
pub use render::plan::{beats_to_seconds, duration_to_frames};

pub(crate) mod encode;
pub(crate) mod process;
pub(crate) mod render;

pub use process::ChunkSize;
/// The clock a render advances. Re-exported so callers can name it without
/// depending on `tutti-transport` directly; `FrozenClock` is the "this graph has no
/// transport" answer.
pub use tutti_transport::{FrozenClock, RenderClock};

use std::path::{Path, PathBuf};
use tutti_measure::Loudness;
use tutti_types::Db;

/// A file an export wrote, from a file [`Output`] of [`render()`]
/// ([`Produced::File`]) or [`write_buffers`].
#[derive(Debug, Clone, PartialEq)]
pub struct Written {
    /// Where it landed — the `path` the entry point was given.
    pub path: PathBuf,
    /// Size on disk after finalization, in bytes. `0` if the file could not
    /// be stat'd.
    pub bytes: u64,
    /// Events the graph lost during the render that produced the file;
    /// nonzero only under [`EventLossPolicy::Allow`] (under `Fail` the render
    /// is an error instead).
    pub dropped_events: u64,
    /// Samples the quantizer clipped: those past full scale (after any gain,
    /// resample and dither) written to an integer format, which the encoder
    /// clamps to it, **and non-finite ones** (a NaN is written as 0, an
    /// infinity as full scale). `0` for a format that does not quantize (`Pcm::Float32`,
    /// and Ogg Vorbis, whose encoder takes the float stream). A normalizing
    /// export keeps its target and ceiling one LSB plus the dither amplitude
    /// below full scale for an integer format, so this stays `0` unless the
    /// programme itself was past full scale.
    pub clipped_samples: u64,
    /// The gain a normalizing output applied ([`FileTarget::with_normalize`]);
    /// `None` for the others, so `None` means the file is as rendered. Chosen
    /// against the target or ceiling after an integer format's headroom was
    /// taken off it, so for [`Normalize::Peak`] it is
    /// `target - measured.true_peak` of that lowered target. An output that
    /// took another's gain ([`GainSource::FromOutput`]) reports that gain, the
    /// same figure the output it followed reports.
    pub gain: Option<Db>,
    /// What a normalizing output read from the audio before it applied
    /// [`gain`](Self::gain): the programme at the file's rate (after any
    /// resample, which moves the true peak), measured once. `None` where
    /// `gain` is, and for an output that took another's gain: it measured
    /// nothing, and the reading is of the output it followed.
    pub measured: Option<Loudness>,
}

/// The frame width an export config asks for, as the interleave stride.
///
/// The one place a [`ChannelLayout`] becomes the `usize` stride the render and
/// the encoders use, and zero is the only rejected width. That the check is
/// a bound rather than a list of enumerated widths is what lets a 3- or 5-wide
/// master export at all: nothing here is generic over the count.
fn frame_width(layout: ChannelLayout) -> Result<usize> {
    match layout.count() {
        0 => Err(Error::UnsupportedChannels),
        n => Ok(n as usize),
    }
}

/// Renders `graph` once into every one of `outputs`, and writes or returns
/// what each becomes ([`Produced`], in the order given).
///
/// One pass: the graph is stepped once per block however many outputs there
/// are, and each output reads its [`Feed`] (the graph's outputs, or a plan tap)
/// folds it onto its [`Target`]'s layout, and writes it. An output that does
/// not normalize is streamed through its resample, dither and encoder as the
/// blocks arrive and holds no PCM; one that does keeps its audio (in memory up
/// to 512 MiB across the render, past it in a temporary `f32` file beside its
/// target, removed afterwards), is measured at its file's rate and written
/// again with the gain, and the graph is never rendered twice. An output that
/// takes another's gain ([`GainSource::FromOutput`]) is written with it, so a
/// set of stems keeps the master's balance.
///
/// The render is the `job`'s: its rate (the graph must be prepared at it:
/// [`RenderGraph::prepare`] or [`RenderGraph::fork`]), the audible span, the
/// latency trimmed from the head of every output, the tail kept past the end
/// and what to do about a dropped event. `clock` is advanced once per block,
/// after the graph processes ([`RenderClock::render_graph`]); pass
/// [`FrozenClock`] for a graph with no time-dependent nodes. `graph` is
/// borrowed, so a graph holding a unit whose teardown is main-thread-only (an
/// in-process VST2 plugin's fork) and rendered on a worker is brought back and
/// dropped on the main thread.
///
/// Every file is written to a temporary beside its path and moved into place
/// at the end, after every output's encoder has been finalized and the fork
/// and the event loss have been checked: a render that fails, or is
/// cancelled, leaves every path as it was and no temporary. The renames are
/// the one step that is not undone: if one fails after others landed the
/// error is [`Error::NotCommitted`] and the files that landed are kept.
///
/// `control` is how a host watches and stops the render: it is told the
/// [`Stage`] and fraction as the render goes (`Rendering`, then with a
/// normalizing output `Measuring` and `Writing`, then `Finalizing`), and a
/// [`CancelToken`] it holds is checked at every point of doc 019 section 6.2
/// that this render has: before each block, between the stages, in each chunk
/// of the measurement and the write, before each output is finalized and once
/// more before the first rename. A cancelled render is [`Outcome::Cancelled`],
/// not an error. Pass [`Control::none`] for a render nobody watches.
///
/// # Errors
///
/// [`Error::NoOutputs`] for an empty list; [`Error::UnsupportedChannels`] for
/// a target of no channels; [`Error::Target`] for an output that cannot be
/// rendered as given ([`TargetError`]: a tap whose lanes are not as wide as
/// its placement or cannot be placed on the target, a gain taken from an
/// output that does not measure, two outputs with one path);
/// [`Error::InvalidConfig`] if the graph was prepared at another rate or its
/// editor does not feed its executor, or the graph's channels cannot be placed
/// on a target's ([`ConfigError::Placement`]); an encoder's refusal of a format
/// and width ([`Error::Encode`], before anything is rendered);
/// [`Error::RenderTooLarge`] for planes that cannot be held;
/// [`Error::ForkFailed`] if a forked node (a hosted plugin) failed during the
/// render and [`Error::EventsDropped`] if the graph lost an event and the
/// job's policy is [`EventLossPolicy::Fail`] (the default), both checked
/// before anything is put in place;
/// [`Error::TapOverrun`] and [`Error::TapStarved`] from a tap;
/// [`Error::Unmeasurable`] for a normalizing output whose audio has no reading
/// to take a gain from (silent, under 400 ms or under the loudness gate for
/// [`Normalize::Lufs`], at the true-peak floor for [`Normalize::Peak`], or a
/// width or rate the meter does not take); [`Error::Resample`];
/// [`Error::Io`] and [`Error::NotCommitted`].
pub fn render(
    graph: &mut RenderGraph,
    job: &Job,
    clock: &dyn RenderClock,
    outputs: Vec<Output>,
    control: &mut Control<'_>,
) -> Result<Outcome<Vec<Produced>>> {
    render::outputs::render_outputs(
        graph,
        job,
        clock,
        outputs,
        control,
        spill::SPILL_MEMORY_BYTES,
    )
}

/// [`render()`] for the one output: the helper that keeps `vec![output]` out of
/// the call of a render with one target.
///
/// # Errors
///
/// As [`render()`].
pub fn render_one(
    graph: &mut RenderGraph,
    job: &Job,
    clock: &dyn RenderClock,
    output: Output,
    control: &mut Control<'_>,
) -> Result<Outcome<Produced>> {
    Ok(render(graph, job, clock, vec![output], control)?.map(the_one))
}

/// What a render of one output produced.
fn the_one(mut produced: Vec<Produced>) -> Produced {
    produced.pop().expect("one output produces one result")
}

/// Writes already-rendered audio to `path`.
///
/// With a [`BufferTarget`] output and [`Rendered::apply_gain`] this makes
/// measure-then-apply possible by hand: render to planes, measure
/// ([`Normalize::gain_for_rendered`]), apply a gain, write.
///
/// `config.encode` is honoured as-is, `channels` and `topology` included:
/// planes of another width than `config.encode.channels` are folded onto it
/// the way a graph's outputs are (the engine's order for the plane width, the
/// file's `topology` for the file; a 5.1 render written as stereo carries its
/// centre and surrounds at -3 dB), and planes of the file's own width are
/// written as they are. `config.render` is not consulted — the
/// frames already exist and carry their own rate in [`Rendered::sample_rate`] —
/// but `config.resample` still applies, so a caller can convert on the way out.
/// Dither is applied here, at the real depth and after any resample, which is
/// the only point where one LSB is known.
///
/// # Errors
///
/// [`Error::UnsupportedChannels`] for a zero-channel `config.encode.channels`;
/// [`Error::InvalidConfig`] ([`ConfigError::Placement`]) for a plane width the
/// fold cannot place onto the file's — a channel is never dropped to make it
/// fit; [`Error::Resample`] or [`Error::Encode`] from the resample and encode
/// stages.
pub fn write_buffers(
    rendered: &Rendered,
    config: &ExportConfig,
    path: impl AsRef<Path>,
) -> Result<Written> {
    frame_width(config.encode.channels)?;
    // No render stage: the write is the whole of it, in the bands a
    // normalized render gives its write.
    let mut control = Control::none();
    control.weigh(Weights::Normalized);
    let target = config.file_target(path.as_ref(), None);
    // The planes carry the events their render lost: judged by this config's
    // policy, before anything is written.
    render::check_loss(config.render.on_event_loss, rendered.dropped_events())?;
    let Outcome::Finished(chain) = encode::write_planes(rendered, &target, &mut control)? else {
        unreachable!("a control with no token is never cancelled");
    };
    let files = vec![render::outputs::PendingFile {
        index: 0,
        path: target.path().to_path_buf(),
        chain,
    }];
    let Outcome::Finished(mut landed) = render::outputs::finalize_files(files, &mut control)?
    else {
        unreachable!("a control with no token is never cancelled");
    };
    let (_, path, file) = landed.remove(0);
    Ok(encode::written(&path, file, rendered.dropped_events()))
}
