//! Ogg Vorbis (vorbis_rs). Streams via `encode_audio_block`.
//!
//! vorbis_rs takes planar per-channel slices, so each block is deinterleaved on
//! the way through. Vorbis's channel mappings cover mono, stereo, and 3–8
//! surround, so this carries any width the export dispatch admits below that.
//!
//! Vorbis is lossy and always float internally, so `bit_depth` and `dither` do
//! not apply — quantization noise has nothing to dither against here.

use crate::encode::Encoder;
use crate::error::{Error, Result};
use crate::render::{drive, FrameSource, RenderPlan};
use crate::spec::ExportSpec;
use std::io::BufWriter;
use std::num::{NonZeroU32, NonZeroU8};
use std::path::Path;
use vorbis_rs::{VorbisBitrateManagementStrategy, VorbisEncoderBuilder};

pub(crate) struct OggEncoder {
    encoder: vorbis_rs::VorbisEncoder<BufWriter<std::fs::File>>,
}

impl OggEncoder {
    pub(crate) fn create(path: &Path, spec: &ExportSpec) -> Result<Self> {
        let sr = NonZeroU32::new(spec.output_rate())
            .ok_or_else(|| Error::InvalidConfig("Sample rate must be non-zero".into()))?;
        let ch = NonZeroU8::new(spec.encode.channels.count() as u8)
            .ok_or_else(|| Error::InvalidConfig("Channel count must be non-zero".into()))?;

        let writer = BufWriter::new(std::fs::File::create(path)?);
        let mut builder = VorbisEncoderBuilder::new(sr, ch, writer)
            .map_err(|e| Error::Encoding(format!("Failed to create OGG encoder: {e}")))?;
        builder.bitrate_management_strategy(VorbisBitrateManagementStrategy::QualityVbr {
            target_quality: spec.encode.ogg.quality,
        });
        let encoder = builder
            .build()
            .map_err(|e| Error::Encoding(format!("Failed to build OGG encoder: {e}")))?;
        Ok(Self { encoder })
    }
}

impl<const CH: usize> Encoder<CH> for OggEncoder {
    fn encode(
        mut self,
        src: &mut dyn FrameSource<CH>,
        plan: &RenderPlan,
        _spec: &ExportSpec,
    ) -> Result<()> {
        // Reused planar staging, so a block deinterleaves without allocating.
        let mut planes: Vec<Vec<f32>> = vec![Vec::new(); CH];
        drive(src, plan, |frames| {
            for p in planes.iter_mut() {
                p.clear();
                p.reserve(frames.len());
            }
            for f in frames {
                for (p, &s) in planes.iter_mut().zip(f.iter()) {
                    p.push(s);
                }
            }
            let block: Vec<&[f32]> = planes.iter().map(|p| p.as_slice()).collect();
            self.encoder
                .encode_audio_block(block.as_slice())
                .map_err(|e| Error::Encoding(format!("OGG encoding failed: {e}")))
        })?;

        self.encoder
            .finish()
            .map_err(|e| Error::Encoding(format!("OGG finalization failed: {e}")))?;
        Ok(())
    }
}
