//! The renders a test that has nothing to cancel and nobody to tell makes:
//! one output into a file, into memory, or into a normalized file, from the
//! `ExportConfig` that describes it, each [`tutti_export::render_one`] under
//! [`Control::none`] and its finished value. A cancel cannot happen under that
//! control, so a `Cancelled` here is a bug in the pipeline and fails the
//! test.

// Each test crate includes this module and uses some of it.
#![allow(dead_code)]

use std::borrow::BorrowMut;
use std::path::Path;

use tutti_export::{
    Control, ExportConfig, Normalize, Outcome, Output, Produced, RenderClock, RenderGraph,
    Rendered, Result, Setup, Written, render_one,
};
use tutti_graph::{Editor, ForkMode, ForkTarget};
use tutti_types::SampleRate;

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
