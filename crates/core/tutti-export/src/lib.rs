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
//! **It does not normalize.** Choosing a gain means measuring the whole signal
//! first, which is two passes and a caller's composition. Measure with
//! `tutti_analysis::loudness` (a streaming meter — run it *while* rendering),
//! take `Loudness::gain_to`, and apply it. Hiding that inside an export is what
//! forced the whole signal into memory before.
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

#[cfg(feature = "bevy")]
pub mod ecs;

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

    /// Channel count.
    pub fn channels(&self) -> usize {
        self.planes.len()
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

/// Dispatch a `$body` block, generic over `const CH: usize`, on a runtime
/// [`ChannelLayout`].
///
/// The render pipeline is const-generic in its frame width, so the caller
/// resolves the requested layout to one of the enumerated widths (1/2/4/6/8/12
/// — mono through 7.1.4) and the whole pipeline monomorphizes at it. An
/// unenumerated width is a clean [`Error::UnsupportedChannels`], never a silent
/// channel drop.
macro_rules! dispatch_channels {
    ($layout:expr, $ch:ident => $body:block) => {{
        match $layout.count() {
            1 => {
                const $ch: usize = 1;
                $body
            }
            2 => {
                const $ch: usize = 2;
                $body
            }
            4 => {
                const $ch: usize = 4;
                $body
            }
            6 => {
                const $ch: usize = 6;
                $body
            }
            8 => {
                const $ch: usize = 8;
                $body
            }
            12 => {
                const $ch: usize = 12;
                $body
            }
            n => Err(Error::UnsupportedChannels(n)),
        }
    }};
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
    let channels = config.encode.channels;
    dispatch_channels!(channels, CH => {
        encode::encode_planes::<CH>(rendered, config, path)
    })
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
    let channels = config.encode.channels;
    dispatch_channels!(channels, CH => {
        let mut net = net;
        let plan = render::RenderPlan::new(&config.render);
        let mut src = render::NetSource::<CH>::new(&mut net, config.render.sample_rate, clock);
        encode::encode_to_file::<CH>(&mut src, config.render.sample_rate, &plan, config, path)
    })
}

/// Render `net` into memory.
///
/// Applies the same gate and dither as [`render_to_file`], and reports the rate
/// it actually rendered at. Resampling is **not** applied here — that is
/// `config.resample`'s job at the file boundary, and a caller holding planes can
/// resample them itself; reporting a rate the samples are not at is the bug this
/// shape avoids.
pub fn render_to_buffers(
    net: tutti_core::dsp::Net,
    config: &ExportConfig,
    clock: &dyn RenderClock,
) -> Result<Rendered> {
    let channels = config.encode.channels;
    dispatch_channels!(channels, CH => {
        let mut net = net;
        let plan = render::RenderPlan::new(&config.render);
        let mut src = render::NetSource::<CH>::new(&mut net, config.render.sample_rate, clock);

        // `vec![Vec::with_capacity(n); CH]` would clone ONE empty Vec CH times,
        // and a clone does not carry capacity — every plane would reallocate.
        let mut planes: Vec<Vec<f32>> =
            (0..CH).map(|_| Vec::with_capacity(plan.output_length.get())).collect();
        let mut dither = process::DitherState::for_config(config);
        let mut staging: Vec<[f32; CH]> = Vec::new();
        render::drive(&mut src, &plan, |block| {
            staging.clear();
            staging.extend_from_slice(block);
            dither.apply(&mut staging);
            for f in &staging {
                for (plane, &s) in planes.iter_mut().zip(f.iter()) {
                    plane.push(s);
                }
            }
            Ok(())
        })?;

        Ok(Rendered { planes, sample_rate: config.render.sample_rate })
    })
}
