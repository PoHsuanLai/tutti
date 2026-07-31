//! # tutti-export
//!
//! Offline audio export: render a Tutti graph to a file, or to buffers.
//!
//! ```ignore
//! use tutti_export::{render_to_file, ExportConfig, RenderConfig, EncodeConfig};
//! use tutti_core::{SampleRate, FrozenClock};
//!
//! render_to_file(
//!     net,
//!     &ExportConfig {
//!         render: RenderConfig {
//!             sample_rate: SampleRate(48_000.0),
//!             duration_seconds: 30.0,
//!             ..Default::default()
//!         },
//!         encode: EncodeConfig {
//!             format: AudioFormat::Flac(Flac::default()),
//!             ..Default::default()
//!         },
//!         ..Default::default()
//!     },
//!     &FrozenClock,          // or your OfflineTimeline
//!     "master.flac".as_ref(),
//! )?;
//! ```
//!
//! ## What this crate does not do
//!
//! **It does not spawn threads.** Both entry points are synchronous and `Send`.
//! A host that wants a render off the main thread already owns a task pool that
//! is better at it than a raw `std::thread` would be — this crate's own ECS
//! surface routes its renders through Bevy's `AsyncComputeTaskPool` for exactly
//! that reason.
//!
//! **It does not normalize *while streaming*.** Choosing a gain means measuring
//! the whole signal first, which is two passes. So normalization is not a field
//! on [`ExportConfig`] that quietly changes what `render_to_file` costs — it is
//! [`render_normalized_to_file`], a separate entry point whose name says which
//! path you are on. Hiding the two passes inside one export is what forced the
//! whole signal into memory before.
//!
//! For a gain of your own — logged, gated, or derived some other way — compose
//! the steps directly: measure with `tutti_analysis::loudness` (a streaming
//! meter, so it can run *while* rendering), take `Loudness::gain_to`, apply it
//! with [`Rendered::apply_gain`], and write with [`write_buffers`].
//!
//! **It does not decide how to buffer.** Every format streams, because every
//! codec library it uses supports incremental encoding. There is no
//! buffered-versus-streaming mode to pick.

mod error;
pub use error::{Error, Result};

mod options;
pub use options::{AudioFormat, BitDepth, Dither, Flac, Ogg};
pub use tutti_types::ChannelLayout;

mod config;
pub use config::{EncodeConfig, ExportConfig, RenderConfig, Resample};

mod normalize;
pub use normalize::{render_normalized_to_file, Normalize};
/// Frame-count arithmetic — pure, and public so a caller can size a render
/// before committing to one.
pub use render::plan::{beats_to_seconds, duration_to_frames};

pub(crate) mod encode;
pub(crate) mod process;
pub(crate) mod render;

pub use process::ChunkSize;
/// The clock a render advances. Re-exported so callers can name it without
/// depending on `tutti-core` directly; `FrozenClock` is the "this graph has no
/// transport" answer.
pub use tutti_core::transport::{FrozenClock, RenderClock};

use std::path::{Path, PathBuf};
use tutti_types::Samples;

/// A file that was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
    pub path: PathBuf,
    pub bytes: u64,
}

/// Rendered audio, one `Vec` per channel.
///
/// Planes rather than a `(left, right)` pair: the pair could not express a
/// surround render, so the in-memory path used to silently fold anything wider
/// than stereo — and its callers hardcoded "2 channels" downstream because the
/// type gave them no other answer.
#[derive(Debug, Clone, PartialEq)]
pub struct Rendered {
    pub planes: Vec<Vec<f32>>,
    pub sample_rate: tutti_core::SampleRate,
}

impl Rendered {
    /// Frames per plane.
    pub fn frames(&self) -> Samples {
        Samples(self.planes.first().map_or(0, |p| p.len()))
    }

    /// Channel count — the interleave stride, and the number of planes.
    pub fn channels(&self) -> usize {
        self.planes.len()
    }

    /// The width these planes carry, as the engine's channel vocabulary.
    ///
    /// [`channels`](Self::channels) is the same number as a raw stride, for the
    /// indexing arithmetic that wants one. This is the *declaration* — what
    /// callers reaching for a layout (the loudness meter, a resample, an encode
    /// config) actually want, so they stop re-wrapping the count themselves.
    pub fn layout(&self) -> ChannelLayout {
        ChannelLayout::from_count(self.planes.len() as u16)
    }

    /// Multiply every sample by `gain` — the apply half of a measure-then-apply
    /// normalization.
    pub fn apply_gain(&mut self, gain: tutti_types::Db) {
        let scale = gain.to_amplitude().get();
        for plane in self.planes.iter_mut() {
            for s in plane.iter_mut() {
                *s *= scale;
            }
        }
    }

    /// The planes interleaved — the shape a meter or an encoder takes.
    pub fn interleaved(&self) -> Vec<f32> {
        let frames = self.frames().get();
        let mut out = Vec::with_capacity(frames * self.channels());
        for i in 0..frames {
            for plane in &self.planes {
                out.push(plane[i]);
            }
        }
        out
    }
}

