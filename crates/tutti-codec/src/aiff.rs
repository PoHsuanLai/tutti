//! AIFF (`aifc`). Streams; `finish` back-patches the COMM/SSND sizes.
//!
//! AIFF does not require a total size up front: `aifc` takes samples
//! incrementally and patches the sizes at `finalize`, the same way a WAV
//! header is patched. It also owns the 80-bit IEEE 754 extended encoding of the
//! sample-rate field, which is the part worth not writing by hand.
//!
//! `SampleFormat::{I16, I24, F32}` line up exactly with the three [`Pcm`]
//! widths, so there is no conversion policy here beyond the PCM scaling every
//! format shares.

use crate::error::{Codec, Error, Result, SpecError};
use crate::pcm::{Pcm, f32_to_i16, f32_to_i24};
use crate::spec::Spec;
use crate::writer::FormatWriter;
use aifc::{AifcWriteInfo, AifcWriter, FileFormat, SampleFormat};
use std::io::{Seek, Write};
use tutti_types::Interleaved;

/// Writes an AIFF (or AIFF-C) file through the [`FormatWriter`] contract.
pub struct AiffWriter<W: Write + Seek> {
    writer: AifcWriter<W>,
    pcm: Pcm,
    ints: Vec<i32>,
    shorts: Vec<i16>,
}

impl<W: Write + Seek> std::fmt::Debug for AiffWriter<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AiffWriter").finish_non_exhaustive()
    }
}

impl<W: Write + Seek> AiffWriter<W> {
    /// Starts the file in `sink`, writing its header.
    ///
    /// # Errors
    ///
    /// As [`Encoder::create`](crate::Encoder::create).
    pub fn create(spec: &Spec, pcm: Pcm, sink: W) -> Result<Self> {
        spec.header_rate()?;
        let channels = spec.channels()?;
        let info = AifcWriteInfo {
            // Float samples are an AIFF-C extension; plain AIFF is integer-only,
            // so the container follows the width rather than the other way
            // round.
            file_format: match pcm {
                Pcm::Float32 => FileFormat::Aifc,
                _ => FileFormat::Aiff,
            },
            channels: i16::try_from(channels).map_err(|_| SpecError::TooManyChannels {
                channels,
                max: i16::MAX.unsigned_abs(),
            })?,
            // The extended float the header states; the one rate that is not
            // narrowed to whole Hz.
            sample_rate: spec.rate.get(),
            sample_format: match pcm {
                Pcm::Int16 => SampleFormat::I16,
                Pcm::Int24 => SampleFormat::I24,
                Pcm::Float32 => SampleFormat::F32,
            },
        };
        let writer = AifcWriter::new(sink, &info)
            .map_err(|e| Error::encoding(Codec::Aiff, format!("{e:?}")))?;
        Ok(Self {
            writer,
            pcm,
            ints: Vec::new(),
            shorts: Vec::new(),
        })
    }
}

impl<W: Write + Seek> FormatWriter for AiffWriter<W> {
    type Sink = W;

    fn write(&mut self, block: Interleaved<'_>) -> Result<()> {
        // `aifc` takes an interleaved sample run, which is the shape the block
        // already has.
        let buf = block.samples();
        match self.pcm {
            Pcm::Int16 => {
                self.shorts.clear();
                self.shorts.extend(buf.iter().map(|&s| f32_to_i16(s)));
                self.writer.write_samples_i16(&self.shorts)
            }
            Pcm::Int24 => {
                self.ints.clear();
                self.ints.extend(buf.iter().map(|&s| f32_to_i24(s)));
                self.writer.write_samples_i24(&self.ints)
            }
            Pcm::Float32 => self.writer.write_samples_f32(buf),
        }
        .map_err(|e| Error::encoding(Codec::Aiff, format!("write failed: {e:?}")))
    }

    fn finish(mut self) -> Result<Self::Sink> {
        self.writer
            .finalize()
            .map_err(|e| Error::encoding(Codec::Aiff, format!("finalization failed: {e:?}")))?;
        Ok(self.writer.into_inner())
    }
}
