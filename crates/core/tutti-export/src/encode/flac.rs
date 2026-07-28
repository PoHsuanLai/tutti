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

use crate::encode::Encoder;
use crate::error::{Error, Result};
use crate::options::BitDepth;
use crate::render::{FrameSource, RenderPlan};
use crate::spec::ExportSpec;
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
    pub(crate) fn create(path: &std::path::Path, spec: &ExportSpec) -> Result<Self> {
        if spec.encode.bit_depth == BitDepth::Float32 {
            return Err(Error::UnsupportedFormat(
                "FLAC does not support 32-bit float".into(),
            ));
        }
        Ok(Self {
            path: path.to_path_buf(),
            compression_level: spec.encode.flac.compression_level,
            bit_depth: spec.encode.bit_depth,
        })
    }
}

/// Adapts the render to `flacenc::Source`.
///
/// flacenc asks for `block_size` frames at a time; we pull that many from the
/// render (already gated and dithered), convert to `i32` at the target depth,
/// and hand them over. A short read means the render is done, which is exactly
/// flacenc's stop condition.
struct RenderSource<'a, const CH: usize> {
    src: &'a mut dyn FrameSource<CH>,
    plan: &'a RenderPlan,
    dither: crate::process::DitherState,
    channels: usize,
    bits: usize,
    sample_rate: usize,
    bit_depth: BitDepth,
    /// Frames pulled but not yet handed to flacenc, interleaved as `i32`.
    pending: Vec<i32>,
    /// Frames that have passed the gate, ever — the `kept_so_far` the cursor
    /// needs, which is not `pending.len()` once flacenc starts draining.
    pending_frames_total: usize,
    done: bool,
}

impl<const CH: usize> RenderSource<'_, CH> {
    /// Fill `pending` until it holds at least `want` frames or the render ends.
    fn pull_until(&mut self, want: usize) {
        while !self.done && self.pending.len() < want * self.channels {
            let before = self.pending.len();
            let mut staging: Vec<[f32; CH]> = Vec::new();
            // One block per call: `drive` runs the whole render, so instead we
            // step the source directly and apply the same gate `drive` would.
            let produced = self.src.produced();
            if produced >= self.plan.total {
                self.done = true;
                break;
            }
            let block_want = self
                .plan
                .total
                .remaining_after(produced)
                .min(tutti_types::Samples(tutti_core::MAX_BUFFER_SIZE));
            let mut block = vec![[0.0f32; CH]; block_want.get()];
            let n = self.src.fill(&mut block);
            if n == 0 {
                self.done = true;
                break;
            }
            let cursor = crate::render::BlockCursor {
                block_start: produced,
                latency: self.plan.latency,
                kept_so_far: tutti_types::Samples(self.pending_frames_total),
                output_length: self.plan.output_length,
            };
            let window = cursor.window(tutti_types::Samples(n));
            if !window.is_empty() {
                staging.extend_from_slice(&block[window.clone()]);
                self.dither.apply(&mut staging);
                self.pending_frames_total += staging.len();
                for f in &staging {
                    for &s in f.iter() {
                        self.pending.push(f32_to_i32(s, self.bit_depth));
                    }
                }
            }
            if self.pending.len() == before && self.src.produced() >= self.plan.total {
                self.done = true;
            }
        }
    }
}

impl<const CH: usize> Source for RenderSource<'_, CH> {
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
        self.pull_until(block_size);
        let want = (block_size * self.channels).min(self.pending.len());
        if want == 0 {
            return Ok(0);
        }
        let chunk: Vec<i32> = self.pending.drain(..want).collect();
        dest.fill_interleaved(&chunk)?;
        Ok(want / self.channels)
    }
}

impl<const CH: usize> Encoder<CH> for FlacEncoder {
    fn encode(
        self,
        src: &mut dyn FrameSource<CH>,
        plan: &RenderPlan,
        spec: &ExportSpec,
    ) -> Result<()> {
        let bits = bits_for(self.bit_depth);
        let source = RenderSource::<CH> {
            src,
            plan,
            dither: crate::process::DitherState::for_spec(spec),
            channels: CH,
            bits,
            sample_rate: spec.output_rate() as usize,
            bit_depth: self.bit_depth,
            pending: Vec::new(),
            pending_frames_total: 0,
            done: false,
        };

        // `compression_level` is the app-facing knob; flacenc expresses effort
        // through its own preset, which `Encoder::default()` already sets to a
        // balanced point. Mapping the 0–8 scale onto flacenc's individual
        // coding options is a separate change — the level is accepted and
        // currently unmapped rather than silently reinterpreted.
        let _ = self.compression_level;
        let config = EncoderConfig::default()
            .into_verified()
            .map_err(|e| Error::Encoding(format!("Invalid FLAC config: {e:?}")))?;

        let stream = encode_with_fixed_block_size(&config, source, BLOCK_SIZE)
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

#[inline]
fn f32_to_i32(sample: f32, bit_depth: BitDepth) -> i32 {
    let clamped = sample.clamp(-1.0, 1.0);
    match bit_depth {
        BitDepth::Int16 => (clamped * 32767.0) as i32,
        _ => (clamped * 8388607.0) as i32,
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
}
