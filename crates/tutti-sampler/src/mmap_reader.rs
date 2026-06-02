use memmap2::Mmap;
use std::fs::File;
use std::path::Path;

use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Int16,
    Int24,
    Float32,
}

impl Format {
    fn bytes_per_sample(self) -> usize {
        match self {
            Self::Int16 => 2,
            Self::Int24 => 3,
            Self::Float32 => 4,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Container {
    Wav,
    Aiff,
}

#[derive(Debug, Clone)]
pub struct Info {
    pub sample_rate: u32,
    pub channels: u16,
    pub total_frames: u64,
    pub sample_format: Format,
}

pub struct MmapReader {
    mmap: Mmap,
    info: Info,
    data_offset: usize,
    file_format: Container,
}

impl MmapReader {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let file = File::open(path)?;
        let mmap = unsafe { Mmap::map(&file)? };

        if mmap.len() < 12 {
            return Err(Error::MmapReader("file too small".into()));
        }

        let magic = &mmap[0..4];
        if magic == b"RIFF" {
            Self::parse_wav(mmap)
        } else if magic == b"FORM" {
            Self::parse_aiff(mmap)
        } else {
            Err(Error::MmapReader(
                "unsupported format: expected RIFF or FORM".into(),
            ))
        }
    }

    fn parse_wav(mmap: Mmap) -> Result<Self> {
        let data = &mmap[..];
        if data.len() < 12 || &data[8..12] != b"WAVE" {
            return Err(Error::MmapReader(
                "invalid WAV: missing WAVE identifier".into(),
            ));
        }

        let mut pos = 12;
        let mut fmt_found = false;
        let mut channels: u16 = 0;
        let mut sample_rate: u32 = 0;
        let mut bits_per_sample: u16 = 0;
        let mut audio_format: u16 = 0;
        let mut data_offset: usize = 0;
        let mut data_size: u32 = 0;
        let mut data_found = false;

        while pos + 8 <= data.len() {
            let chunk_id = &data[pos..pos + 4];
            let chunk_size =
                u32::from_le_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]]);

            if chunk_id == b"fmt " {
                if pos + 8 + 16 > data.len() {
                    return Err(Error::MmapReader("truncated fmt chunk".into()));
                }
                let base = pos + 8;
                audio_format = u16::from_le_bytes([data[base], data[base + 1]]);
                channels = u16::from_le_bytes([data[base + 2], data[base + 3]]);
                sample_rate = u32::from_le_bytes([
                    data[base + 4],
                    data[base + 5],
                    data[base + 6],
                    data[base + 7],
                ]);
                bits_per_sample = u16::from_le_bytes([data[base + 14], data[base + 15]]);
                fmt_found = true;
            } else if chunk_id == b"data" {
                data_offset = pos + 8;
                data_size = chunk_size;
                data_found = true;
                break;
            }

            // Chunks are word-aligned
            let advance = 8 + ((chunk_size as usize + 1) & !1);
            pos += advance;
        }

        if !fmt_found {
            return Err(Error::MmapReader("WAV: fmt chunk not found".into()));
        }
        if !data_found {
            return Err(Error::MmapReader("WAV: data chunk not found".into()));
        }

        let sample_format = match (audio_format, bits_per_sample) {
            (1, 16) => Format::Int16,
            (1, 24) => Format::Int24,
            (3, 32) => Format::Float32,
            _ => {
                return Err(Error::MmapReader(format!(
                    "unsupported WAV format: audio_format={audio_format}, bits={bits_per_sample}"
                )));
            }
        };

        let frame_size = usize::from(channels) * sample_format.bytes_per_sample();
        let total_frames = if frame_size > 0 {
            data_size as u64 / frame_size as u64
        } else {
            0
        };

