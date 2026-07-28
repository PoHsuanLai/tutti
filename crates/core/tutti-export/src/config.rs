//! What to render and how to write it — plain data, filled in by struct
//! literal.
//!
//! There is no builder. Every field is public and every struct is `Default`, so
//! a caller writes what it means and lets `..Default::default()` cover the rest:
//!
//! ```ignore
//! ExportConfig {
//!     render: RenderConfig {
//!         sample_rate: SampleRate(48_000.0),
//!         duration_seconds: 30.0,
//!         ..Default::default()
//!     },
//!     encode: EncodeConfig {
//!         format: AudioFormat::Flac(Flac::default()),
//!         ..Default::default()
//!     },
//!     ..Default::default()
//! }
//! ```
//!
//! The fluent builder this replaced put every setter on every path, including
//! four that one path silently ignored. A literal cannot do that: the fields a
//! stage reads are the fields in the struct it is handed.
//!
//! **This module is data, and only data.** No method here reads a graph, opens
//! a file, or decides anything — the derivations that used to hang off these
//! structs live next to the code that needs them (`render::plan` for frame
//! counts, `encode` for the codec rate). A config you can construct without a
//! `Net` and compare with `==` is one a caller can build up, log, diff, and hand
//! around; one with an "ask the graph" mode is not.

use crate::options::{AudioFormat, BitDepth, Dither};
use crate::process::ChunkSize;
use tutti_core::SampleRate;
use tutti_types::{ChannelLayout, Samples};

/// The render stage: what to produce, and for how long.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RenderConfig {
    /// Rate the graph is rendered at. The output rate may differ — see
    /// [`ExportConfig::resample`].
    pub sample_rate: SampleRate,
    /// Length in seconds. `f64` deliberately — see [`duration_to_frames`].
    pub duration_seconds: f64,
    /// Leading frames to drop — look-ahead limiters, linear-phase filters.
    ///
    /// A plain count, not a `Trim { None, Reported, Exact }` enum. `Reported`
    /// meant "ask the graph", which forced `RenderPlan::new` to take a
    /// `&mut Net` for arithmetic that is otherwise pure, and put a decision
    /// inside a value. Callers that want the graph's own figure call
    /// [`reported_latency`](crate::reported_latency) and pass the answer — the
    /// same shape as the clock, for the same reason: the caller knows, so the
    /// caller says.
    pub latency: Samples,
}

impl Default for RenderConfig {
    fn default() -> Self {
        Self {
            sample_rate: SampleRate(44_100.0),
            duration_seconds: 0.0,
            latency: Samples(0),
        }
    }
}

/// The encode stage: what file to write.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct EncodeConfig {
    pub format: AudioFormat,
    pub bit_depth: BitDepth,
    /// Width of the written file. A graph wider than this is folded with the
    /// ITU/Dolby matrix, never truncated; a narrower one is zero-filled.
    pub channels: ChannelLayout,
}

/// Sample-rate conversion applied on the way out.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Resample {
    pub target_rate: SampleRate,
    pub chunk: ChunkSize,
}

impl Resample {
    pub fn to(target_rate: impl Into<SampleRate>) -> Self {
        Self {
            target_rate: target_rate.into(),
            chunk: ChunkSize::default(),
        }
    }
}

/// One export.
///
/// No `normalize` field. Normalization needs the whole signal measured before a
/// gain can be chosen, which is two passes — and a *caller's* composition, not a
/// stage this crate hides. Measure with
/// [`tutti_analysis::measure_loudness`](https://docs.rs), take
/// `Loudness::gain_to`, apply it. That is why this crate can stream.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ExportConfig {
    pub render: RenderConfig,
    pub encode: EncodeConfig,
    /// `None` writes at the render rate.
    pub resample: Option<Resample>,
    /// Applied when quantizing to an integer bit depth. Ignored for
    /// [`BitDepth::Float32`], which does not quantize.
    pub dither: Dither,
}
