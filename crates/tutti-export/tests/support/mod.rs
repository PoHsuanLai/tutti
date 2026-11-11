//! The renders a test that has nothing to cancel and nobody to tell makes:
//! one output into a file, into memory, or into a normalized file, from the
//! [`ExportConfig`] that describes it, each [`tutti_export::render_one`] under
//! [`Control::none`] and its finished value.
//!
//! [`ExportConfig`] is this suite's own bundle (what the crate's public one,
//! retired in doc 019 step 5, was): one render and one file, filled in by
//! struct literal, and the [`Job`] and targets `render` takes built from it.
//! A field a use has no need of is ignored. A cancel cannot happen under that
//! control, so a `Cancelled` here is a bug in the pipeline and fails the
//! test.

// Each test crate includes this module and uses some of it.
#![allow(dead_code)]

use std::borrow::BorrowMut;
use std::path::{Path, PathBuf};

use tutti_codec::Format;
use tutti_dsp::DitherShape;
use tutti_export::{
    BufferTarget, Control, EventLossPolicy, FileTarget, Job, Normalize, NormalizeSpec, Outcome,
    Output, Produced, RenderGraph, Rendered, Resample, Result, Setup, Written, render_one,
};
use tutti_graph::{Editor, ForkMode, ForkTarget};
use tutti_transport::RenderClock;
use tutti_types::{ChannelLayout, ChannelTopology, SampleRate, Samples, Seconds};

/// The render stage: what to produce, and for how long.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RenderConfig {
    pub sample_rate: SampleRate,
    pub duration_seconds: Seconds,
    pub latency: Samples,
    pub tail: Samples,
    pub on_event_loss: EventLossPolicy,
}

impl Default for RenderConfig {
    fn default() -> Self {
        Self {
            sample_rate: SampleRate(44_100.0),
            duration_seconds: Seconds(0.0),
            latency: Samples(0),
            tail: Samples(0),
            on_event_loss: EventLossPolicy::default(),
        }
    }
}

/// The encode stage: what file to write.
#[derive(Debug, Clone, PartialEq)]
pub struct EncodeConfig {
    pub format: Format,
    pub channels: ChannelLayout,
    pub topology: Option<ChannelTopology>,
}

impl Default for EncodeConfig {
    fn default() -> Self {
        Self {
            format: Format::default(),
            channels: ChannelLayout::STEREO,
            topology: None,
        }
    }
}

/// One render and one file.
#[derive(Debug, Clone, PartialEq)]
pub struct ExportConfig {
    pub render: RenderConfig,
    pub encode: EncodeConfig,
    pub resample: Option<Resample>,
    pub dither: Option<DitherShape>,
}

impl Default for ExportConfig {
    fn default() -> Self {
        Self {
            render: RenderConfig::default(),
            encode: EncodeConfig::default(),
            resample: None,
            dither: Some(DitherShape::Triangular),
        }
    }
}

impl ExportConfig {
    /// The job this config describes: the duration rounded to the nearest
    /// frame, and a render of no audible span (a tail alone) the tail's length.
    pub fn job(&self) -> Result<Job> {
        let r = &self.render;
        let secs = r.duration_seconds.get();
        assert!(secs.is_finite() && secs >= 0.0, "a test names a length");
        let duration = Samples((secs * r.sample_rate.get()).round() as usize);
        let (length, tail) = if duration == Samples(0) {
            (r.tail, Samples(0))
        } else {
            (duration, r.tail)
        };
        Ok(Job::new(r.sample_rate, length, r.latency, tail)
            .expect("a test names a render")
            .with_event_loss(r.on_event_loss))
    }

    /// The file at `path`, normalized by `normalize` (measured on this
    /// output) when given.
    pub fn file_target(
        &self,
        path: impl Into<PathBuf>,
        normalize: Option<Normalize>,
    ) -> FileTarget {
        FileTarget::new(path, self.encode.format, self.encode.channels)
            .with_topology(self.encode.topology.clone())
            .with_resample(self.resample)
            .with_dither(self.dither)
            .with_normalize(normalize.map(NormalizeSpec::own))
    }

    /// The planes: `encode.channels` wide.
    pub fn buffer_target(&self) -> BufferTarget {
        BufferTarget::new(self.encode.channels).with_topology(self.encode.topology.clone())
    }

