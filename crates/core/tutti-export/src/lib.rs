//! # tutti-export
//!
//! Offline audio export for the Tutti audio engine.
//!
//! ## Two starting points
//!
//! ```ignore
//! use tutti_export::{Export, Normalize, BitDepth};
//!
//! // From a Tutti graph:
//! Export::graph(net, 44100.0)
//!     .duration_seconds(10.0)
//!     .bit_depth(BitDepth::Int24)
//!     .normalize(Normalize::lufs(-14.0))
//!     .to_file("master.flac")    // format inferred from extension
//!     .run()?;
//!
//! // From already-rendered buffers:
//! Export::buffers(left, right, 44100.0)
//!     .to_file("clip.wav")
//!     .run()?;
//! ```
//!
//! ## Two execution modes
//!
//! Every terminal returns a [`Run<T>`]. Pick one of:
//!
//! - [`Run::run`] — block this thread.
//! - [`Run::spawn`] — run on a worker thread; poll via [`Handle`].

mod error;
pub use error::{Error, Result};

mod progress;
pub use progress::Phase;

mod options;
pub use options::{AudioFormat, BitDepth, Dither, Flac, Normalize, Ogg};
pub use tutti_types::ChannelLayout;

mod run;
pub use run::{Handle, Rendered, Run, State, Written};

mod buffer;
mod graph;
pub use buffer::BufferExport;
pub use graph::GraphExport;
// The loop-range type is the engine's one validated `LoopRange` (a beat span
// with `end > start` checked once), re-exported so `GraphExport::loop_range`
// callers can name it without depending on `tutti_core` directly.
pub use tutti_core::transport::LoopRange;

pub(crate) mod encode;
pub(crate) mod process;
pub(crate) mod render;

pub use process::ResampleQuality;

/// Entry-point namespace for both export starting points.
#[derive(Debug)]
pub struct Export;

impl Export {
    /// Configure an export from a Tutti audio graph.
    pub fn graph(net: tutti_core::dsp::Net, sample_rate: f64) -> GraphExport {
        GraphExport::new(net, sample_rate)
    }

    /// Configure an export from already-rendered stereo buffers.
    pub fn buffers(left: Vec<f32>, right: Vec<f32>, sample_rate: f64) -> BufferExport {
        BufferExport::new(left, right, sample_rate)
    }
}