        Ok(Self {
            mmap,
            info: Info {
                sample_rate,
                channels,
                total_frames,
                sample_format,
            },
            data_offset,
            file_format: Container::Wav,
        })
    }

    fn parse_aiff(mmap: Mmap) -> Result<Self> {
        let data = &mmap[..];
        if data.len() < 12 {
            return Err(Error::MmapReader("truncated AIFF".into()));
        }

        let form_type = &data[8..12];
        let is_aifc = form_type == b"AIFC";
        if form_type != b"AIFF" && !is_aifc {
            return Err(Error::MmapReader(
                "invalid AIFF: missing AIFF/AIFC identifier".into(),
            ));
        }

        let mut pos = 12;
        let mut comm_found = false;
        let mut channels: u16 = 0;
        let mut total_frames: u64 = 0;
        let mut bits_per_sample: u16 = 0;
        let mut sample_rate: u32 = 0;
        let mut data_offset: usize = 0;
        let mut data_found = false;

        while pos + 8 <= data.len() {
            let chunk_id = &data[pos..pos + 4];
            let chunk_size =
                u32::from_be_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]]);

            if chunk_id == b"COMM" {
                if pos + 8 + 18 > data.len() {
                    return Err(Error::MmapReader("truncated COMM chunk".into()));
                }
                let base = pos + 8;
                channels = u16::from_be_bytes([data[base], data[base + 1]]);
                total_frames = u32::from_be_bytes([
                    data[base + 2],
                    data[base + 3],
                    data[base + 4],
                    data[base + 5],
                ]) as u64;
                bits_per_sample = u16::from_be_bytes([data[base + 6], data[base + 7]]);
                sample_rate = ieee_extended_to_u32(&data[base + 8..base + 18]);
                comm_found = true;
            } else if chunk_id == b"SSND" {
                if pos + 8 + 8 > data.len() {
                    return Err(Error::MmapReader("truncated SSND chunk".into()));
                }
                let base = pos + 8;
                let offset_field = u32::from_be_bytes([
                    data[base],
                    data[base + 1],
                    data[base + 2],
                    data[base + 3],
                ]);
                // SSND has 8 bytes of offset+blockSize before audio data
                data_offset = base + 8 + offset_field as usize;
                data_found = true;
                if comm_found {
                    break;
                }
            }

            let advance = 8 + ((chunk_size as usize + 1) & !1);
            pos += advance;
        }

        if !comm_found {
            return Err(Error::MmapReader("AIFF: COMM chunk not found".into()));
        }
        if !data_found {
            return Err(Error::MmapReader("AIFF: SSND chunk not found".into()));
        }

        let sample_format = match bits_per_sample {
            16 => Format::Int16,
            24 => Format::Int24,
            32 => Format::Float32,
            _ => {
                return Err(Error::MmapReader(format!(
                    "unsupported AIFF bit depth: {bits_per_sample}"
                )));
            }
        };

        Ok(Self {
            mmap,
            info: Info {
                sample_rate,
                channels,
                total_frames,
                sample_format,
            },
            data_offset,
            file_format: Container::Aiff,
        })
    }

    pub fn info(&self) -> &Info {
        &self.info
    }

    /// Read interleaved frames starting at `start_frame` into `output`.
    /// `output` length must be a multiple of `channels`.
    /// Returns the number of frames actually read.
    pub fn read_frames(&self, start_frame: u64, output: &mut [f32]) -> usize {
        let channels = usize::from(self.info.channels);
        if channels == 0 || output.is_empty() {
            return 0;
        }

        let requested_frames = output.len() / channels;
        if start_frame >= self.info.total_frames {
            output.fill(0.0);
            return 0;
        }

        let available = (self.info.total_frames - start_frame) as usize;
        let frames_to_read = requested_frames.min(available);
        let bps = self.info.sample_format.bytes_per_sample();
        let frame_size = channels * bps;
        let byte_offset = self.data_offset + start_frame as usize * frame_size;
        let byte_end = byte_offset + frames_to_read * frame_size;

        let data = &self.mmap[..];
        if byte_end > data.len() {
            output.fill(0.0);
            return 0;
        }

        let raw = &data[byte_offset..byte_end];
        let total_samples = frames_to_read * channels;

        match (self.file_format, self.info.sample_format) {
            (Container::Wav, Format::Int16) => {
                for (i, out) in output[..total_samples].iter_mut().enumerate() {
                    let off = i * 2;
                    let val = i16::from_le_bytes([raw[off], raw[off + 1]]);
                    *out = val as f32 / 32768.0;
                }
            }
            (Container::Wav, Format::Int24) => {
                for (i, out) in output[..total_samples].iter_mut().enumerate() {
                    let off = i * 3;
                    let val = ((raw[off] as i32)
                        | ((raw[off + 1] as i32) << 8)
                        | ((raw[off + 2] as i32) << 16))
                        // Sign extend from 24-bit
                        << 8
                        >> 8;
                    *out = val as f32 / 8388608.0;
                }
            }
            (Container::Wav, Format::Float32) => {
                for (i, out) in output[..total_samples].iter_mut().enumerate() {
                    let off = i * 4;
                    *out = f32::from_le_bytes([raw[off], raw[off + 1], raw[off + 2], raw[off + 3]]);
                }
            }
            (Container::Aiff, Format::Int16) => {
                for (i, out) in output[..total_samples].iter_mut().enumerate() {
                    let off = i * 2;
                    let val = i16::from_be_bytes([raw[off], raw[off + 1]]);
                    *out = val as f32 / 32768.0;
                }
            }
            (Container::Aiff, Format::Int24) => {
                for (i, out) in output[..total_samples].iter_mut().enumerate() {
                    let off = i * 3;
                    let val = ((raw[off] as i32) << 16
                        | (raw[off + 1] as i32) << 8
                        | (raw[off + 2] as i32))
                        << 8
                        >> 8;
                    *out = val as f32 / 8388608.0;
                }
            }
            (Container::Aiff, Format::Float32) => {
                for (i, out) in output[..total_samples].iter_mut().enumerate() {
                    let off = i * 4;
                    *out = f32::from_be_bytes([raw[off], raw[off + 1], raw[off + 2], raw[off + 3]]);
                }
            }
        }

        let written = frames_to_read * channels;
        output[written..].fill(0.0);

        frames_to_read
    }

    /// Read samples for a single channel.
    /// Returns the number of frames actually read.
    pub fn read_channel(&self, channel: u16, start_frame: u64, output: &mut [f32]) -> usize {
        let channels = usize::from(self.info.channels);
        if usize::from(channel) >= channels || output.is_empty() {
            return 0;
        }

        let num_frames = output.len();
        let mut interleaved = vec![0.0f32; num_frames * channels];
        let frames_read = self.read_frames(start_frame, &mut interleaved);

        for i in 0..num_frames {
            if i < frames_read {
                output[i] = interleaved[i * channels + usize::from(channel)];
            } else {
                output[i] = 0.0;
            }
        }

        frames_read
    }
}

