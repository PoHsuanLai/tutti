//! What to render and how to write it — plain data, filled in by struct
//! literal.
//!
//! There is no builder. Every field is public and every struct is `Default`, so
//! a caller writes what it means and lets `..Default::default()` cover the rest:
//!
//! ```
//! # use tutti_core::SampleRate;
//! # use tutti_export::{AudioFormat, EncodeConfig, ExportConfig, Flac, RenderConfig};
//! let config = ExportConfig {
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
//! };
//! # let _ = config;
//! ```
//!
//! A literal is what keeps a setter off a path that ignores it: the fields a
//! stage reads are the fields in the struct it is handed.
//!
//! **This module is data, and only data.** No method here reads a graph, opens a
//! file, or decides anything — the derivations live next to the code that needs
//! them (`render::plan` for frame counts, `encode` for the codec rate). A config
//! constructible without a graph and comparable with `==` is one a caller can
//! build up, log, diff, and hand around; one with an "ask the graph" mode is
//! not.

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
    /// Length of the audible span, in seconds. `f64` deliberately — see
    /// [`duration_to_frames`](crate::duration_to_frames), which explains why
    /// `Seconds` (an f32) cannot carry an hour-long render.
    pub duration_seconds: f64,
    /// Leading FRAMES to drop — look-ahead limiters, linear-phase filters.
    ///
    /// A plain count, not a `{ None, Reported, Exact }` enum: an "ask the graph"
    /// mode would force `RenderPlan::new` to take a graph for arithmetic
    /// that is otherwise pure, putting a decision inside a value. Callers that
    /// want the graph's own figure call
    /// [`RenderGraph::reported_latency`](crate::RenderGraph::reported_latency) and pass the answer — the
    /// same shape as the clock, for the same reason: the caller knows, so the
    /// caller says.
    pub latency: Samples,
    /// Trailing FRAMES to render past the audible span — reverb decay, delay
    /// repeats.
    ///
    /// Unlike [`latency`](Self::latency) these are *kept*: they are audio the
    /// graph produces after the requested span, so the written file is longer
    /// than `duration_seconds` by exactly this much.
    ///
    /// A plain count, for the same reason `latency` is one. The graph's own
    /// figure comes from [`RenderGraph::reported_tail`](crate::RenderGraph::reported_tail), which returns
    /// something a caller must resolve into a number rather than a number
    /// itself — a graph that never decays has no frame count, and neither does
    /// one whose nodes declined to answer. Where to stop is the caller's
    /// decision, and this field is where the caller states it.
    pub tail: Samples,
}

impl Default for RenderConfig {
    fn default() -> Self {
        Self {
            sample_rate: SampleRate(44_100.0),
            duration_seconds: 0.0,
            latency: Samples(0),
            tail: Samples(0),
        }
    }
}

/// The encode stage: what file to write.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EncodeConfig {
    /// The container to write, carrying that format's own settings.
    pub format: AudioFormat,
    /// Depth the samples are quantized to. `Float32` does not quantize, so it is
    /// also the depth [`ExportConfig::dither`] is ignored at.
    pub bit_depth: BitDepth,
    /// Width of the written file. A graph wider than this is folded with the
    /// ITU/Dolby matrix, never truncated; a narrower one is zero-filled.
    pub channels: ChannelLayout,
}

/// The format's and depth's own defaults, written to a **stereo** file.
///
/// Hand-written because [`ChannelLayout`] has no `Default` — a width silently
/// chosen by a derive is how a graph once grew two global inputs nobody
/// declared. Stereo is still the right answer for a bounce left unspecified,
/// but here it is a stated choice: a wider graph folds down to it through the
/// ITU/Dolby matrix, and a caller wanting the graph's own width sets
/// `channels`.
impl Default for EncodeConfig {
    fn default() -> Self {
        Self {
            format: AudioFormat::default(),
            bit_depth: BitDepth::default(),
            channels: ChannelLayout::STEREO,
        }
    }
}

/// Sample-rate conversion applied on the way out.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Resample {
    /// Rate the file is written at. Equal to the render rate means no
    /// conversion — the comparison is made on the integer rate the codec headers
    /// speak, so two rates an ULP apart are the same rate.
    pub target_rate: SampleRate,
    /// Input FRAMES per FFT chunk, and how finely each is subdivided. Sets the
    /// converter's anti-alias steepness and its latency.
    pub chunk: ChunkSize,
}

impl Resample {
    /// Convert to `target_rate` with the default [`ChunkSize`].
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
    /// What to produce, and for how long.
    pub render: RenderConfig,
    /// What file to write it into.
    pub encode: EncodeConfig,
    /// `None` writes at the render rate.
    pub resample: Option<Resample>,
    /// Applied when quantizing to an integer bit depth. Ignored for
    /// [`BitDepth::Float32`], which does not quantize.
    pub dither: Dither,
}
