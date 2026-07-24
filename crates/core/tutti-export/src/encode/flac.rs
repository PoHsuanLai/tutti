//! FLAC encoder (flacenc-backed). Whole-signal only; streaming slot is
//! reserved but currently rejected at the opener.

use crate::encode::EncodeRequest;
use crate::error::{Error, Result};
use crate::options::BitDepth;
use crate::process::fold_frame;
use flacenc::bitsink::ByteSink;
use flacenc::component::BitRepr;
use flacenc::config::Encoder as EncoderConfig;
use flacenc::encode_with_fixed_block_size;
use flacenc::error::Verify;
use flacenc::source::MemSource;
use std::fs::File;
use std::io::Write;

const BLOCK_SIZE: usize = 4096;

pub(crate) fn encode(frames: &[[f32; 2]], request: &EncodeRequest<'_>) -> Result<()> {
    if request.bit_depth == BitDepth::Float32 {
        return Err(Error::UnsupportedFormat(
            "FLAC does not support 32-bit float".into(),
        ));
    }
    let bits_per_sample = bits_for(request.bit_depth);

    // A layout wider than stereo is written as stereo — the pipeline has only
    // two source channels.
    let (samples, channels): (Vec<i32>, usize) = if request.channels.count() == 1 {
        (
            frames
                .iter()
                .map(|&f| f32_to_i32(fold_frame(f), request.bit_depth))
                .collect(),
            1,
        )
    } else {
        let mut out = Vec::with_capacity(frames.len() * 2);
        for &[l, r] in frames {
            out.push(f32_to_i32(l, request.bit_depth));
            out.push(f32_to_i32(r, request.bit_depth));
        }
        (out, 2)
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
    fn f32_to_i32_scales_to_bit_depth() {
        assert_eq!(f32_to_i32(0.0, BitDepth::Int16), 0);
        assert_eq!(f32_to_i32(0.5, BitDepth::Int16), 16383);
        assert_eq!(f32_to_i32(1.0, BitDepth::Int16), 32767);
        assert_eq!(f32_to_i32(-0.5, BitDepth::Int16), -16383);
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
