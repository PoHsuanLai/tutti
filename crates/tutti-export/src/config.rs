//! What to render and how to write it — plain data, filled in by struct
//! literal.
//!
//! There is no builder. Every field is public and every struct is `Default`, so
//! a caller writes what it means and lets `..Default::default()` cover the rest:
//!
//! ```
//! # use tutti_types::SampleRate;
//! # use tutti_export::{EncodeConfig, ExportConfig, FlacLevel, Format, IntDepth, RenderConfig};
//! let config = ExportConfig {
//!     render: RenderConfig {
//!         sample_rate: SampleRate(48_000.0),
//!         duration_seconds: 30.0,
//!         ..Default::default()
//!     },
//!     encode: EncodeConfig {
//!         format: Format::Flac { depth: IntDepth::Int24, level: FlacLevel::Standard },
//!         ..Default::default()
//!     },
//!     ..Default::default()
//! };
//! # let _ = config;
//! ```
//!
//! A literal is not a guarantee that every field is read: one config serves
//! four entry points, and each ignores some of it. What each reads is on
//! [`ExportConfig`].
//!
//! This module is data, and only data. No method here reads a graph, opens a
//! file, or decides anything — the derivations live next to the code that needs
//! them (`render::plan` for frame counts, `encode` for the codec rate). A config
//! constructible without a graph and comparable with `==` is one a caller can
//! build up, log, diff, and hand around; one with an "ask the graph" mode is
//! not.

use crate::options::Dither;
use crate::process::ChunkSize;
use tutti_codec::Format;
use tutti_types::SampleRate;
use tutti_types::{ChannelLayout, Samples};

/// The render stage: what to produce, and for how long.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RenderConfig {
    /// Rate the graph is rendered at. The output rate may differ — see
    /// [`ExportConfig::resample`].
    pub sample_rate: SampleRate,
    /// Length of the audible span, in seconds; a render of no frames (this
    /// default's `0.0`, with no [`tail`](Self::tail)) is
    /// [`ConfigError::EmptyRender`](crate::ConfigError::EmptyRender), and a
    /// NaN, infinite or negative value
    /// [`ConfigError::InvalidDuration`](crate::ConfigError::InvalidDuration).
    /// `f64` deliberately — see
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
    /// A node's tail excludes its latency, so a render that trims less than
    /// the graph's latency must count the untrimmed rest here too, or the
    /// programme's last frames are cut: the frames to keep past the span are
    /// the untrimmed latency plus the graph's tail. The session's
    /// `with_reported_tail` does that sum; a caller setting this by hand does
    /// it itself.
    ///
    /// A plain count, for the same reason `latency` is one. The graph's own
    /// figure comes from [`RenderGraph::reported_tail`](crate::RenderGraph::reported_tail), which returns
    /// something a caller must resolve into a number rather than a number
    /// itself — a graph that never decays has no frame count, and neither does
    /// one whose nodes declined to answer. Where to stop is the caller's
    /// decision, and this field is where the caller states it.
    pub tail: Samples,
    /// What a render does when the graph dropped an event: a port past its
    /// capacity, a full delay. The default fails the render
    /// ([`Error::EventsDropped`](crate::Error::EventsDropped)), because the
    /// audio of a dropped note-on or note-off is wrong and nothing in it says so.
    pub on_event_loss: EventLossPolicy,
}

/// What a render does when the graph it renders dropped events
/// ([`RenderConfig::on_event_loss`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum EventLossPolicy {
    /// The render stops at the first block that dropped an event with
    /// [`Error::EventsDropped`](crate::Error::EventsDropped), and a file it
    /// wrote is removed. [`write_buffers`](crate::write_buffers) applies it to
    /// the planes' own count.
    #[default]
    Fail,
    /// The render succeeds and says how many events were dropped
    /// ([`Rendered::dropped_events`](crate::Rendered::dropped_events),
    /// [`Written::dropped_events`](crate::Written::dropped_events)): for a caller that
    /// checks the figure itself.
    Allow,
}

impl Default for RenderConfig {
    fn default() -> Self {
        Self {
            sample_rate: SampleRate(44_100.0),
            duration_seconds: 0.0,
            latency: Samples(0),
            tail: Samples(0),
            on_event_loss: EventLossPolicy::default(),
        }
    }
}

