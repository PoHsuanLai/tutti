//! WAV (hound). Streams; hound back-patches the RIFF sizes on `finalize`.

use crate::config::ExportConfig;
use crate::encode::{pump_blocks, Encoder};
use crate::error::{Error, Result};
use crate::options::BitDepth;
use crate::render::{FrameSource, RenderPlan};
use hound::{SampleFormat, WavSpec, WavWriter};
use std::io::BufWriter;
use std::path::Path;
use tutti_core::pcm::Sample;

pub(crate) struct WavEncoder {
    writer: WavWriter<BufWriter<std::fs::File>>,
    bit_depth: BitDepth,
}

impl WavEncoder {
    pub(crate) fn create(path: &Path, config: &ExportConfig) -> Result<Self> {
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
        })
    }
}

impl Encoder for WavEncoder {
    fn encode(
        mut self,
        src: &mut dyn FrameSource,
        source_rate: tutti_core::SampleRate,
        plan: &RenderPlan,
        config: &ExportConfig,
    ) -> Result<()> {
        pump_blocks(src, source_rate, plan, config, |frames| {
            // Already interleaved — hound wants a flat sample stream, which is
            // exactly what the render hands over. The `interleave` helper this
            // replaced re-copied a buffer that was never de-interleaved.
            for &s in frames.samples() {
                // The depth dispatch is `tutti-types`', shared with the live
                // `WavOut` sink; only the writer call per variant is ours. An
                // offline render and a live capture therefore quantize a given
                // sample identically by construction, not by two hand-written
                // matches happening to agree.
                match self.bit_depth.quantize(s) {
                    Sample::I16(v) => self.writer.write_sample(v),
                    Sample::I24(v) => self.writer.write_sample(v),
                    Sample::F32(v) => self.writer.write_sample(v),
                }
                .map_err(io_err)?;
            }
            Ok(())
        })?;
        self.writer.finalize().map_err(io_err)?;
        Ok(())
    }
}

fn io_err(e: hound::Error) -> Error {
    Error::Io(std::io::Error::other(e))
}
