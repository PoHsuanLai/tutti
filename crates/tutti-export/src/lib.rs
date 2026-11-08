#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod error;
pub use error::{ConfigError, Error, Result};

mod options;
pub use options::Dither;
/// The container and its settings, and the sample widths, which are
/// `tutti-codec`'s. Re-exported so a caller configuring an export names them
/// from here.
pub use tutti_codec::{FlacLevel, Format, IntDepth, Pcm, Quality};
pub use tutti_types::ChannelLayout;

mod control;
use control::{Control, never_stop};
pub use control::{Flow, Outcome, Pass, Progress};

mod config;
pub use config::{EncodeConfig, EventLossPolicy, ExportConfig, RenderConfig, Resample};

mod normalize;
pub use normalize::{Normalize, render_normalized_to_file, render_normalized_to_file_with};

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

use std::borrow::BorrowMut;
use std::path::{Path, PathBuf};
use tutti_types::Samples;

/// A file an export wrote, returned by [`render_to_file`], [`write_buffers`]
/// and [`render_normalized_to_file`].
#[derive(Debug, Clone, PartialEq, Eq)]
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
}

/// Rendered audio held in memory, one `Vec` per channel.
///
/// Returned by [`render_to_buffers`]; written with [`write_buffers`]. Planes
/// rather than a `(left, right)` pair, so a surround render keeps every
/// channel.
#[derive(Clone, PartialEq)]
pub struct Rendered {
    /// One `Vec` per channel, all the same length. Planar, not interleaved —
    /// [`interleaved`](Self::interleaved) is the conversion.
    pub planes: Vec<Vec<f32>>,
    /// The rate these samples are at, which is not necessarily the rate a
    /// config asked to write: [`render_to_buffers`] does not resample, so this
    /// is always the render rate. An encoder must read it from here rather than
    /// from a config, or it converts from a rate the samples were never at.
    pub sample_rate: tutti_types::SampleRate,
    /// Events the graph lost during the render (not a note-off it refused and
    /// sent again); nonzero only under [`EventLossPolicy::Allow`] (under `Fail`
    /// the render is an error instead). A lost note-on or note-off makes the
    /// audio wrong.
    pub dropped_events: u64,
}

impl std::fmt::Debug for Rendered {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rendered")
            .field("channels", &self.channels())
            .field("frames", &self.frames())
            .field("sample_rate", &self.sample_rate)
            .field("dropped_events", &self.dropped_events)
            .finish_non_exhaustive()
    }
}

impl Rendered {
    /// Returns the frames per plane — the length of one channel, not the total
    /// sample count.
    pub fn frames(&self) -> Samples {
        Samples(self.planes.first().map_or(0, |p| p.len()))
    }

    /// Returns the channel count — the interleave stride, and the number of
    /// planes.
    pub fn channels(&self) -> usize {
        self.planes.len()
    }

    /// Returns the width these planes carry as a [`ChannelLayout`].
    ///
    /// [`channels`](Self::channels) is the same number as a raw stride.
    pub fn layout(&self) -> ChannelLayout {
        ChannelLayout::from(u16::try_from(self.planes.len()).unwrap_or(u16::MAX))
    }

    /// Multiplies every sample by `gain` — the apply half of a
    /// measure-then-apply normalization.
    pub fn apply_gain(&mut self, gain: tutti_types::Db) {
        let scale = gain.to_amplitude().get();
        for plane in self.planes.iter_mut() {
            for s in plane.iter_mut() {
                *s *= scale;
            }
        }
    }

    /// Returns the planes interleaved — the shape a meter or an encoder takes.
    ///
    /// Allocates the whole render. A `Rendered` holds an entire offline
    /// pass in memory, so this doubles that for the duration of the call; on a
    /// long export it is the largest single allocation in the crate. Prefer
    /// [`interleaved_into`](Self::interleaved_into) wherever the interleaved
    /// copy is read and dropped rather than kept.
    pub fn interleaved(&self) -> Vec<f32> {
        let mut out = Vec::new();
        self.interleaved_into(&mut out);
        out
    }

    /// Interleaves into a caller-owned buffer, reusing its allocation.
    ///
    /// `out` is cleared first, so it is a destination and not an accumulator.
    /// Prefer it over [`interleaved`](Self::interleaved) when interleaving more
    /// than once.
    pub fn interleaved_into(&self, out: &mut Vec<f32>) {
        out.clear();
        if self.planes.is_empty() {
            return;
        }
        out.resize(self.frames().interleaved_len(self.layout()), 0.0);
        tutti_types::InterleavedMut::new(out, self.layout()).interleave_from(&self.planes, 0);
    }
}