/// The encode stage: what file to write.
#[derive(Debug, Clone, PartialEq)]
pub struct EncodeConfig {
    /// The container to write, carrying that format's own settings, the sample
    /// width among them. A width that does not quantize (`Pcm::Float32`, and
    /// Ogg Vorbis) is also where [`ExportConfig::dither`] is ignored.
    pub format: Format,
    /// Width of the written file. A graph (or, for
    /// [`write_buffers`](crate::write_buffers), a set of planes) of another
    /// width is folded onto it by speaker position: a wider one with the
    /// ITU/Dolby matrix, a narrower one by the engine's order, with the
    /// channels the source lacks left silent. A width the fold cannot place
    /// is [`ConfigError::Placement`](crate::ConfigError::Placement), never a
    /// dropped channel.
    pub channels: ChannelLayout,
    /// Which speaker each file channel feeds, for the WAV speaker mask and Ogg
    /// Vorbis's channel order: `None`
    /// is the engine's SMPTE order for `channels` (mono, stereo and 5.1 named;
    /// a bare 4 or 8 unassigned), and a caller that knows it is writing quad
    /// passes [`ChannelTopology::quad`](tutti_types::ChannelTopology::quad).
    /// Its width must be `channels`. WAV writes the samples as given. Ogg
    /// Vorbis permutes 3 to 8 channels into the order its specification fixes
    /// for the width, and refuses a 3, 4, 5 or 7 channel export with no
    /// topology (the engine has no default order for those widths) or one it
    /// cannot place. The other formats ignore it.
    pub topology: Option<tutti_types::ChannelTopology>,
}

/// A 24-bit WAV, written to a stereo file. There is a default only with the
/// `wav` feature: a build without it names its format.
///
/// Hand-written because [`ChannelLayout`] has no `Default`, so the width is a
/// stated choice rather than one a derive picks. Stereo is the answer for a
/// bounce left unspecified: a wider graph folds down to it through the
/// ITU/Dolby matrix, and a caller wanting the graph's own width sets
/// `channels`.
#[cfg(feature = "wav")]
impl Default for EncodeConfig {
    fn default() -> Self {
        Self {
            format: Format::default(),
            channels: ChannelLayout::STEREO,
            topology: None,
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

/// Everything one export needs: what to render, how to encode it, and the
/// optional resample and dither on the way out.
///
/// Plain data, built by struct literal with `..Default::default()` for the rest
/// (see the [crate-level example](crate)). The default renders at 44.1 kHz to
/// a stereo WAV with triangular dither and no resample, and has **no
/// duration**: a render of no frames is
/// [`ConfigError::EmptyRender`](crate::ConfigError::EmptyRender), so set at
/// least [`RenderConfig::duration_seconds`] (a NaN, infinite or negative one
/// is [`ConfigError::InvalidDuration`](crate::ConfigError::InvalidDuration)).
///
/// There is no `normalize` field: normalization needs the whole signal
/// measured before a gain can be chosen, which is two passes. Use
/// [`render_normalized_to_file`](crate::render_normalized_to_file), or measure
/// with [`tutti_measure::measure_loudness`], take `Loudness::gain_to` and
/// apply it with [`Rendered::apply_gain`](crate::Rendered::apply_gain).
///
/// # What each entry point reads
///
/// One config serves every entry point, and a field an entry point has no use
/// for is ignored, not refused:
///
/// - [`render_to_file`](crate::render_to_file): all of it.
/// - [`render_normalized_to_file`](crate::render_normalized_to_file): all of
///   it; `resample` is applied before the measurement, and for an integer
///   format the peak target and ceiling are pulled down to leave the format's
///   headroom.
/// - [`render_to_buffers`](crate::render_to_buffers): `render`, and
///   `encode.channels` and `encode.topology` for the fold. **Ignores**
///   `encode.format`, `resample` and `dither`: the planes are not an output.
/// - [`write_buffers`](crate::write_buffers): `encode`, `resample` and
///   `dither`, and `render.on_event_loss` for the planes' own loss count.
///   **Ignores** the rest of `render` (`sample_rate`, `duration_seconds`,
///   `latency`, `tail`): the frames exist and carry their own rate.
#[derive(Debug, Clone, PartialEq)]
pub struct ExportConfig {
    /// What to produce, and for how long.
    pub render: RenderConfig,
    /// What file to write it into.
    pub encode: EncodeConfig,
    /// `None` writes at the render rate.
    pub resample: Option<Resample>,
    /// Applied when quantizing to an integer width. Ignored for a format that
    /// does not quantize (`Pcm::Float32`, and Ogg Vorbis).
    pub dither: Dither,
}

/// A 24-bit stereo WAV with no duration, which a render refuses until one is
/// set. Only with the `wav` feature, like [`EncodeConfig`]'s.
#[cfg(feature = "wav")]
impl Default for ExportConfig {
    fn default() -> Self {
        Self {
            render: RenderConfig::default(),
            encode: EncodeConfig::default(),
            resample: None,
            dither: Dither::default(),
        }
    }
}
