//! WAV encoder (hound-backed). Whole-signal and streaming.

use crate::encode::sink::StreamingEncoder;
use crate::encode::EncodeRequest;
use crate::error::{Error, Result};
use crate::options::BitDepth;
use crate::process::fold_frame;
use hound::{SampleFormat, WavSpec, WavWriter};
use std::io::{BufWriter, Seek, Write};
use std::path::Path;
use tutti_core::pcm::{f32_to_i16, f32_to_i24};
use tutti_types::ChannelLayout;

/// The stereo render pipeline emits either 1 channel (a folded mono file) or 2
/// (stereo). A `ChannelLayout` asking for more than stereo is served as stereo —
/// there are only two source channels to write. So both the header and the
/// per-frame write key off "is this a mono request?".
fn is_mono(channels: ChannelLayout) -> bool {
    channels.count() == 1
}

pub(crate) fn encode(frames: &[[f32; 2]], request: &EncodeRequest<'_>) -> Result<()> {
    let spec = spec(request.sample_rate, request.bit_depth, request.channels);
    let mut writer = WavWriter::create(request.path, spec).map_err(io_err)?;
    write_frames(&mut writer, frames, request.bit_depth, request.channels)?;
    writer.finalize().map_err(io_err)?;
    Ok(())
}

pub(crate) fn open_stream(
    path: &Path,
    sample_rate: u32,
    bit_depth: BitDepth,
    channels: ChannelLayout,
) -> Result<Box<dyn StreamingEncoder>> {
    let writer = WavWriter::create(path, spec(sample_rate, bit_depth, channels)).map_err(io_err)?;
    Ok(Box::new(StreamingWavEncoder {
        writer,
        bit_depth,
        channels,
    }))
}

struct StreamingWavEncoder {
    writer: WavWriter<BufWriter<std::fs::File>>,
    bit_depth: BitDepth,
    channels: ChannelLayout,
}

impl StreamingEncoder for StreamingWavEncoder {
    fn write_frames(&mut self, frames: &[[f32; 2]]) -> Result<()> {
        write_frames(&mut self.writer, frames, self.bit_depth, self.channels)
    }

    fn finalize(self: Box<Self>) -> Result<()> {
        self.writer.finalize().map_err(io_err)?;
        Ok(())
    }
}

fn spec(sample_rate: u32, bit_depth: BitDepth, channels: ChannelLayout) -> WavSpec {
    let (bits_per_sample, sample_format) = match bit_depth {
        BitDepth::Int16 => (16, SampleFormat::Int),
        BitDepth::Int24 => (24, SampleFormat::Int),
        BitDepth::Float32 => (32, SampleFormat::Float),
    };
    WavSpec {
        // 1 for a mono fold, else 2 — the pipeline only has two source channels.
        channels: if is_mono(channels) { 1 } else { 2 },
        sample_rate,
        bits_per_sample,
        sample_format,
    }
}

fn write_frames<W: Write + Seek>(
    writer: &mut WavWriter<W>,
    frames: &[[f32; 2]],
    bit_depth: BitDepth,
    channels: ChannelLayout,
) -> Result<()> {
    // One closure quantizes a sample to the target bit depth; the channel loop
    // decides how many samples per frame (folding to mono when asked).
    let emit = |writer: &mut WavWriter<W>, s: f32| -> Result<()> {
        match bit_depth {
            BitDepth::Int16 => writer.write_sample(f32_to_i16(s)).map_err(io_err),
            BitDepth::Int24 => writer.write_sample(f32_to_i24(s)).map_err(io_err),
            BitDepth::Float32 => writer.write_sample(s).map_err(io_err),
        }
    };
    if is_mono(channels) {
        for &frame in frames {
            emit(writer, fold_frame(frame))?;
        }
    } else {
        for &[l, r] in frames {
            emit(writer, l)?;
            emit(writer, r)?;
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
    fn export_buffers_mono_folds_and_averages_channels() {
        use crate::{ChannelLayout, Export};

        // Distinct L/R so the mono fold (average) is observable.
        let left = vec![1.0, 0.0, -1.0];
        let right = vec![0.0, 0.0, 1.0];

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mono.wav");
        Export::buffers(left, right, 44100.0)
            .bit_depth(BitDepth::Float32)
            .channels(ChannelLayout::Mono)
            .to_file(&path)
            .run()
            .unwrap();

        let reader = hound::WavReader::open(&path).unwrap();
        assert_eq!(reader.spec().channels, 1, "mono file has one channel");
        let samples: Vec<f32> = reader.into_samples::<f32>().map(|s| s.unwrap()).collect();
        assert_eq!(samples.len(), 3, "one sample per frame");
        // (1+0)/2, (0+0)/2, (-1+1)/2
        assert!((samples[0] - 0.5).abs() < 1e-6);
        assert!(samples[1].abs() < 1e-6);
        assert!(samples[2].abs() < 1e-6);
    }

    #[test]
    fn streaming_wav_encoder_writes_expected_frames() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("streaming.wav");

        let mut encoder =
            open_stream(&path, 44100, BitDepth::Int16, ChannelLayout::Stereo).unwrap();

        encoder
            .write_frames(&[[0.0, 0.1], [0.25, -0.1], [0.5, 0.0]])
            .unwrap();
        encoder.write_frames(&[[-0.5, 0.3], [0.75, -0.3]]).unwrap();
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
