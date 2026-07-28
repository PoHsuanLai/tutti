//! AIFF (`aifc`). Streams; `finalize` back-patches the COMM/SSND sizes.
//!
//! This replaced ~200 lines of hand-rolled IFF chunk writing, including a
//! by-hand 80-bit IEEE 754 extended encoder for the sample-rate field. That code
//! carried a comment saying AIFF "requires total size up front" and so could not
//! stream — which was a property of the hand-rolled writer, not of the format:
//! `aifc` takes samples incrementally and patches the sizes at `finalize`, the
//! same way hound does for RIFF.
//!
//! `SampleFormat::{I16, I24, F32}` line up exactly with our three bit depths, so
//! there is no conversion policy here beyond the PCM scaling every format does.

use crate::config::ExportConfig;
use crate::encode::{interleave, pump_blocks, Encoder};
use crate::error::{Error, Result};
use crate::options::BitDepth;
use crate::render::{FrameSource, RenderPlan};
use aifc::{AifcWriteInfo, AifcWriter, FileFormat, SampleFormat};
use std::io::BufWriter;
use std::path::Path;
use tutti_core::pcm::{f32_to_i16, f32_to_i24};

pub(crate) struct AiffEncoder {
    writer: AifcWriter<BufWriter<std::fs::File>>,
    bit_depth: BitDepth,
}

impl AiffEncoder {
    pub(crate) fn create(path: &Path, config: &ExportConfig) -> Result<Self> {
        let sample_format = match config.encode.bit_depth {
            BitDepth::Int16 => SampleFormat::I16,
            BitDepth::Int24 => SampleFormat::I24,
            BitDepth::Float32 => SampleFormat::F32,
        };
        let info = AifcWriteInfo {
            // Float samples are an AIFF-C extension; plain AIFF is integer-only,
            // so the container follows the depth rather than the other way
            // round.
            file_format: match config.encode.bit_depth {
                BitDepth::Float32 => FileFormat::Aifc,
                _ => FileFormat::Aiff,
            },
            channels: config.encode.channels.count() as i16,
            sample_rate: crate::encode::output_rate(config).get(),
            sample_format,
        };
        let file = BufWriter::new(std::fs::File::create(path)?);
        let writer =
            AifcWriter::new(file, &info).map_err(|e| Error::Encoding(format!("AIFF: {e:?}")))?;
        Ok(Self {
            writer,
            bit_depth: config.encode.bit_depth,
        })
    }
}

impl<const CH: usize> Encoder<CH> for AiffEncoder {
    fn encode(
        mut self,
        src: &mut dyn FrameSource<CH>,
        plan: &RenderPlan,
        config: &ExportConfig,
    ) -> Result<()> {
        let mut buf = Vec::new();
        let mut ints: Vec<i32> = Vec::new();
        let mut shorts: Vec<i16> = Vec::new();
        pump_blocks(src, plan, config, |frames| {
            interleave(frames, &mut buf);
            match self.bit_depth {
                BitDepth::Int16 => {
                    shorts.clear();
                    shorts.extend(buf.iter().map(|&s| f32_to_i16(s)));
                    self.writer.write_samples_i16(&shorts)
                }
                BitDepth::Int24 => {
                    ints.clear();
                    ints.extend(buf.iter().map(|&s| f32_to_i24(s)));
                    self.writer.write_samples_i24(&ints)
                }
                BitDepth::Float32 => self.writer.write_samples_f32(&buf),
            }
            .map_err(|e| Error::Encoding(format!("AIFF write failed: {e:?}")))
        })?;

        self.writer
            .finalize()
            .map_err(|e| Error::Encoding(format!("AIFF finalization failed: {e:?}")))?;
        Ok(())
    }
}
