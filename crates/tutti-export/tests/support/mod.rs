//! The three render entry points for a test that has nothing to cancel and
//! nobody to tell: each is the crate's own function called with
//! [`Control::none`], and its finished value. A cancel cannot happen under
//! that control, so a `Cancelled` here is a bug in the pipeline and fails the
//! test.

// Each test crate includes this module and uses some of it.
#![allow(dead_code)]

use std::borrow::BorrowMut;
use std::path::Path;

use tutti_export::{
    Control, ExportConfig, Normalize, Outcome, RenderClock, RenderGraph, Rendered, Result, Written,
};

/// The value of a render nothing could have cancelled.
fn finished<T>(outcome: Outcome<T>) -> T {
    outcome
        .finished()
        .expect("a control with no token is never cancelled")
}

/// [`tutti_export::render_to_file`] under [`Control::none`].
pub fn render_to_file(
    graph: impl BorrowMut<RenderGraph>,
    config: &ExportConfig,
    clock: &dyn RenderClock,
    path: impl AsRef<Path>,
) -> Result<Written> {
    tutti_export::render_to_file(graph, config, clock, path, &mut Control::none()).map(finished)
}

/// [`tutti_export::render_to_buffers`] under [`Control::none`].
pub fn render_to_buffers(
    graph: impl BorrowMut<RenderGraph>,
    config: &ExportConfig,
    clock: &dyn RenderClock,
) -> Result<Rendered> {
    tutti_export::render_to_buffers(graph, config, clock, &mut Control::none()).map(finished)
}

/// [`tutti_export::render_normalized_to_file`] under [`Control::none`].
pub fn render_normalized_to_file(
    graph: impl BorrowMut<RenderGraph>,
    config: &ExportConfig,
    clock: &dyn RenderClock,
    normalize: Normalize,
    path: impl AsRef<Path>,
) -> Result<Written> {
    tutti_export::render_normalized_to_file(
        graph,
        config,
        clock,
        normalize,
        path,
        &mut Control::none(),
    )
    .map(finished)
}