/// Convert an 80-bit IEEE 754 extended precision float to u32.
/// Used for AIFF sample rate field.
fn ieee_extended_to_u32(bytes: &[u8]) -> u32 {
    let sign = (bytes[0] >> 7) & 1;
    let exponent = ((u16::from(bytes[0]) & 0x7F) << 8) | u16::from(bytes[1]);
    let mantissa = u64::from_be_bytes([
        bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7], bytes[8], bytes[9],
    ]);

    if exponent == 0 && mantissa == 0 {
        return 0;
    }

    // Bias for extended precision is 16383
    let exp = exponent as i32 - 16383;
    // The mantissa has an explicit integer bit (bit 63)
    let f = mantissa as f64 / (1u64 << 63) as f64 * 2.0f64.powi(exp);

    if sign == 1 {
        0 // Negative sample rates don't make sense
    } else {
        f as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_wav_16bit(samples: &[i16], channels: u16, sample_rate: u32) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        let data_size = samples.len() as u32 * 2;
        let file_size = 36 + data_size;
        let byte_rate = sample_rate * channels as u32 * 2;
        let block_align = channels * 2;

        f.write_all(b"RIFF").unwrap();
        f.write_all(&file_size.to_le_bytes()).unwrap();
        f.write_all(b"WAVE").unwrap();

        // fmt chunk
        f.write_all(b"fmt ").unwrap();
        f.write_all(&16u32.to_le_bytes()).unwrap();
        f.write_all(&1u16.to_le_bytes()).unwrap(); // PCM
        f.write_all(&channels.to_le_bytes()).unwrap();
        f.write_all(&sample_rate.to_le_bytes()).unwrap();
        f.write_all(&byte_rate.to_le_bytes()).unwrap();
        f.write_all(&block_align.to_le_bytes()).unwrap();
        f.write_all(&16u16.to_le_bytes()).unwrap(); // bits per sample

        // data chunk
        f.write_all(b"data").unwrap();
        f.write_all(&data_size.to_le_bytes()).unwrap();
        for &s in samples {
            f.write_all(&s.to_le_bytes()).unwrap();
        }

        f.flush().unwrap();
        f
    }

    fn write_wav_f32(samples: &[f32], channels: u16, sample_rate: u32) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        let data_size = samples.len() as u32 * 4;
        let file_size = 36 + data_size;
        let byte_rate = sample_rate * channels as u32 * 4;
        let block_align = channels * 4;

        f.write_all(b"RIFF").unwrap();
        f.write_all(&file_size.to_le_bytes()).unwrap();
        f.write_all(b"WAVE").unwrap();

        f.write_all(b"fmt ").unwrap();
        f.write_all(&16u32.to_le_bytes()).unwrap();
        f.write_all(&3u16.to_le_bytes()).unwrap(); // IEEE float
        f.write_all(&channels.to_le_bytes()).unwrap();
        f.write_all(&sample_rate.to_le_bytes()).unwrap();
        f.write_all(&byte_rate.to_le_bytes()).unwrap();
        f.write_all(&block_align.to_le_bytes()).unwrap();
        f.write_all(&32u16.to_le_bytes()).unwrap();

        f.write_all(b"data").unwrap();
        f.write_all(&data_size.to_le_bytes()).unwrap();
        for &s in samples {
            f.write_all(&s.to_le_bytes()).unwrap();
        }

        f.flush().unwrap();
        f
    }

    fn write_wav_24bit(samples: &[i32], channels: u16, sample_rate: u32) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        let data_size = samples.len() as u32 * 3;
        let file_size = 36 + data_size;
        let byte_rate = sample_rate * channels as u32 * 3;
        let block_align = channels * 3;

        f.write_all(b"RIFF").unwrap();
        f.write_all(&file_size.to_le_bytes()).unwrap();
        f.write_all(b"WAVE").unwrap();

        f.write_all(b"fmt ").unwrap();
        f.write_all(&16u32.to_le_bytes()).unwrap();
        f.write_all(&1u16.to_le_bytes()).unwrap(); // PCM
        f.write_all(&channels.to_le_bytes()).unwrap();
        f.write_all(&sample_rate.to_le_bytes()).unwrap();
        f.write_all(&byte_rate.to_le_bytes()).unwrap();
        f.write_all(&block_align.to_le_bytes()).unwrap();
        f.write_all(&24u16.to_le_bytes()).unwrap();

        f.write_all(b"data").unwrap();
        f.write_all(&data_size.to_le_bytes()).unwrap();
        for &s in samples {
            let bytes = s.to_le_bytes();
            f.write_all(&bytes[0..3]).unwrap();
        }

        f.flush().unwrap();
        f
    }

    fn u32_to_ieee_extended(val: u32) -> [u8; 10] {
        let f = val as f64;
        if f == 0.0 {
            return [0; 10];
        }
        let exp = f.log2().floor() as i32;
        let mantissa = f / 2.0f64.powi(exp);
        let biased_exp = (exp + 16383) as u16;
        let m = (mantissa * (1u64 << 63) as f64) as u64;

        let mut buf = [0u8; 10];
        buf[0] = (biased_exp >> 8) as u8;
        buf[1] = biased_exp as u8;
        let m_bytes = m.to_be_bytes();
        buf[2..10].copy_from_slice(&m_bytes);
        buf
    }

    fn write_aiff_16bit(samples: &[i16], channels: u16, sample_rate: u32) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        let num_frames = samples.len() / channels as usize;
        let data_size = samples.len() as u32 * 2;
        let ssnd_size = data_size + 8; // offset + blockSize fields
        let sr_bytes = u32_to_ieee_extended(sample_rate);

        // COMM chunk: 8 + 18 = 26 bytes
        // SSND chunk: 8 + ssnd_size
        let form_size = 4 + 26 + 8 + ssnd_size;

        f.write_all(b"FORM").unwrap();
        f.write_all(&form_size.to_be_bytes()).unwrap();
        f.write_all(b"AIFF").unwrap();

        // COMM chunk
        f.write_all(b"COMM").unwrap();
        f.write_all(&18u32.to_be_bytes()).unwrap();
        f.write_all(&channels.to_be_bytes()).unwrap();
        f.write_all(&(num_frames as u32).to_be_bytes()).unwrap();
        f.write_all(&16u16.to_be_bytes()).unwrap(); // bits per sample
        f.write_all(&sr_bytes).unwrap();

        // SSND chunk
        f.write_all(b"SSND").unwrap();
        f.write_all(&ssnd_size.to_be_bytes()).unwrap();
        f.write_all(&0u32.to_be_bytes()).unwrap(); // offset
        f.write_all(&0u32.to_be_bytes()).unwrap(); // blockSize
        for &s in samples {
            f.write_all(&s.to_be_bytes()).unwrap();
        }

        f.flush().unwrap();
        f
    }

    #[test]
    fn wav_16bit_mono() {
        let samples: Vec<i16> = vec![0, 16384, 32767, -32768, -16384, 0];
        let f = write_wav_16bit(&samples, 1, 44100);
        let reader = MmapReader::open(f.path()).unwrap();

        let info = reader.info();
        assert_eq!(info.sample_rate, 44100);
        assert_eq!(info.channels, 1);
        assert_eq!(info.total_frames, 6);
        assert_eq!(info.sample_format, Format::Int16);

        let mut buf = [0.0f32; 6];
        let n = reader.read_frames(0, &mut buf);
        assert_eq!(n, 6);
        assert!((buf[0] - 0.0).abs() < 1e-4);
        assert!((buf[1] - 0.5).abs() < 1e-4);
        assert!((buf[2] - (32767.0 / 32768.0)).abs() < 1e-4);
        assert!((buf[3] - (-1.0)).abs() < 1e-4);
    }

    #[test]
    fn wav_16bit_stereo_random_access() {
        // Interleaved stereo: [L0, R0, L1, R1, L2, R2]
        let samples: Vec<i16> = vec![1000, -1000, 2000, -2000, 3000, -3000];
        let f = write_wav_16bit(&samples, 2, 48000);
        let reader = MmapReader::open(f.path()).unwrap();

        assert_eq!(reader.info().total_frames, 3);
        assert_eq!(reader.info().channels, 2);

        // Read from frame 1 (skipping first frame)
        let mut buf = [0.0f32; 4]; // 2 frames * 2 channels
        let n = reader.read_frames(1, &mut buf);
        assert_eq!(n, 2);
        assert!((buf[0] - 2000.0 / 32768.0).abs() < 1e-4);
        assert!((buf[1] - (-2000.0 / 32768.0)).abs() < 1e-4);
        assert!((buf[2] - 3000.0 / 32768.0).abs() < 1e-4);
        assert!((buf[3] - (-3000.0 / 32768.0)).abs() < 1e-4);
    }

    #[test]
    fn wav_f32_mono() {
        let samples: Vec<f32> = vec![0.0, 0.5, 1.0, -1.0, -0.5, 0.25];
        let f = write_wav_f32(&samples, 1, 44100);
        let reader = MmapReader::open(f.path()).unwrap();

        assert_eq!(reader.info().sample_format, Format::Float32);
        assert_eq!(reader.info().total_frames, 6);

        let mut buf = [0.0f32; 6];
        reader.read_frames(0, &mut buf);
        for (a, b) in buf.iter().zip(samples.iter()) {
            assert!((a - b).abs() < 1e-7);
        }
    }

    #[test]
    fn wav_24bit_mono() {
        let samples: Vec<i32> = vec![0, 4194304, 8388607, -8388608];
        let f = write_wav_24bit(&samples, 1, 44100);
        let reader = MmapReader::open(f.path()).unwrap();

        assert_eq!(reader.info().sample_format, Format::Int24);
        assert_eq!(reader.info().total_frames, 4);

        let mut buf = [0.0f32; 4];
        reader.read_frames(0, &mut buf);
        assert!((buf[0] - 0.0).abs() < 1e-5);
        assert!((buf[1] - 0.5).abs() < 1e-5);
        assert!((buf[2] - (8388607.0 / 8388608.0)).abs() < 1e-5);
        assert!((buf[3] - (-1.0)).abs() < 1e-5);
    }

    #[test]
    fn aiff_16bit_mono() {
        let samples: Vec<i16> = vec![0, 16384, 32767, -32768];
        let f = write_aiff_16bit(&samples, 1, 44100);
        let reader = MmapReader::open(f.path()).unwrap();

        let info = reader.info();
        assert_eq!(info.sample_rate, 44100);
        assert_eq!(info.channels, 1);
        assert_eq!(info.total_frames, 4);
        assert_eq!(info.sample_format, Format::Int16);

        let mut buf = [0.0f32; 4];
        let n = reader.read_frames(0, &mut buf);
        assert_eq!(n, 4);
        assert!((buf[0] - 0.0).abs() < 1e-4);
        assert!((buf[1] - 0.5).abs() < 1e-4);
        assert!((buf[2] - (32767.0 / 32768.0)).abs() < 1e-4);
        assert!((buf[3] - (-1.0)).abs() < 1e-4);
    }

    #[test]
    fn read_channel_extracts_single_channel() {
        let samples: Vec<i16> = vec![100, 200, 300, 400, 500, 600];
        let f = write_wav_16bit(&samples, 2, 44100);
        let reader = MmapReader::open(f.path()).unwrap();

        let mut left = [0.0f32; 3];
        let mut right = [0.0f32; 3];
        reader.read_channel(0, 0, &mut left);
        reader.read_channel(1, 0, &mut right);

        assert!((left[0] - 100.0 / 32768.0).abs() < 1e-4);
        assert!((left[1] - 300.0 / 32768.0).abs() < 1e-4);
        assert!((left[2] - 500.0 / 32768.0).abs() < 1e-4);

        assert!((right[0] - 200.0 / 32768.0).abs() < 1e-4);
        assert!((right[1] - 400.0 / 32768.0).abs() < 1e-4);
        assert!((right[2] - 600.0 / 32768.0).abs() < 1e-4);
    }

    #[test]
    fn read_past_end_returns_zero() {
        let samples: Vec<i16> = vec![1000, 2000];
        let f = write_wav_16bit(&samples, 1, 44100);
        let reader = MmapReader::open(f.path()).unwrap();

        let mut buf = [99.0f32; 4];
        let n = reader.read_frames(1, &mut buf);
        assert_eq!(n, 1);
        assert!((buf[0] - 2000.0 / 32768.0).abs() < 1e-4);
        assert_eq!(buf[1], 0.0);
        assert_eq!(buf[2], 0.0);
        assert_eq!(buf[3], 0.0);
    }

    #[test]
    fn read_at_end_returns_zero_frames() {
        let samples: Vec<i16> = vec![1000];
        let f = write_wav_16bit(&samples, 1, 44100);
        let reader = MmapReader::open(f.path()).unwrap();

        let mut buf = [99.0f32; 2];
        let n = reader.read_frames(100, &mut buf);
        assert_eq!(n, 0);
        assert_eq!(buf[0], 0.0);
        assert_eq!(buf[1], 0.0);
    }

    #[test]
    fn invalid_file_rejected() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"NOT_AN_AUDIO_FILE_HEADER").unwrap();
        f.flush().unwrap();
        assert!(MmapReader::open(f.path()).is_err());
    }
}