/// The frame width an export config asks for, as the interleave stride.
///
/// The one place a [`ChannelLayout`] becomes the `usize` stride the render and
/// the encoders use, and zero is the only rejected width. That the check is
/// a bound rather than a list of enumerated widths is what lets a 3- or 5-wide
/// master export at all: nothing here is generic over the count.
/// The file's channels, for the fold from the graph's.
fn file_channels(config: &ExportConfig) -> render::FileChannels<'_> {
    render::FileChannels {
        layout: config.encode.channels,
        topology: config.encode.topology.as_ref(),
    }
}

fn frame_width(layout: ChannelLayout) -> Result<usize> {
    match layout.count() {
        0 => Err(Error::UnsupportedChannels(0)),
        n => Ok(n as usize),
    }
}

/// Writes already-rendered audio to `path`.
///
/// With [`render_to_buffers`] and [`Rendered::apply_gain`] this makes
/// measure-then-apply possible: render to buffers, measure, apply a gain,
/// write.
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
    let written = encode::encode_planes(
        rendered,
        config,
        path.as_ref(),
        &mut Control::new(Pass::ONLY, &mut never_stop),
    )?;
    Ok(finished(written))
}

/// The value of an outcome no tick could have cancelled.
fn finished<T>(outcome: Outcome<T>) -> T {
    match outcome {
        Outcome::Finished(t) => t,
        Outcome::Cancelled => unreachable!("`never_stop` never cancels"),
    }
}

/// Renders `graph` and writes it to `path`.
///
/// Streams: the encoder pulls the graph one block at a time and no PCM is held
/// whole. The frame count comes from `config.render` (duration, latency trim,
/// tail); the output is resampled, dithered and encoded as `config` says.
/// `clock` is advanced once per block, after the graph processes
/// ([`RenderClock::render_graph`]) — pass [`FrozenClock`] for a graph with no
/// time-dependent nodes.
///
/// `graph` is built for the render or forked from a live one
/// ([`RenderGraph`]), and must be prepared at `config.render.sample_rate`.
/// Pass it by value, or as `&mut RenderGraph` to keep it: a graph holding a
/// unit whose teardown is main-thread-only (an in-process VST2 plugin's fork)
/// and rendered on a worker is brought back and dropped on the main thread.
/// Blocks until the file is finalized.
///
/// # Errors
///
/// As [`write_buffers`], plus [`Error::InvalidConfig`] if the graph was
/// prepared at another rate or its editor does not feed its executor, or the
/// render has no length ([`ConfigError::EmptyRender`],
/// [`ConfigError::InvalidDuration`]), and
/// [`Error::ForkFailed`] if a forked node (a hosted plugin) failed during the
/// render — checked before the file is put in place, so `path` keeps what it
/// held — and
/// [`Error::EventsDropped`] if the graph lost an event and
/// `config.render.on_event_loss` is [`EventLossPolicy::Fail`] (the default):
/// checked before the file is put in place, so `path` keeps what it held.
pub fn render_to_file(
    graph: impl BorrowMut<RenderGraph>,
    config: &ExportConfig,
    clock: &dyn RenderClock,
    path: impl AsRef<Path>,
) -> Result<Written> {
    render_to_file_with(graph, config, clock, path, &mut never_stop).map(finished)
}

/// [`render_to_file`], reporting to `tick` after each block.
///
/// `tick` sees the [`Progress`] of the pass (one frame count rising to its
/// total) and answers [`Flow::Stop`] to end the render, which returns
/// [`Outcome::Cancelled`]. The render is written to a temporary file beside
/// `path` and renamed over it when it finishes, so a cancelled or failed
/// render leaves what was at `path` as it was.
///
/// # Errors
///
/// As [`render_to_file`]. A cancelled render is not an error.
pub fn render_to_file_with(
    mut graph: impl BorrowMut<RenderGraph>,
    config: &ExportConfig,
    clock: &dyn RenderClock,
    path: impl AsRef<Path>,
    tick: &mut dyn FnMut(Progress) -> Flow,
) -> Result<Outcome<Written>> {
    frame_width(config.encode.channels)?;
    let path = path.as_ref();
    let plan = render::RenderPlan::new(&config.render)?;
    let mut control = Control::new(Pass::ONLY, tick);
    // The loss check is inside `encode_to_file`, before the rename.
    render::with_source(
        graph.borrow_mut(),
        config.render.sample_rate,
        clock,
        &file_channels(config),
        |src| {
            encode::encode_to_file(
                src,
                config.render.sample_rate,
                &plan,
                config,
                path,
                &mut control,
            )
        },
    )
    .map(|(written, _lost)| written)
}

