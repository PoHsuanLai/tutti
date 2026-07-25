//! WAV encoder (hound-backed). Whole-signal and streaming.
//!
//! hound's `WavSpec.channels` is a runtime `u16`, so WAV carries any width
//! natively — the header is simply the plane count and the samples are written
//! interleaved frame-major. The whole-signal path receives per-channel planes
//! from [`rechannel`](crate::encode::rechannel); the streaming path receives
//! already-mapped interleaved blocks and only needs the source channel count.

use crate::encode::sink::StreamingEncoder;
use crate::encode::EncodeRequest;
use crate::error::{Error, Result};
use crate::options::BitDepth;
use hound::{SampleFormat, WavSpec, WavWriter};
use std::io::{BufWriter, Seek, Write};
use std::path::Path;
use tutti_core::pcm::{f32_to_i16, f32_to_i24};
use tutti_types::ChannelLayout;

/// Encode per-channel `planes` (already mapped to `request.channels` by
/// [`rechannel`](crate::encode::rechannel)) to a WAV file. The header carries
/// `planes.len()` channels; samples are written interleaved.
pub(crate) fn encode(planes: &[Vec<f32>], request: &EncodeRequest<'_>) -> Result<()> {
    let channels = planes.len() as u16;
    let spec = spec(request.sample_rate, request.bit_depth, channels);
    let mut writer = WavWriter::create(request.path, spec).map_err(io_err)?;
    write_planes(&mut writer, planes, request.bit_depth)?;
    writer.finalize().map_err(io_err)?;
    Ok(())
}

pub(crate) fn open_stream(
    path: &Path,
    sample_rate: u32,
    bit_depth: BitDepth,
    channels: ChannelLayout,
) -> Result<Box<dyn StreamingEncoder>> {
    let writer =
        WavWriter::create(path, spec(sample_rate, bit_depth, channels.count())).map_err(io_err)?;
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
    fn write_interleaved(&mut self, interleaved: &[f32], channels: u16) -> Result<()> {
        // Frames arrive already at the file width: the render→frame fold
        // (`NetSource`) downmixes/upmixes the graph to `CH` = the file channel
        // count, so samples pass straight through interleaved.
        debug_assert_eq!(channels, self.channels.count());
        let emit = emitter(self.bit_depth);
        for &s in interleaved {
            emit(&mut self.writer, s)?;
        }
        Ok(())
    }

    fn finalize(self: Box<Self>) -> Result<()> {
        self.writer.finalize().map_err(io_err)?;
        Ok(())
    }
}

fn spec(sample_rate: u32, bit_depth: BitDepth, channels: u16) -> WavSpec {
    let (bits_per_sample, sample_format) = match bit_depth {
        BitDepth::Int16 => (16, SampleFormat::Int),
        BitDepth::Int24 => (24, SampleFormat::Int),
        BitDepth::Float32 => (32, SampleFormat::Float),
    };
    WavSpec {
        channels,
        sample_rate,
        bits_per_sample,
        sample_format,
    }
}

/// A closure that quantizes one sample to `bit_depth` and writes it. Shared by
/// the whole-signal and streaming paths.
fn emitter<W: Write + Seek>(bit_depth: BitDepth) -> impl Fn(&mut WavWriter<W>, f32) -> Result<()> {
    move |writer: &mut WavWriter<W>, s: f32| -> Result<()> {
        match bit_depth {
            BitDepth::Int16 => writer.write_sample(f32_to_i16(s)).map_err(io_err),
            BitDepth::Int24 => writer.write_sample(f32_to_i24(s)).map_err(io_err),
            BitDepth::Float32 => writer.write_sample(s).map_err(io_err),
        }
    }
}

/// Write `planes` interleaved (frame-major): sample 0 of every channel, then
/// sample 1 of every channel, and so on.
fn write_planes<W: Write + Seek>(
    writer: &mut WavWriter<W>,
    planes: &[Vec<f32>],
    bit_depth: BitDepth,
) -> Result<()> {
    let emit = emitter(bit_depth);
    let len = planes.iter().map(|p| p.len()).min().unwrap_or(0);
    for i in 0..len {
        for plane in planes {
            emit(writer, plane[i])?;
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

        // Interleaved stereo: 5 frames × 2 channels.
        encoder
            .write_interleaved(&[0.0, 0.1, 0.25, -0.1, 0.5, 0.0], 2)
            .unwrap();
        encoder
            .write_interleaved(&[-0.5, 0.3, 0.75, -0.3], 2)
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

    #[test]
    fn wav_encode_writes_quad_planes() {
        use crate::encode::EncodeRequest;
        use crate::options::{Flac, Ogg};
        use crate::AudioFormat;

        // Four distinct planes → a 4-channel WAV (calls the plane-level encoder
        // directly, as the top-level `encode` would after `rechannel`).
        let planes = vec![
            vec![0.1, 0.2],
            vec![0.3, 0.4],
            vec![0.5, 0.6],
            vec![-0.1, -0.2],
        ];
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quad.wav");
        encode(
            &planes,
            &EncodeRequest {
                path: &path,
                format: AudioFormat::Wav,
                sample_rate: 44100,
                bit_depth: BitDepth::Float32,
                channels: ChannelLayout::Quad,
                flac: Flac::default(),
                ogg: Ogg::default(),
            },
        )
        .unwrap();

        let reader = hound::WavReader::open(&path).unwrap();
        assert_eq!(reader.spec().channels, 4);
        let samples: Vec<f32> = reader.into_samples::<f32>().map(|s| s.unwrap()).collect();
        // 2 frames × 4 channels, interleaved frame-major.
        assert_eq!(samples.len(), 8);
        assert!((samples[0] - 0.1).abs() < 1e-6); // f0 c0
        assert!((samples[1] - 0.3).abs() < 1e-6); // f0 c1
        assert!((samples[3] - (-0.1)).abs() < 1e-6); // f0 c3
        assert!((samples[4] - 0.2).abs() < 1e-6); // f1 c0
    }
}
