//! OGG Vorbis encoder (vorbis_rs-backed). Whole-signal only.

use crate::encode::EncodeRequest;
use crate::error::{Error, Result};
use crate::process::Chunk;
use std::io::BufWriter;
use std::num::{NonZeroU32, NonZeroU8};
use vorbis_rs::{VorbisBitrateManagementStrategy, VorbisEncoderBuilder};

const BLOCK_SIZE: usize = 4096;

pub(crate) fn encode(audio: Chunk, request: &EncodeRequest<'_>) -> Result<()> {
    let quality = request.ogg.quality;

    let channels: Vec<Vec<f32>> = match audio {
        Chunk::Stereo { left, right } => vec![left, right],
        Chunk::Mono(samples) => vec![samples],
    };

    let num_channels = channels.len();
    let num_frames = channels[0].len();

    let file = std::fs::File::create(request.path)?;
    let writer = BufWriter::new(file);

    let sr = NonZeroU32::new(request.sample_rate)
        .ok_or_else(|| Error::InvalidConfig("Sample rate must be non-zero".into()))?;
    let ch = NonZeroU8::new(num_channels as u8)
        .ok_or_else(|| Error::InvalidConfig("Channel count must be non-zero".into()))?;

    let mut encoder = VorbisEncoderBuilder::new(sr, ch, writer)
        .map_err(|e| Error::Encoding(format!("Failed to create OGG encoder: {e}")))?;
    encoder.bitrate_management_strategy(VorbisBitrateManagementStrategy::QualityVbr {
        target_quality: quality,
    });
    let mut encoder = encoder
        .build()
        .map_err(|e| Error::Encoding(format!("Failed to build OGG encoder: {e}")))?;

    let mut offset = 0;
    while offset < num_frames {
        let block_end = (offset + BLOCK_SIZE).min(num_frames);
        let block: Vec<&[f32]> = channels.iter().map(|c| &c[offset..block_end]).collect();
        encoder
            .encode_audio_block(block.as_slice())
            .map_err(|e| Error::Encoding(format!("OGG encoding failed: {e}")))?;
        offset = block_end;
    }

    encoder
        .finish()
        .map_err(|e| Error::Encoding(format!("OGG finalization failed: {e}")))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::{AudioFormat, ChannelMode, Export};

    #[test]
    fn ogg_stereo_sine_produces_valid_ogg() {
        let sample_rate = 44100u32;
        let num_samples = (sample_rate as f64 * 0.1) as usize;

        let left: Vec<f32> = (0..num_samples)
            .map(|i| {
                (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sample_rate as f32).sin() * 0.5
            })
            .collect();
        let right = left.clone();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.ogg");
        Export::buffers(left, right, sample_rate as f64)
            .format(AudioFormat::OggVorbis)
            .to_file(&path)
            .run()
            .unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[0..4], b"OggS");
        assert!(bytes.len() > 100);
    }

    #[test]
    fn ogg_mono_writes_ogg_magic() {
        let num_samples = 4410;
        let left: Vec<f32> = vec![0.0; num_samples];
        let right = left.clone();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_mono.ogg");
        Export::buffers(left, right, 44100.0)
            .format(AudioFormat::OggVorbis)
            .channels(ChannelMode::Mono)
            .to_file(&path)
            .run()
            .unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[0..4], b"OggS");
    }
}
