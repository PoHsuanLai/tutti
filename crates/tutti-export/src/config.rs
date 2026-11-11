//! What to render and how to write it — plain data, filled in by struct
//! literal.
//!
//! This is the bundle a caller that has one render and one file fills in
//! (and what `tutti-session`'s export request still carries); it is not what
//! the pipeline takes. [`render`](crate::render()) takes a [`Job`] and a list of
//! [`Output`](crate::Output)s, and [`ExportConfig::job`] and
//! [`ExportConfig::file_target`] are the bridge to them. It goes with the
//! session's request in doc 019 step 5.
//!
//! There is no builder. Every field is public and every struct is `Default`, so
//! a caller writes what it means and lets `..Default::default()` cover the rest:
//!
//! ```
//! # use tutti_types::{SampleRate, Seconds};
//! # use tutti_export::{EncodeConfig, ExportConfig, FlacLevel, Format, IntDepth, RenderConfig};
//! let config = ExportConfig {
//!     render: RenderConfig {
//!         sample_rate: SampleRate(48_000.0),
//!         duration_seconds: Seconds(30.0),
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
//! several uses, and each ignores some of it. What each reads is on
//! [`ExportConfig`].
//!
//! This module is data, and only data. No method here reads a graph, opens a
//! file, or decides anything — the derivations live next to the code that needs
//! them (`render::plan` for frame counts, `encode` for the codec rate). A config
//! constructible without a graph and comparable with `==` is one a caller can
//! build up, log, diff, and hand around; one with an "ask the graph" mode is
//! not.

use crate::error::ConfigError;
use crate::job::{Job, JobError};
use crate::normalize::Normalize;
use crate::output::{BufferTarget, FileTarget, NormalizeSpec};
use crate::process::ChunkSize;
use std::path::PathBuf;
use tutti_codec::Format;
use tutti_dsp::DitherShape;
use tutti_types::{ChannelLayout, Samples};
use tutti_types::{SampleRate, Seconds};

/// The render stage: what to produce, and for how long.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RenderConfig {
    /// Rate the graph is rendered at. The output rate may differ — see
    /// [`ExportConfig::resample`].
    pub sample_rate: SampleRate,
    /// Length of the audible span, in seconds; a render of no frames (this
    /// default's `0.0`, with no [`tail`](Self::tail)) is
    /// [`ConfigError::EmptyRender`], and a
    /// NaN, infinite or negative value
    /// [`ConfigError::InvalidDuration`].
    /// The span is rounded to the nearest frame.
    pub duration_seconds: Seconds,
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
    /// The container to write, carrying that format's own settings, the sample
    /// width among them. A width that does not quantize (`Pcm::Float32`, and
    /// Ogg Vorbis) is also where [`ExportConfig::dither`] is ignored.
    pub format: Format,
    /// Width of the written file. A graph (or, for
    /// [`write_buffers`](crate::write_buffers), a set of planes) of another
    /// width is folded onto it by speaker position: a wider one with the
    /// ITU/Dolby matrix, a narrower one by the engine's order, with the
    /// channels the source lacks left silent. A width the fold cannot place
    /// is [`ConfigError::Placement`], never a
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
/// [`ConfigError::EmptyRender`], so set at
/// least [`RenderConfig::duration_seconds`] (a NaN, infinite or negative one
/// is [`ConfigError::InvalidDuration`]).
///
/// There is no `normalize` field: a file target asks for it
/// ([`FileTarget::with_normalize`]), and [`file_target`](Self::file_target)
/// takes it as an argument.
///
/// # What each use reads
///
/// One config serves every use, and a field a use has no need of is ignored,
/// not refused:
///
/// - [`job`](Self::job): `render`.
/// - [`file_target`](Self::file_target): all of `encode`, `resample` and
///   `dither`.
/// - [`buffer_target`](Self::buffer_target): `encode.channels` and
///   `encode.topology` for the fold. **Ignores** `encode.format`, `resample`
///   and `dither`: the planes are not an output.
/// - [`write_buffers`](crate::write_buffers): `encode`, `resample` and
///   `dither`, and `render.on_event_loss` for the planes' own loss count.
///   **Ignores** the rest of `render`: the frames exist and carry their own
///   rate.
#[derive(Debug, Clone, PartialEq)]
pub struct ExportConfig {
    /// What to produce, and for how long.
    pub render: RenderConfig,
    /// What file to write it into.
    pub encode: EncodeConfig,
    /// `None` writes at the render rate.
    pub resample: Option<Resample>,
    /// Applied when quantizing to an integer width; `None` adds none. Ignored
    /// for a format that does not quantize (`Pcm::Float32`, and Ogg Vorbis).
    pub dither: Option<DitherShape>,
}

impl RenderConfig {
    /// The [`Job`] this config describes.
    ///
    /// A tail-only render (no audible span, a tail) is a render, and `Job`
    /// refuses a zero length, so its length is then the tail: the same frames
    /// and the same trimmed head.
    ///
    /// # Errors
    ///
    /// [`ConfigError::InvalidDuration`] for a duration that is NaN, infinite
    /// or negative; [`ConfigError::EmptyRender`] for a render that keeps no
    /// frame; otherwise what [`Job::new`] refuses ([`ConfigError::Job`]).
    pub fn job(&self) -> Result<Job, ConfigError> {
        if !self.duration_seconds.get().is_finite() || self.duration_seconds.get() < 0.0 {
            return Err(ConfigError::InvalidDuration);
        }
        let duration = crate::duration_to_frames(self.duration_seconds, self.sample_rate);
        let (length, tail) = if duration == Samples(0) {
            (self.tail, Samples(0))
        } else {
            (duration, self.tail)
        };
        Job::new(self.sample_rate, length, self.latency, tail)
            .map(|job| job.with_event_loss(self.on_event_loss))
            .map_err(|e| match e {
                JobError::ZeroLength => ConfigError::EmptyRender,
                other => ConfigError::Job(other),
            })
    }
}

impl ExportConfig {
    /// The [`Job`] of `render` ([`RenderConfig::job`]).
    ///
    /// # Errors
    ///
    /// As [`RenderConfig::job`].
    pub fn job(&self) -> Result<Job, ConfigError> {
        self.render.job()
    }

    /// The job of a render held in memory: as [`job`](Self::job), except that
    /// a span past [`MAX_FRAMES`](crate::MAX_FRAMES) is
    /// [`Error::RenderTooLarge`](crate::Error::RenderTooLarge), the error of
    /// planes that cannot be held.
    ///
    /// # Errors
    ///
    /// As [`job`](Self::job), and the one above.
    pub fn buffers_job(&self) -> crate::Result<Job> {
        match self.render.job() {
            Err(ConfigError::Job(JobError::TooLong { total: Some(total) })) => {
                Err(crate::Error::RenderTooLarge {
                    frames: total,
                    channels: self.encode.channels,
                })
            }
            job => Ok(job?),
        }
    }

    /// The file at `path` this config describes, normalized by `normalize`
    /// (measured on this output) when given.
    #[must_use]
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

    /// The planes this config describes: `encode.channels` wide.
    #[must_use]
    pub fn buffer_target(&self) -> BufferTarget {
        BufferTarget::new(self.encode.channels).with_topology(self.encode.topology.clone())
    }
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
            dither: Some(DitherShape::Triangular),
        }
    }
}