/// Renders `graph` ([`RenderGraph`]) into memory.
///
/// Renders the same frames as [`render_to_file`] and reports the rate it
/// actually rendered at. `graph` by value or as `&mut RenderGraph`, as there. Only `config.render` and `config.encode.channels` are
/// read.
///
/// Neither resampled nor dithered, for the same reason in both cases: they
/// are *output* steps, and these planes are not an output. `config.resample`
/// belongs at the file boundary (a caller holding planes can resample them
/// itself, and reporting a rate the samples are not at is the bug this shape
/// avoids). Dither is scaled to one LSB of the encoded bit depth — which `f32`
/// planes do not have, so applying it here adds noise to a signal that
/// quantizes to nothing.
///
/// Dithering here would also be *measured* by anything that reads these planes.
/// A silent render at an integer depth would come back reading roughly −86 dBTP
/// of dither rather than silence, and normalizing that reading amplifies the
/// noise toward full scale.
///
/// [`write_buffers`] dithers on the way out, at the real depth and after any
/// resample — the only point where the LSB is known.
///
/// # Errors
///
/// [`Error::UnsupportedChannels`] for a zero-channel `config.encode.channels`;
/// [`Error::InvalidConfig`] if the graph was prepared at another rate or its
/// editor does not feed its executor; [`Error::ForkFailed`] if a forked node
/// failed during the render; [`Error::EventsDropped`] if the graph lost an
/// event and `config.render.on_event_loss` is [`EventLossPolicy::Fail`] (the
/// default).
pub fn render_to_buffers(
    graph: impl BorrowMut<RenderGraph>,
    config: &ExportConfig,
    clock: &dyn RenderClock,
) -> Result<Rendered> {
    render_to_buffers_with(graph, config, clock, &mut never_stop).map(finished)
}

/// [`render_to_buffers`], reporting to `tick` after each block; see
/// [`render_to_file_with`]. A cancelled render returns [`Outcome::Cancelled`]
/// and the planes rendered so far are dropped.
///
/// # Errors
///
/// As [`render_to_buffers`].
pub fn render_to_buffers_with(
    graph: impl BorrowMut<RenderGraph>,
    config: &ExportConfig,
    clock: &dyn RenderClock,
    tick: &mut dyn FnMut(Progress) -> Flow,
) -> Result<Outcome<Rendered>> {
    render_pass(graph, config, clock, &mut Control::new(Pass::ONLY, tick))
}

/// The error for planes that cannot be held.
fn too_large(plan: &render::RenderPlan, config: &ExportConfig) -> Error {
    Error::RenderTooLarge {
        frames: plan.output_length,
        channels: config.encode.channels,
    }
}

/// One render into memory, reporting through `control`.
fn render_pass(
    mut graph: impl BorrowMut<RenderGraph>,
    config: &ExportConfig,
    clock: &dyn RenderClock,
    control: &mut Control<'_>,
) -> Result<Outcome<Rendered>> {
    let ch = frame_width(config.encode.channels)?;
    let plan = render::RenderPlan::new(&config.render)?;

    // `vec![Vec::with_capacity(n); ch]` would clone ONE empty Vec `ch` times,
    // and a clone does not carry capacity — every plane would reallocate.
    //
    // Reserved fallibly: the duration is the caller's, and an hour of 7.1.4
    // (or a typo) is a plain `Err` here where `with_capacity` would abort the
    // process.
    let mut planes: Vec<Vec<f32>> = Vec::new();
    planes
        .try_reserve_exact(ch)
        .map_err(|_| too_large(&plan, config))?;
    for _ in 0..ch {
        let mut plane = Vec::new();
        plane
            .try_reserve_exact(plan.output_length.get())
            .map_err(|_| too_large(&plan, config))?;
        planes.push(plane);
    }
    let (driven, lost) = render::with_source(
        graph.borrow_mut(),
        config.render.sample_rate,
        clock,
        &file_channels(config),
        |src| {
            let driven = render::drive(
                src,
                config.encode.channels,
                &plan,
                control,
                config.render.on_event_loss,
                |block| {
                    block.append_to_planes(&mut planes);
                    Ok(())
                },
            )?;
            // Before the planes are handed back: a unit that failed rendered
            // silence into them.
            if driven == Outcome::Finished(()) {
                src.check_health()?;
            }
            Ok(driven)
        },
    )?;
    // A cancelled render is no render: it is not judged.
    if driven == Outcome::Finished(()) {
        render::check_loss(config.render.on_event_loss, lost)?;
    }

    Ok(driven.map(|()| Rendered {
        planes,
        sample_rate: config.render.sample_rate,
        dropped_events: lost,
    }))
}