    pub fn buffers_job(&self) -> Result<Job> {
        self.job()
    }
}

/// [`tutti_export::write_buffers`] into the file `config` describes at `path`.
pub fn write_buffers(
    rendered: &Rendered,
    config: &ExportConfig,
    path: impl AsRef<Path>,
) -> Result<Written> {
    tutti_export::write_buffers(
        rendered,
        &config.file_target(path.as_ref(), None),
        config.render.on_event_loss,
    )
}

/// The master of `live`, forked offline at `rate` and assembled: what an
/// export gets from a live graph (the session's fork, without its pool or
/// root).
pub fn fork_offline(live: &Editor, rate: SampleRate) -> RenderGraph {
    let (editor, executor) = live
        .fork(
            ForkTarget::Master,
            ForkMode::Offline,
            RenderGraph::prepare(rate),
        )
        .expect("every node here is forkable");
    RenderGraph::assemble(editor, executor, Setup::new()).expect("forked together")
}

/// The value of a render nothing could have cancelled.
fn finished<T>(outcome: Outcome<T>) -> T {
    outcome
        .finished()
        .expect("a control with no token is never cancelled")
}

/// One output of `config`'s render, rendered under [`Control::none`].
fn render_output(
    mut graph: impl BorrowMut<RenderGraph>,
    job: tutti_export::Job,
    clock: &dyn RenderClock,
    output: Output,
) -> Result<Produced> {
    render_one(
        graph.borrow_mut(),
        &job,
        clock,
        output,
        &mut Control::none(),
    )
    .map(finished)
}

/// `config`'s render into a file at `path`.
pub fn render_to_file(
    graph: impl BorrowMut<RenderGraph>,
    config: &ExportConfig,
    clock: &dyn RenderClock,
    path: impl AsRef<Path>,
) -> Result<Written> {
    let job = config.job()?;
    let output = Output::master(config.file_target(path.as_ref(), None));
    render_output(graph, job, clock, output).map(|p| p.file().expect("a file output"))
}

/// `config`'s render into memory (not resampled, not dithered).
pub fn render_to_buffers(
    graph: impl BorrowMut<RenderGraph>,
    config: &ExportConfig,
    clock: &dyn RenderClock,
) -> Result<Rendered> {
    let job = config.buffers_job()?;
    let output = Output::master(config.buffer_target());
    render_output(graph, job, clock, output).map(|p| p.buffers().expect("a buffers output"))
}

/// `config`'s render into a file at `path`, normalized by `normalize`.
pub fn render_normalized_to_file(
    graph: impl BorrowMut<RenderGraph>,
    config: &ExportConfig,
    clock: &dyn RenderClock,
    normalize: Normalize,
    path: impl AsRef<Path>,
) -> Result<Written> {
    let job = config.job()?;
    let output = Output::master(config.file_target(path.as_ref(), Some(normalize)));
    render_output(graph, job, clock, output).map(|p| p.file().expect("a file output"))
}

/// `config`'s render into a file at `path` (normalized when `normalize` is
/// set), under `control`: the outcome as the render returns it, for a test that
/// cancels or watches.
pub fn render_file_under(
    mut graph: impl BorrowMut<RenderGraph>,
    config: &ExportConfig,
    clock: &dyn RenderClock,
    normalize: Option<Normalize>,
    path: impl AsRef<Path>,
    control: &mut Control<'_>,
) -> Result<Outcome<Written>> {
    let job = config.job()?;
    let output = Output::master(config.file_target(path.as_ref(), normalize));
    render_one(graph.borrow_mut(), &job, clock, output, control)
        .map(|o| o.map(|p| p.file().expect("a file output")))
}

/// `config`'s render into memory under `control`, as the render returns it.
pub fn render_buffers_under(
    mut graph: impl BorrowMut<RenderGraph>,
    config: &ExportConfig,
    clock: &dyn RenderClock,
    control: &mut Control<'_>,
) -> Result<Outcome<Rendered>> {
    let job = config.buffers_job()?;
    let output = Output::master(config.buffer_target());
    render_one(graph.borrow_mut(), &job, clock, output, control)
        .map(|o| o.map(|p| p.buffers().expect("a buffers output")))
}
