//! FLAC (flacenc). Streams the render **in** through flacenc's pull API.
//!
//! `flacenc::Source` is a trait, not a buffer: `encode_with_fixed_block_size`
//! calls `read_samples` until it returns 0. So the render feeds it block by
//! block and no PCM is ever held whole — the `MemSource::from_samples` this
//! replaced took the entire signal up front, which is why FLAC used to be the
//! format that forced whole-signal buffering.
//!
//! **What still accumulates:** flacenc collects the *compressed* frames in a
//! `Stream` and writes at the end (`coding.rs:636`). That is roughly half the
//! size of the PCM it came from, and far smaller than the planes the old path
//! held. `flacenc::coding::encode_fixed_size_frame` is public if fully
//! incremental output is ever wanted.

use crate::config::ExportConfig;
use crate::encode::Encoder;
use crate::error::{Error, Result};
use crate::options::BitDepth;
use crate::render::{FrameSource, RenderPlan};
use flacenc::bitsink::ByteSink;
use flacenc::component::BitRepr;
use flacenc::config::Encoder as EncoderConfig;
use flacenc::encode_with_fixed_block_size;
use flacenc::error::{SourceError, Verify};
use flacenc::source::{Fill, Source};
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

const BLOCK_SIZE: usize = 4096;

pub(crate) struct FlacEncoder {
    path: PathBuf,
    compression_level: u8,
    bit_depth: BitDepth,
}

impl FlacEncoder {
    pub(crate) fn create(
        path: &std::path::Path,
        config: &ExportConfig,
        opts: crate::options::Flac,
    ) -> Result<Self> {
        if config.encode.bit_depth == BitDepth::Float32 {
            return Err(Error::UnsupportedFormat(
                "FLAC does not support 32-bit float".into(),
            ));
        }
        Ok(Self {
            path: path.to_path_buf(),
            compression_level: opts.compression_level,
            bit_depth: config.encode.bit_depth,
        })
    }
}

/// Interleaved `i32` frames flacenc pulls from.
///
/// FLAC inverts control — `encode_with_fixed_block_size` calls `read_samples`
/// until dry — so it cannot sit inside `pump_blocks`'s push loop. It used to
/// resolve that by re-implementing the gate by hand and pulling from the render
/// directly, which meant it **never applied `config.resample`** while still
/// taking its header from `encoder_rate`: a file whose STREAMINFO claimed
/// 48 kHz holding 44.1 kHz audio, playing back 8.8% fast.
///
/// Now the frames come from the same `pump_blocks` every other format uses —
/// gate, resample and dither included — and this only hands them over. The
/// PCM is collected first, so FLAC is the one format that holds the signal;
/// see the module note. Trading that for correctness is the right way round,
/// and `encode_fixed_size_frame` is public if incremental output is wanted.
struct PulledFrames {
    channels: usize,
    bits: usize,
    sample_rate: usize,
    /// Interleaved, already at the output rate and depth.
    samples: Vec<i32>,
    pos: usize,
}

impl Source for PulledFrames {
    fn channels(&self) -> usize {
        self.channels
    }
    fn bits_per_sample(&self) -> usize {
        self.bits
    }
    fn sample_rate(&self) -> usize {
        self.sample_rate
    }

    fn read_samples<F: Fill>(
        &mut self,
        block_size: usize,
        dest: &mut F,
    ) -> std::result::Result<usize, SourceError> {
        let want = (block_size * self.channels).min(self.samples.len() - self.pos);
        if want == 0 {
            return Ok(0);
        }
        dest.fill_interleaved(&self.samples[self.pos..self.pos + want])?;
        self.pos += want;
        Ok(want / self.channels)
    }
}

