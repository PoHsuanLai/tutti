//! WAV encoder (hound-backed). Whole-signal and streaming.

use crate::encode::sink::StreamingEncoder;
use crate::encode::EncodeRequest;
use crate::error::{Error, Result};
use crate::options::{BitDepth, ChannelMode};
use crate::process::Chunk;
use hound::{SampleFormat, WavSpec, WavWriter};
use std::io::{BufWriter, Seek, Write};
use std::path::Path;
use tutti_core::pcm::{f32_to_i16, f32_to_i24};

pub(crate) fn encode(audio: Chunk, request: &EncodeRequest<'_>) -> Result<()> {
    match audio {
        Chunk::Stereo { left, right } => {
            let spec = spec(request.sample_rate, request.bit_depth, ChannelMode::Stereo);
            let mut writer = WavWriter::create(request.path, spec).map_err(io_err)?;
            write_stereo(&mut writer, &left, &right, request.bit_depth)?;
            writer.finalize().map_err(io_err)?;
        }
        Chunk::Mono(samples) => {
            let spec = spec(request.sample_rate, request.bit_depth, ChannelMode::Mono);
            let mut writer = WavWriter::create(request.path, spec).map_err(io_err)?;
            write_mono(&mut writer, &samples, request.bit_depth)?;
            writer.finalize().map_err(io_err)?;
        }
    }
    Ok(())
}

pub(crate) fn open_stream(
    path: &Path,
    sample_rate: u32,
    bit_depth: BitDepth,
    channels: ChannelMode,
) -> Result<Box<dyn StreamingEncoder>> {
    let writer = WavWriter::create(path, spec(sample_rate, bit_depth, channels)).map_err(io_err)?;
    Ok(Box::new(StreamingWavEncoder { writer, bit_depth }))
}

struct StreamingWavEncoder {
    writer: WavWriter<BufWriter<std::fs::File>>,
    bit_depth: BitDepth,
}

impl StreamingEncoder for StreamingWavEncoder {
    fn write_chunk(&mut self, chunk: Chunk) -> Result<()> {
        match chunk {
            Chunk::Stereo { left, right } => {
                write_stereo(&mut self.writer, &left, &right, self.bit_depth)
            }
            Chunk::Mono(samples) => write_mono(&mut self.writer, &samples, self.bit_depth),
        }
    }

    fn finalize(self: Box<Self>) -> Result<()> {
        self.writer.finalize().map_err(io_err)?;
        Ok(())
    }
}

fn spec(sample_rate: u32, bit_depth: BitDepth, channels: ChannelMode) -> WavSpec {
    let (bits_per_sample, sample_format) = match bit_depth {
        BitDepth::Int16 => (16, SampleFormat::Int),
        BitDepth::Int24 => (24, SampleFormat::Int),
        BitDepth::Float32 => (32, SampleFormat::Float),
    };
    WavSpec {
        channels: channels.count(),
        sample_rate,
        bits_per_sample,
        sample_format,
    }
}

fn write_stereo<W: Write + Seek>(
    writer: &mut WavWriter<W>,
    left: &[f32],
    right: &[f32],
    bit_depth: BitDepth,
) -> Result<()> {
    if left.len() != right.len() {
        return Err(Error::InvalidData(
            "Left and right channels have different lengths".into(),
        ));
    }
    match bit_depth {
        BitDepth::Int16 => {
            for (&l, &r) in left.iter().zip(right) {
                writer.write_sample(f32_to_i16(l)).map_err(io_err)?;
                writer.write_sample(f32_to_i16(r)).map_err(io_err)?;
            }
        }
        BitDepth::Int24 => {
            for (&l, &r) in left.iter().zip(right) {
                writer.write_sample(f32_to_i24(l)).map_err(io_err)?;
                writer.write_sample(f32_to_i24(r)).map_err(io_err)?;
            }
        }
        BitDepth::Float32 => {
            for (&l, &r) in left.iter().zip(right) {
                writer.write_sample(l).map_err(io_err)?;
                writer.write_sample(r).map_err(io_err)?;
            }
        }
    }
    Ok(())
}

fn write_mono<W: Write + Seek>(
    writer: &mut WavWriter<W>,
    samples: &[f32],
    bit_depth: BitDepth,
) -> Result<()> {
    match bit_depth {
        BitDepth::Int16 => {
            for &s in samples {
                writer.write_sample(f32_to_i16(s)).map_err(io_err)?;
            }
        }
        BitDepth::Int24 => {
            for &s in samples {
                writer.write_sample(f32_to_i24(s)).map_err(io_err)?;
            }
        }
        BitDepth::Float32 => {
            for &s in samples {
                writer.write_sample(s).map_err(io_err)?;
            }
        }
    }
    Ok(())
}

fn io_err(e: hound::Error) -> Error {
    Error::Io(std::io::Error::other(e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_buffers_writes_valid_wav() {
        use crate::Export;

        let left = vec![0.0, 0.5, -0.5];
        let right = vec![0.1, -0.1, 0.0];

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.wav");
        Export::buffers(left, right, 44100.0)
            .to_file(&path)
            .run()
            .unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
    }

    #[test]
    fn streaming_wav_encoder_writes_expected_frames() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("streaming.wav");

        let mut encoder = open_stream(&path, 44100, BitDepth::Int16, ChannelMode::Stereo).unwrap();

        encoder
            .write_chunk(Chunk::Stereo {
                left: vec![0.0, 0.25, 0.5],
                right: vec![0.1, -0.1, 0.0],
            })
            .unwrap();
        encoder
            .write_chunk(Chunk::Stereo {
                left: vec![-0.5, 0.75],
                right: vec![0.3, -0.3],
            })
            .unwrap();
        encoder.finalize().unwrap();

        let reader = hound::WavReader::open(&path).unwrap();
        let spec = reader.spec();
        assert_eq!(spec.channels, 2);
        assert_eq!(spec.sample_rate, 44100);
        assert_eq!(spec.bits_per_sample, 16);

        let samples: Vec<i16> = reader.into_samples::<i16>().map(|s| s.unwrap()).collect();
        assert_eq!(samples.len(), 10); // 5 frames × 2 channels
    }
}
