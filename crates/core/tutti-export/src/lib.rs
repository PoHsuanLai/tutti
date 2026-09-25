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

/// A file that was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
    /// Where it landed — the `path` the entry point was given.
    pub path: PathBuf,
    /// Size on disk after finalization. `0` if the file could not be stat'd.
    ///
    /// This is the only thing an export reports back, which is why a
    /// normalization that silently failed to apply would leave no trace: no
    /// field here records the gain.
    pub bytes: u64,
}

/// Rendered audio, one `Vec` per channel.
///
/// Planes rather than a `(left, right)` pair: a pair cannot express a surround
/// render, so it forces the in-memory path to fold anything wider than stereo
/// and leaves callers hardcoding "2 channels" downstream because the type gives
/// them no other answer.
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
    /// FRAMES per plane — the length of one channel, not the total sample
    /// count.
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
        ChannelLayout::from(self.planes.len() as u16)
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

    /// Interleave into a caller-owned buffer, reusing its allocation.
    ///
    /// `out` is cleared first, so it is a destination and not an accumulator.
    /// This exists for the same reason
    /// [`Interleaved::fold_to_mono_into`](tutti_types::Interleaved::fold_to_mono_into)
    /// does — [`interleaved`](Self::interleaved) allocates once per call, and
    /// its callers measure the result and drop it. A two-pass normalize that
    /// interleaves twice in one scope pays that twice over a buffer it could
    /// have reused.
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

/// Write already-rendered audio to `path`.
///
/// The third of the API, and what makes measure-then-apply usable: render to
/// buffers, measure, apply a gain, write. Without it a caller who normalized has
/// nowhere to put the result.
///
/// `config.encode` is honoured as-is. `config.render` is not consulted — the
/// frames already exist and carry their own rate in [`Rendered::sample_rate`] —
/// but `config.resample` still applies, so a caller can convert on the way out.
/// Dither is applied here, at the real depth and after any resample, which is
/// the only point where one LSB is known.
pub fn write_buffers(rendered: &Rendered, config: &ExportConfig, path: &Path) -> Result<Written> {
    frame_width(config.encode.channels)?;
    encode::encode_planes(rendered, config, path)
}

/// Render `graph` and write it to `path`.
///
/// Streams: the encoder pulls the graph one block at a time and no PCM is held
/// whole. `clock` is advanced once per block, after the graph processes (per
/// 64 frames for a native graph holding `Legacy` units, which is rendered
/// chunk-major: [`RenderClock::render_graph`]) — pass
/// [`FrozenClock`] for a graph with no time-dependent nodes.
///
/// `graph` is a native graph, built for the render or forked from a live one
/// ([`RenderGraph`]).
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

/// Render `graph` ([`RenderGraph`]) into memory.
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
