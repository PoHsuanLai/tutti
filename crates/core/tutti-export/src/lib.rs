#![doc = include_str!("../README.md")]

mod error;
pub use error::{Error, Result};

mod options;
pub use options::{AudioFormat, BitDepth, Dither, Flac, Ogg};
pub use tutti_types::ChannelLayout;

mod config;
pub use config::{EncodeConfig, ExportConfig, RenderConfig, Resample};

mod normalize;
pub use normalize::{render_normalized_to_file, Normalize};

mod graph;
pub use graph::{RenderGraph, GRAPH_MAX_BLOCK};
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

/// A file an export wrote, returned by [`render_to_file`], [`write_buffers`]
/// and [`render_normalized_to_file`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
    /// Where it landed — the `path` the entry point was given.
    pub path: PathBuf,
    /// Size on disk after finalization, in bytes. `0` if the file could not
    /// be stat'd.
    pub bytes: u64,
}

/// Rendered audio held in memory, one `Vec` per channel.
///
/// Returned by [`render_to_buffers`]; written with [`write_buffers`]. Planes
/// rather than a `(left, right)` pair, so a surround render keeps every
/// channel.
#[derive(Debug, Clone, PartialEq)]
pub struct Rendered {
    /// One `Vec` per channel, all the same length. Planar, not interleaved —
    /// [`interleaved`](Self::interleaved) is the conversion.
    pub planes: Vec<Vec<f32>>,
    /// The rate these samples are **at**, which is not necessarily the rate a
    /// config asked to write: [`render_to_buffers`] does not resample, so this
    /// is always the render rate. An encoder must read it from here rather than
    /// from a config, or it converts from a rate the samples were never at.
    pub sample_rate: tutti_core::SampleRate,
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
        ChannelLayout::from(self.planes.len() as u16)
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
    /// **Allocates the whole render.** A `Rendered` holds an entire offline
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
        let frames = self.frames().get();
        out.reserve(frames * self.channels());
        for i in 0..frames {
            for plane in &self.planes {
                out.push(plane[i]);
            }
        }
    }
}

/// The frame width an export config asks for, as the interleave stride.
///
/// The one place a [`ChannelLayout`] becomes the `usize` stride the render and
/// the encoders use, and **zero is the only rejected width**. That the check is
/// a bound rather than a list of enumerated widths is what lets a 3- or 5-wide
/// master export at all: nothing here is generic over the count.
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
/// `config.encode` is honoured as-is. `config.render` is not consulted — the
/// frames already exist and carry their own rate in [`Rendered::sample_rate`] —
/// but `config.resample` still applies, so a caller can convert on the way out.
/// Dither is applied here, at the real depth and after any resample, which is
/// the only point where one LSB is known.
///
/// # Errors
///
/// [`Error::UnsupportedChannels`] for a zero-channel `config.encode.channels`;
/// [`Error::UnsupportedFormat`] if the format's feature is off or it cannot
/// write the bit depth; [`Error::InvalidConfig`], [`Error::Resample`],
/// [`Error::Encoding`] or [`Error::Io`] from the resample and encode stages.
pub fn write_buffers(rendered: &Rendered, config: &ExportConfig, path: &Path) -> Result<Written> {
    frame_width(config.encode.channels)?;
    encode::encode_planes(rendered, config, path)
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
/// Blocks until the file is finalized.
///
/// # Errors
///
/// As [`write_buffers`], plus [`Error::InvalidConfig`] if the graph was
/// prepared at another rate or its editor does not feed its executor, and
/// [`Error::ForkFailed`] if a forked node (a hosted plugin) failed during the
/// render — the file may exist but is not a valid render.
pub fn render_to_file(
    mut graph: RenderGraph,
    config: &ExportConfig,
    clock: &dyn RenderClock,
    path: &Path,
) -> Result<Written> {
    frame_width(config.encode.channels)?;
    let plan = render::RenderPlan::new(&config.render);
    render::with_source(&mut graph, config.render.sample_rate, clock, |src| {
        encode::encode_to_file(src, config.render.sample_rate, &plan, config, path)
    })
}

/// Renders `graph` ([`RenderGraph`]) into memory.
///
/// Renders the same frames as [`render_to_file`] and reports the rate it
/// actually rendered at. Only `config.render` and `config.encode.channels` are
/// read.
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
///
/// # Errors
///
/// [`Error::UnsupportedChannels`] for a zero-channel `config.encode.channels`;
/// [`Error::InvalidConfig`] if the graph was prepared at another rate or its
/// editor does not feed its executor; [`Error::ForkFailed`] if a forked node
/// failed during the render.
pub fn render_to_buffers(
    mut graph: RenderGraph,
    config: &ExportConfig,
    clock: &dyn RenderClock,
) -> Result<Rendered> {
    let ch = frame_width(config.encode.channels)?;
    let plan = render::RenderPlan::new(&config.render);

    // `vec![Vec::with_capacity(n); ch]` would clone ONE empty Vec `ch` times,
    // and a clone does not carry capacity — every plane would reallocate.
    let mut planes: Vec<Vec<f32>> = (0..ch)
        .map(|_| Vec::with_capacity(plan.output_length.get()))
        .collect();
    render::with_source(&mut graph, config.render.sample_rate, clock, |src| {
        render::drive(src, ch, &plan, |block| {
            for f in block.iter() {
                for (plane, &s) in planes.iter_mut().zip(f.iter()) {
                    plane.push(s);
                }
            }
            Ok(())
        })
    })?;

    Ok(Rendered {
        planes,
        sample_rate: config.render.sample_rate,
    })
}
