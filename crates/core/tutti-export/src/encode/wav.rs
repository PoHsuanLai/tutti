//! WAV (hound). Streams; hound back-patches the RIFF sizes on `finalize`.

use crate::encode::{interleave, pump_blocks, Encoder};
use crate::error::{Error, Result};
use crate::options::BitDepth;
use crate::render::{NetSource, RenderPlan};
use crate::spec::ExportSpec;
use hound::{SampleFormat, WavSpec, WavWriter};
use std::io::BufWriter;
use std::path::Path;
use tutti_core::pcm::{f32_to_i16, f32_to_i24};

pub(crate) struct WavEncoder {
    writer: WavWriter<BufWriter<std::fs::File>>,
    bit_depth: BitDepth,
}

impl WavEncoder {
    pub(crate) fn create(path: &Path, spec: &ExportSpec) -> Result<Self> {
        let (bits_per_sample, sample_format) = match spec.encode.bit_depth {
            BitDepth::Int16 => (16, SampleFormat::Int),
            BitDepth::Int24 => (24, SampleFormat::Int),
            BitDepth::Float32 => (32, SampleFormat::Float),
        };
        let writer = WavWriter::create(
            path,
            WavSpec {
                channels: spec.encode.channels.count(),
                sample_rate: spec.output_rate(),
                bits_per_sample,
                sample_format,
            },
        )
        .map_err(io_err)?;
        Ok(Self {
            writer,
            bit_depth: spec.encode.bit_depth,
        })
    }
}

impl<const CH: usize> Encoder<CH> for WavEncoder {
    fn encode(
        mut self,
        src: &mut NetSource<'_, CH>,
        plan: &RenderPlan,
        spec: &ExportSpec,
    ) -> Result<()> {
        let mut buf = Vec::new();
        pump_blocks(src, plan, spec, |frames| {
            interleave(frames, &mut buf);
            for &s in &buf {
                match self.bit_depth {
                    BitDepth::Int16 => self.writer.write_sample(f32_to_i16(s)),
                    BitDepth::Int24 => self.writer.write_sample(f32_to_i24(s)),
                    BitDepth::Float32 => self.writer.write_sample(s),
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