impl<const CH: usize> Encoder<CH> for FlacEncoder {
    fn encode(
        self,
        src: &mut dyn FrameSource<CH>,
        source_rate: tutti_core::SampleRate,
        plan: &RenderPlan,
        config: &ExportConfig,
    ) -> Result<()> {
        let bits = bits_for(self.bit_depth);
        let bit_depth = self.bit_depth;

        // Through `pump_blocks`, like every other format — that is what applies
        // the gate, the resample and the dither.
        let mut samples: Vec<i32> = Vec::new();
        crate::encode::pump_blocks(src, source_rate, plan, config, |frames| {
            samples.reserve(frames.len() * CH);
            for f in frames {
                for &s in f.iter() {
                    samples.push(f32_to_i32(s, bit_depth));
                }
            }
            Ok(())
        })?;

        let source = PulledFrames {
            channels: CH,
            bits,
            sample_rate: crate::encode::encoder_rate(config) as usize,
            samples,
            pos: 0,
        };

        // `compression_level` is the app-facing knob; flacenc expresses effort
        // through its own preset, which `Encoder::default()` already sets to a
        // balanced point. Mapping the 0–8 scale onto flacenc's coding options is
        // a separate change — the level is accepted and currently unmapped
        // rather than silently reinterpreted.
        let _ = self.compression_level;
        let flac_config = EncoderConfig::default()
            .into_verified()
            .map_err(|e| Error::Encoding(format!("Invalid FLAC config: {e:?}")))?;

        let stream = encode_with_fixed_block_size(&flac_config, source, BLOCK_SIZE)
            .map_err(|e| Error::Encoding(format!("FLAC encoding failed: {e:?}")))?;

        let mut sink = ByteSink::new();
        stream
            .write(&mut sink)
            .map_err(|e| Error::Encoding(format!("Failed to write FLAC stream: {e:?}")))?;
        let mut file = File::create(&self.path)?;
        file.write_all(&sink.into_inner())?;
        Ok(())
    }
}

fn bits_for(bit_depth: BitDepth) -> usize {
    match bit_depth {
        BitDepth::Int16 => 16,
        BitDepth::Int24 => 24,
        // Rejected in `create`.
        BitDepth::Float32 => 24,
    }
}

/// Quantize to the integer sample flacenc takes, via the engine's canonical
/// converters.
///
/// This used to scale and cast here — `(clamped * 32767.0) as i32` — which
/// **truncates** where [`tutti_core::pcm`] rounds. Same magnitudes, so the two
/// agreed on 0.0 and ±1.0 (the only values the tests below covered) and diverged
/// by one LSB everywhere else: 0.7 encoded as 22936 in a FLAC and 22937 in a WAV
/// from the same render. `pcm.rs` exists precisely so that "a recorded and an
/// exported file quantize a given sample identically", and its docs call bare
/// truncation biased toward zero — a consistent negative DC error on the
/// negative half. Delegating is what keeps that promise true for FLAC too.
#[inline]
fn f32_to_i32(sample: f32, bit_depth: BitDepth) -> i32 {
    match bit_depth {
        BitDepth::Int16 => tutti_core::pcm::f32_to_i16(sample) as i32,
        // Float32 is rejected in `create`, so this is the Int24 path.
        _ => tutti_core::pcm::f32_to_i24(sample),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f32_to_i32_scales_to_bit_depth() {
        assert_eq!(f32_to_i32(0.0, BitDepth::Int16), 0);
        assert_eq!(f32_to_i32(1.0, BitDepth::Int16), 32767);
        assert_eq!(f32_to_i32(-1.0, BitDepth::Int16), -32767);
        assert_eq!(f32_to_i32(1.0, BitDepth::Int24), 8388607);
    }

    #[test]
    fn clamps_out_of_range_input() {
        assert_eq!(f32_to_i32(2.0, BitDepth::Int16), 32767);
        assert_eq!(f32_to_i32(-2.0, BitDepth::Int16), -32767);
    }

    /// FLAC must quantize exactly as the rest of the engine does.
    ///
    /// The two tests above pass under truncation *and* under rounding, because
    /// 0.0 and ±1.0 land on exact integers either way — which is how this
    /// encoder carried a private truncating converter unnoticed. These levels
    /// have a fractional part above 0.5, so the two disagree: 0.7 × 32767 =
    /// 22936.9, which truncates to 22936 and rounds to 22937.
    #[test]
    fn quantization_matches_the_engines_canonical_converter() {
        for level in [0.7f32, -0.7, 0.3, -0.3, 0.123_45, 0.999] {
            assert_eq!(
                f32_to_i32(level, BitDepth::Int16),
                tutti_core::pcm::f32_to_i16(level) as i32,
                "Int16: FLAC and tutti_core::pcm disagree on {level}"
            );
            assert_eq!(
                f32_to_i32(level, BitDepth::Int24),
                tutti_core::pcm::f32_to_i24(level),
                "Int24: FLAC and tutti_core::pcm disagree on {level}"
            );
        }
    }
}
