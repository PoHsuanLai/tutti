//! WAV (hound). Streams; hound back-patches the RIFF sizes on `finalize`, and the
//! speaker mask of a multichannel file is patched in after it (`tutti_types::wav`).
//!
//! A WAV file's sizes are 32-bit: an export that would pass them fails with
//! [`Error::WavTooLarge`] before the file is created when the plan already shows
//! it (an existing file at the path is left alone), and otherwise at the frame
//! that crosses the limit, which removes the partial file it had begun.
//!
//! The offline twin of `tutti_io::WavOut`, and the two stay separate because
//! this one pulls (it drives the render to a planned frame count) while a
//! live sink is pushed blocks by whoever owns the loop. They share the
//! quantization — both dispatch through
//! [`BitDepth::quantize`](tutti_types::pcm::BitDepth::quantize) — so a bounced
//! and a recorded WAV agree sample-for-sample at a given depth. The one
//! deliberate difference: an export dithers first, a live capture does not.

use crate::config::ExportConfig;
use crate::encode::{Encoder, pump_blocks};
use crate::error::{ConfigError, Error, Result};
use crate::options::BitDepth;
use crate::render::{FrameSource, RenderPlan};
use hound::{SampleFormat, WavSpec, WavWriter};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use tutti_types::ChannelLayout;
use tutti_types::pcm::Sample;
use tutti_types::wav;

pub(crate) struct WavEncoder {
    writer: WavWriter<BufWriter<std::fs::File>>,
    bit_depth: BitDepth,
    path: PathBuf,
    /// The speaker mask the finished file declares.
    mask: u32,
    layout: ChannelLayout,
    /// The frames the file may hold: [`wav::max_data_frames`], lowered by tests.
    max_frames: u64,
}

impl WavEncoder {
    /// Opens the file for `plan`, or fails with [`Error::WavTooLarge`] without
    /// touching `path` when the plan cannot fit.
    pub(crate) fn create(
        path: &Path,
        config: &ExportConfig,
        source_rate: tutti_types::SampleRate,
        plan: &RenderPlan,
    ) -> Result<Self> {
        let max_frames = wav::max_data_frames(config.encode.channels, config.encode.bit_depth);
        Self::create_with_limit(path, config, source_rate, plan, max_frames)
    }

    /// [`create`](Self::create) with the frame limit a test lowers.
    fn create_with_limit(
        path: &Path,
        config: &ExportConfig,
        source_rate: tutti_types::SampleRate,
        plan: &RenderPlan,
        max_frames: u64,
    ) -> Result<Self> {
        let mask = match &config.encode.topology {
            Some(t) if t.layout() != config.encode.channels => {
                return Err(ConfigError::TopologyWidth {
                    topology: t.layout(),
                    file: config.encode.channels,
                }
                .into());
            }
            Some(t) => wav::channel_mask(t),
            None => wav::default_channel_mask(config.encode.channels),
        };
        // Before `WavWriter::create`, which truncates an existing file: a
        // refused export must leave what is at `path` alone.
        if Self::planned_frames(plan, source_rate, config) > max_frames {
            return Err(Error::WavTooLarge {
                limit_frames: max_frames,
            });
        }
        let (bits_per_sample, sample_format) = match config.encode.bit_depth {
            BitDepth::Int16 => (16, SampleFormat::Int),
            BitDepth::Int24 => (24, SampleFormat::Int),
            BitDepth::Float32 => (32, SampleFormat::Float),
        };
        let writer = WavWriter::create(
            path,
            WavSpec {
                channels: config.encode.channels.count(),
                sample_rate: crate::encode::encoder_rate(config),
                bits_per_sample,
                sample_format,
            },
        )
        .map_err(io_err)?;
        Ok(Self {
            writer,
            bit_depth: config.encode.bit_depth,
            path: path.to_path_buf(),
            mask,
            layout: config.encode.channels,
            max_frames,
        })
    }

    /// Frames the encoder will write for `plan` at the output rate, rounded
    /// up: the plan's kept frames, scaled only when the driver resamples
    /// (the condition `pump_blocks` uses).
    fn planned_frames(
        plan: &RenderPlan,
        source_rate: tutti_types::SampleRate,
        config: &ExportConfig,
    ) -> u64 {
        let kept = u128::from(plan.output_length.get() as u64);
        if config.resample.is_none() {
            return kept as u64;
        }
        let (out, src) = (
            f64::from(crate::encode::encoder_rate(config)),
            source_rate.get().round(),
        );
        if src <= 0.0 || out == src {
            return kept as u64;
        }
        (kept as f64 * out / src).ceil() as u64
    }
}

/// Counts the frames written against the file's limit.
struct FrameBudget {
    written: u64,
    max: u64,
}

impl FrameBudget {
    /// Takes `frames` more, or fails with [`Error::WavTooLarge`] if the file
    /// would then hold more than `max`.
    fn admit(&mut self, frames: u64) -> Result<()> {
        self.written = self.written.saturating_add(frames);
        if self.written > self.max {
            return Err(Error::WavTooLarge {
                limit_frames: self.max,
            });
        }
        Ok(())
    }
}

impl Encoder for WavEncoder {
    fn encode(
        self,
        src: &mut dyn FrameSource,
        source_rate: tutti_types::SampleRate,
        plan: &RenderPlan,
        config: &ExportConfig,
    ) -> Result<()> {
        let mut budget = FrameBudget {
            written: 0,
            max: self.max_frames,
        };
        let Self {
            mut writer,
            bit_depth,
            path,
            mask,
            layout,
            ..
        } = self;
        let width = u64::from(layout.count().max(1));
        let pumped = pump_blocks(src, source_rate, plan, config, |frames| {
            budget.admit(frames.samples().len() as u64 / width)?;
            // Already interleaved — hound wants a flat sample stream, which is
            // exactly what the render hands over. No de-interleave step to undo.
            for &s in frames.samples() {
                // The depth dispatch is `tutti-types`', shared with the live
                // `WavOut` sink; only the writer call per variant is local. An
                // offline render and a live capture therefore quantize a given
                // sample identically by construction, not by two hand-written
                // matches happening to agree.
                match bit_depth.quantize(s) {
                    Sample::I16(v) => writer.write_sample(v),
                    Sample::I24(v) => writer.write_sample(v),
                    Sample::F32(v) => writer.write_sample(v),
                }
                .map_err(io_err)?;
            }
            Ok(())
        });
        if let Err(e @ Error::WavTooLarge { .. }) = pumped {
            // The file holds a truncated render, which the error says is not an
            // export: close it, then take it away rather than leave it looking
            // like one.
            drop(writer);
            let _ = std::fs::remove_file(&path);
            return Err(e);
        }
        pumped?;
        writer.finalize().map_err(io_err)?;
        // `hound` writes the lowest-N-bits speaker mask and has no way to change
        // it; the one for the channels' speakers goes in over the header.
        wav::patch_channel_mask_at(&path, mask)?;
        Ok(())
    }
}

fn io_err(e: hound::Error) -> Error {
    Error::Io(std::io::Error::other(e))
}

#[cfg(test)]
mod tests;
