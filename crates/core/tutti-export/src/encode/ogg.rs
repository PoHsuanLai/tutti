//! Ogg Vorbis (vorbis_rs). Streams via `encode_audio_block`.
//!
//! vorbis_rs takes planar per-channel slices, so each block is deinterleaved on
//! the way through. Vorbis's channel mappings cover mono, stereo, and 3–8
//! surround; a width past 8 is vorbis's own limit, reported by its builder — the
//! rest of this crate carries any width.
//!
//! Vorbis is lossy and always float internally, so `bit_depth` and `dither` do
//! not apply — quantization noise has nothing to dither against here.

use crate::config::ExportConfig;
use crate::encode::{pump_blocks, Encoder};
use crate::error::{Error, Result};
use crate::render::{FrameSource, RenderPlan};
use std::io::BufWriter;
use std::num::{NonZeroU32, NonZeroU8};
use std::path::Path;
use vorbis_rs::{VorbisBitrateManagementStrategy, VorbisEncoderBuilder};

pub(crate) struct OggEncoder {
    encoder: vorbis_rs::VorbisEncoder<BufWriter<std::fs::File>>,
}

impl OggEncoder {
    pub(crate) fn create(
        path: &Path,
        config: &ExportConfig,
        opts: crate::options::Ogg,
    ) -> Result<Self> {
        let sr = NonZeroU32::new(crate::encode::encoder_rate(config))
            .ok_or_else(|| Error::InvalidConfig("Sample rate must be non-zero".into()))?;
        let ch = NonZeroU8::new(config.encode.channels.count() as u8)
            .ok_or_else(|| Error::InvalidConfig("Channel count must be non-zero".into()))?;

        let writer = BufWriter::new(std::fs::File::create(path)?);
        let mut builder = VorbisEncoderBuilder::new(sr, ch, writer)
            .map_err(|e| Error::Encoding(format!("Failed to create OGG encoder: {e}")))?;
        builder.bitrate_management_strategy(VorbisBitrateManagementStrategy::QualityVbr {
            target_quality: opts.quality,
        });
        let encoder = builder
            .build()
            .map_err(|e| Error::Encoding(format!("Failed to build OGG encoder: {e}")))?;
        Ok(Self { encoder })
    }
}

impl Encoder for OggEncoder {
    fn encode(
        mut self,
        src: &mut dyn FrameSource,
        source_rate: tutti_core::SampleRate,
        plan: &RenderPlan,
        config: &ExportConfig,
    ) -> Result<()> {
        // Through `pump_blocks`, not `drive` — that is what applies the
        // resample. Calling `drive` directly while still taking the header rate
        // from `encoder_rate` writes un-resampled audio under a header claiming
        // the target rate, playing back 8.8% fast at 44.1->48 k.
        //
        // Vorbis is lossy and float internally, so `pump_blocks`'s dither stage
        // is a no-op here by construction — `DitherState::for_config` arms only
        // for an integer bit depth.

        // The stride, derived once at entry — not re-derived per block.
        let ch = config.encode.channels.count() as usize;
        let mut planes: Vec<Vec<f32>> = vec![Vec::new(); ch];
        pump_blocks(src, source_rate, plan, config, |frames| {
            for p in planes.iter_mut() {
                p.clear();
                p.reserve(frames.len());
            }
            for f in frames.iter() {
                for (p, &s) in planes.iter_mut().zip(f.iter()) {
                    p.push(s);
                }
            }
            // An EMPTY block is vorbis's end-of-stream signal, and `pump_blocks`
            // legitimately emits one (the resampler flushes after its last real
            // block). Passing it through closes the stream early and leaves the
            // encoder writing garbage pages — a 1 s render comes out 178 KB
            // instead of 7.8 KB. Dropping it here is load-bearing.
            if frames.is_empty() {
                return Ok(());
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