/// The frame width an export config asks for.
///
/// The one place a [`ChannelLayout`] becomes the `usize` stride the render and
/// the encoders use. Zero is the only rejected width — this used to be a
/// `dispatch_channels!` macro that monomorphized the whole pipeline at one of
/// 1/2/4/6/8/12 and returned [`Error::UnsupportedChannels`] for everything else,
/// which meant a 3- or 5-wide master (`ChannelLayout::from_count(n)` for any
/// unenumerated `n`) could not be exported at all.
fn frame_width(layout: ChannelLayout) -> Result<usize> {
    match layout.count() {
        0 => Err(Error::UnsupportedChannels(0)),
        n => Ok(n as usize),
    }
}

/// The look-ahead latency `net` reports, as a frame count.
///
/// For a caller that wants `RenderConfig::latency` to be whatever the graph says —
/// look-ahead limiters, linear-phase filters. It is a function rather than a
/// `LatencyTrim::Reported` mode on the config because asking a graph is an
/// *action*, and folding it into a value dragged a `&mut Net` into what is
/// otherwise pure arithmetic:
///
/// ```ignore
/// let latency = reported_latency(&mut net);
/// let config = ExportConfig {
///     render: RenderConfig { latency, ..Default::default() },
///     ..Default::default()
/// };
/// ```
///
/// Floored: trimming a partial frame is not something a sink can do.
pub fn reported_latency(net: &mut tutti_core::dsp::Net) -> Samples {
    use tutti_core::AudioUnit;
    Samples(net.latency().unwrap_or(0.0).floor().max(0.0) as usize)
}

/// The tail `net` reports — how long it keeps ringing after its input stops.
///
/// For a caller that wants [`RenderConfig::tail`] to be whatever the graph says:
/// a reverb, a convolver, a hosted plugin that declared a decay. The mirror of
/// [`reported_latency`], and a function for the same reason — asking a graph is
/// an *action*, and folding it into a config value would drag a graph into
/// arithmetic that is otherwise pure.
///
/// Returns the figure **and its caveats** rather than a frame count, because for
/// two graphs there is no count: one that never decays, and one whose nodes were
/// never taught to answer. Resolving either into a number is a decision, so it
/// happens at the call site:
///
/// ```ignore
/// let reported = reported_tail(&net);
/// let tail = reported.samples().unwrap_or_else(|| {
///     // This bounce stops four seconds into an unbounded tail.
///     Seconds(4.0).to_samples(rate)
/// });
/// let config = ExportConfig {
///     render: RenderConfig { tail, ..Default::default() },
///     ..Default::default()
/// };
/// ```
pub fn reported_tail(net: &tutti_core::dsp::Net) -> tutti_types::GraphTail {
    tutti_types::tail::graph_tail(net)
}

/// Write already-rendered audio to `path`.
///
/// The third of the API, and the one that makes measure-then-apply usable:
/// render to buffers, measure, apply a gain, write. Without it a caller who
/// normalized would have nowhere to put the result.
///
/// `config.encode` is honoured as-is. `config.render` is not consulted — the frames
/// already exist — but `config.resample` still applies, so a caller can convert on
/// the way out.
pub fn write_buffers(rendered: &Rendered, config: &ExportConfig, path: &Path) -> Result<Written> {
    frame_width(config.encode.channels)?;
    encode::encode_planes(rendered, config, path)
}

/// Render `net` and write it to `path`.
///
/// Streams: the encoder pulls the graph one block at a time and no PCM is held
/// whole. `clock` is advanced once per block, after the net processes — pass
/// [`FrozenClock`] for a graph with no time-dependent nodes.
pub fn render_to_file(
    net: tutti_core::dsp::Net,
    config: &ExportConfig,
    clock: &dyn RenderClock,
    path: &Path,
) -> Result<Written> {
    frame_width(config.encode.channels)?;
    let mut net = net;
    let plan = render::RenderPlan::new(&config.render);
    let mut src = render::NetSource::new(&mut net, config.render.sample_rate, clock);
    encode::encode_to_file(&mut src, config.render.sample_rate, &plan, config, path)
}

/// Render `net` into memory.
///
/// Applies the same gate as [`render_to_file`], and reports the rate it actually
/// rendered at.
///
/// **Neither resampled nor dithered**, for the same reason in both cases: they
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
pub fn render_to_buffers(
    net: tutti_core::dsp::Net,
    config: &ExportConfig,
    clock: &dyn RenderClock,
) -> Result<Rendered> {
    let ch = frame_width(config.encode.channels)?;
    let mut net = net;
    let plan = render::RenderPlan::new(&config.render);
    let mut src = render::NetSource::new(&mut net, config.render.sample_rate, clock);

    // `vec![Vec::with_capacity(n); ch]` would clone ONE empty Vec `ch` times,
    // and a clone does not carry capacity — every plane would reallocate.
    let mut planes: Vec<Vec<f32>> = (0..ch)
        .map(|_| Vec::with_capacity(plan.output_length.get()))
        .collect();
    render::drive(&mut src, ch, &plan, |block| {
        for f in block.iter() {
            for (plane, &s) in planes.iter_mut().zip(f.iter()) {
                plane.push(s);
            }
        }
        Ok(())
    })?;

    Ok(Rendered {
        planes,
        sample_rate: config.render.sample_rate,
    })
}
