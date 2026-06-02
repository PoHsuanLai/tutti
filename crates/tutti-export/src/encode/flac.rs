//! FLAC encoder (flacenc-backed). Whole-signal only; streaming slot is
//! reserved but currently rejected at the opener.

use crate::encode::EncodeRequest;
use crate::error::{Error, Result};
use crate::options::BitDepth;
use crate::process::ProcessedAudio;
use flacenc::bitsink::ByteSink;
use flacenc::component::BitRepr;
use flacenc::config::Encoder as EncoderConfig;
use flacenc::encode_with_fixed_block_size;
use flacenc::error::Verify;
use flacenc::source::MemSource;
use std::fs::File;
use std::io::Write;

const BLOCK_SIZE: usize = 4096;

pub(crate) fn encode(audio: ProcessedAudio, request: &EncodeRequest<'_>) -> Result<()> {
    if request.bit_depth == BitDepth::Float32 {
        return Err(Error::UnsupportedFormat(
            "FLAC does not support 32-bit float".into(),
        ));
    }
    let bits_per_sample = bits_for(request.bit_depth);

    let (samples, channels) = match audio {
        ProcessedAudio::Stereo { left, right } => (interleave(&left, &right, request.bit_depth), 2),
        ProcessedAudio::Mono(samples) => (
            samples
                .iter()
                .map(|&s| f32_to_i32(s, request.bit_depth))
                .collect(),
            1,
        ),
    };

    let encoder_config = EncoderConfig::default()
        .into_verified()
        .map_err(|e| Error::Encoding(format!("Invalid FLAC config: {:?}", e)))?;

    let source = MemSource::from_samples(
        &samples,
        channels,
        bits_per_sample,
        request.sample_rate as usize,
    );

    let stream = encode_with_fixed_block_size(&encoder_config, source, BLOCK_SIZE)
        .map_err(|e| Error::Encoding(format!("FLAC encoding failed: {:?}", e)))?;

    let mut sink = ByteSink::new();
    stream
        .write(&mut sink)
        .map_err(|e| Error::Encoding(format!("Failed to write FLAC stream: {:?}", e)))?;

    let mut file = File::create(request.path)?;
    file.write_all(&sink.into_inner())?;
    Ok(())
}

fn bits_for(bit_depth: BitDepth) -> usize {
    match bit_depth {
        BitDepth::Int16 => 16,
        BitDepth::Int24 => 24,
        BitDepth::Float32 => unreachable!(),
    }
}

fn interleave(left: &[f32], right: &[f32], bit_depth: BitDepth) -> Vec<i32> {
    let mut out = Vec::with_capacity(left.len() * 2);
    for (&l, &r) in left.iter().zip(right) {
        out.push(f32_to_i32(l, bit_depth));
        out.push(f32_to_i32(r, bit_depth));
    }
    out
}

#[inline]
fn f32_to_i32(sample: f32, bit_depth: BitDepth) -> i32 {
    let clamped = sample.clamp(-1.0, 1.0);
    match bit_depth {
        BitDepth::Int16 => (clamped * 32767.0) as i32,
        BitDepth::Int24 => (clamped * 8388607.0) as i32,
        BitDepth::Float32 => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interleave_preserves_sample_values() {
        let left = vec![0.0, 1.0];
        let right = vec![0.5, -0.5];
        let out = interleave(&left, &right, BitDepth::Int16);

        assert_eq!(out.len(), 4);
        assert_eq!(out[0], 0);
        assert_eq!(out[1], 16383);
        assert_eq!(out[2], 32767);
        assert_eq!(out[3], -16383);
    }

    #[test]
    fn encode_rejects_float32() {
        use crate::{AudioFormat, Export};

        let left = vec![0.0; 100];
        let right = vec![0.0; 100];

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.flac");
        let result = Export::buffers(left, right, 44100.0)
            .format(AudioFormat::Flac)
            .bit_depth(BitDepth::Float32)
            .to_file(&path)
            .run();
        assert!(result.is_err());
    }
}
