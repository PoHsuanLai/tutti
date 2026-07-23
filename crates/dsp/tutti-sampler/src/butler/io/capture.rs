//! [`WavSink`] — the live WAV implementation of [`AudioOut`](crate::AudioOut).
//!
//! An [`AudioOut`](crate::AudioOut) is "push frames → destination"; this is that
//! destination for a WAV file. It writes 32-bit float (default) or 24-bit int,
//! INCREMENTALLY — a recording is minutes long and never held resident. That is
//! the deliberate contrast with `tutti-export`'s offline `FileSink`, which
//! buffers a whole signal before encoding (inherent for compressed formats).
//! Same "push frames → file" concept, two impls; they share the vocabulary
//! ([`AudioOut`](crate::AudioOut)), not the implementation.
//!
//! Kept a thin direct use of `hound` rather than routing through `tutti-export`'s
//! `StreamingEncoder`: a live sink wants the simplest possible path (open →
//! write → finalize), no dither / no mono downmix, no extra crate boundary.
//!
//! The live driver is bevy-tutti's `Recorder`, which pumps a `MicSource`
//! ([`AudioIn`](crate::AudioIn)) into this sink ([`AudioOut`](crate::AudioOut))
//! on a background thread and calls [`finalize`](AudioOut::finalize) once at stop.

use crate::io::AudioOut;
use hound::{SampleFormat, WavSpec, WavWriter};
use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;

/// On-disk sample format for a [`WavSink`].
///
/// Defaults to `F32` — the simplest, lossless-for-our-graph path. `I24` trades a
/// little precision for smaller files where 24-bit int is desired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CaptureFormat {
    /// 32-bit IEEE float (default).
    #[default]
    F32,
    /// 24-bit signed integer.
    I24,
}

/// Live WAV [`AudioOut`]. Owns the `hound` writer plus the channel count and
/// on-disk format needed to encode each frame.
pub struct WavSink {
    writer: WavWriter<BufWriter<File>>,
    channels: usize,
    format: CaptureFormat,
}

impl WavSink {
    /// Create the file and WAV header for `file_path`. Returns `None` if the
    /// file can't be created or the header can't be written.
    pub fn create(
        file_path: &PathBuf,
        sample_rate: f64,
        channels: usize,
        format: CaptureFormat,
    ) -> Option<Self> {
        let (bits_per_sample, sample_format) = match format {
            CaptureFormat::F32 => (32, SampleFormat::Float),
            CaptureFormat::I24 => (24, SampleFormat::Int),
        };
        let spec = WavSpec {
            channels: channels as u16,
            sample_rate: sample_rate as u32,
            bits_per_sample,
            sample_format,
        };

        let file = File::create(file_path).ok()?;
        let buf_writer = BufWriter::new(file);
        let writer = WavWriter::new(buf_writer, spec).ok()?;
        Some(Self {
            writer,
            channels,
            format,
        })
    }
}

impl AudioOut for WavSink {
    fn write(&mut self, frames: &[[f32; 2]]) {
        for &[left, right] in frames {
            match self.format {
                CaptureFormat::F32 => {
                    if self.writer.write_sample(left).is_err() {
                        return;
                    }
                    if self.channels > 1 && self.writer.write_sample(right).is_err() {
                        return;
                    }
                }
                CaptureFormat::I24 => {
                    if self.writer.write_sample(f32_to_i24(left)).is_err() {
                        return;
                    }
                    if self.channels > 1 && self.writer.write_sample(f32_to_i24(right)).is_err() {
                        return;
                    }
                }
            }
        }
    }

    fn finalize(self) -> std::io::Result<()> {
        // `hound::Error` -> io error: finalize flushes the sample buffer and
        // back-patches the RIFF/data chunk sizes. On failure the file is left
        // with a stale header and is unreadable — a real integrity loss.
        self.writer
            .finalize()
            .map_err(|e| std::io::Error::other(e.to_string()))
    }
}

/// Clamp an f32 sample to `[-1.0, 1.0]` and scale to a 24-bit signed integer.
#[inline]
fn f32_to_i24(sample: f32) -> i32 {
    let clamped = sample.clamp(-1.0, 1.0);
    // 24-bit signed range: [-8_388_608, 8_388_607].
    (clamped * 8_388_607.0).round() as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sink writes INCREMENTALLY: feeding frames across many `write` calls
    /// and finalizing must yield a valid WAV whose frame count is the sum of
    /// every block — the sink never has to see the whole recording at once.
    #[test]
    fn wav_sink_writes_incrementally_and_finalizes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("capture.wav");

        let mut sink =
            WavSink::create(&path, 48_000.0, 2, CaptureFormat::F32).expect("sink should open");

        let block: Vec<[f32; 2]> = (0..256)
            .map(|i| [i as f32 / 256.0, -(i as f32) / 256.0])
            .collect();
        let blocks = 5;
        for _ in 0..blocks {
            sink.write(&block);
        }
        sink.finalize()
            .expect("finalize should back-patch the header");

        let reader = hound::WavReader::open(&path).expect("finalized WAV should be readable");
        let spec = reader.spec();
        assert_eq!(spec.channels, 2);
        assert_eq!(spec.sample_rate, 48_000);
        assert_eq!(reader.len() as usize, block.len() * blocks * 2);
    }

    /// Mono capture writes one sample per frame (the right channel is dropped).
    #[test]
    fn wav_sink_mono_writes_one_sample_per_frame() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mono.wav");

        let mut sink =
            WavSink::create(&path, 44_100.0, 1, CaptureFormat::F32).expect("sink should open");
        let frames = vec![[0.5f32, 0.9f32]; 128];
        sink.write(&frames);
        sink.finalize().unwrap();

        let reader = hound::WavReader::open(&path).unwrap();
        assert_eq!(reader.spec().channels, 1);
        assert_eq!(reader.len() as usize, frames.len());
    }
}
