//! [`WavOut`] — the live WAV implementation of [`AudioOut`](crate::AudioOut).
//!
//! An [`AudioOut`](crate::AudioOut) is "push frames → destination"; this is that
//! destination for a WAV file. It writes 32-bit float (default) or 24-bit int,
//! INCREMENTALLY — a recording is minutes long and never held resident. That is
//! the deliberate contrast with `tutti-export`'s offline render path (its
//! `StreamingEncoder`), which can buffer/dither a whole signal before encoding.
//! Same "push frames → file" concept, two impls; they share the vocabulary
//! ([`AudioOut`](crate::AudioOut)), not the implementation.
//!
//! Kept a thin direct use of `hound` rather than routing through `tutti-export`'s
//! `StreamingEncoder`: a live sink wants the simplest possible path (open →
//! write → finalize), no dither / no mono downmix, no extra crate boundary.
//!
//! The live driver is bevy-tutti's `Recorder`, which pumps a `MicIn`
//! ([`AudioIn`](crate::AudioIn)) into this sink ([`AudioOut`](crate::AudioOut))
//! on a background thread and calls [`finalize`](AudioOut::finalize) once at stop.

use crate::io::AudioOut;
use hound::{SampleFormat, WavSpec, WavWriter};
use tutti_core::ChannelLayout;
use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;
use tutti_core::pcm::f32_to_i24;

/// On-disk sample format for a [`WavOut`].
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
pub struct WavOut {
    writer: WavWriter<BufWriter<File>>,
    layout: ChannelLayout,
    format: CaptureFormat,
}

// Hand-rolled: `hound::WavWriter` isn't `Debug`. Print the channel count +
// on-disk format; the writer itself is opaque.
impl std::fmt::Debug for WavOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WavOut")
            .field("layout", &self.layout)
            .field("format", &self.format)
            .finish_non_exhaustive()
    }
}

impl WavOut {
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
        let layout = ChannelLayout::from(channels);
        let spec = WavSpec {
            channels: layout.count(),
            sample_rate: sample_rate as u32,
            bits_per_sample,
            sample_format,
        };

        let file = File::create(file_path).ok()?;
        let buf_writer = BufWriter::new(file);
        let writer = WavWriter::new(buf_writer, spec).ok()?;
        Some(Self {
            writer,
            layout,
            format,
        })
    }
}

impl AudioOut for WavOut {
    fn write(&mut self, frames: &[[f32; 2]]) {
        // Write the right channel only when the sink is stereo (or wider); a mono
        // sink drops it.
        let write_right = match self.layout {
            ChannelLayout::Stereo | ChannelLayout::Quad | ChannelLayout::Multi(_) => true,
            ChannelLayout::Mono => false,
        };
        for &[left, right] in frames {
            match self.format {
                CaptureFormat::F32 => {
                    if self.writer.write_sample(left).is_err() {
                        return;
                    }
                    if write_right && self.writer.write_sample(right).is_err() {
                        return;
                    }
                }
                CaptureFormat::I24 => {
                    if self.writer.write_sample(f32_to_i24(left)).is_err() {
                        return;
                    }
                    if write_right && self.writer.write_sample(f32_to_i24(right)).is_err() {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The sink writes INCREMENTALLY: feeding frames across many `write` calls
    /// and finalizing must yield a valid WAV whose frame count is the sum of
    /// every block — the sink never has to see the whole recording at once.
    #[test]
    fn wav_out_writes_incrementally_and_finalizes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("capture.wav");

        let mut sink =
            WavOut::create(&path, 48_000.0, 2, CaptureFormat::F32).expect("sink should open");

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
    fn wav_out_mono_writes_one_sample_per_frame() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mono.wav");

        let mut sink =
            WavOut::create(&path, 44_100.0, 1, CaptureFormat::F32).expect("sink should open");
        let frames = vec![[0.5f32, 0.9f32]; 128];
        sink.write(&frames);
        sink.finalize().unwrap();

        let reader = hound::WavReader::open(&path).unwrap();
        assert_eq!(reader.spec().channels, 1);
        assert_eq!(reader.len() as usize, frames.len());
    }
}
