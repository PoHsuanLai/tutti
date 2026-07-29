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
//! The live driver is `tutti_cpal::Recorder`, which pumps a `MicIn`
//! ([`AudioIn`](crate::AudioIn)) into this sink ([`AudioOut`](crate::AudioOut))
//! on a background thread and calls [`finalize`](AudioOut::finalize) once at stop.

use hound::{SampleFormat, WavSpec, WavWriter};
use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;
use tutti_core::io::AudioOut;
use tutti_core::pcm::f32_to_i24;
use tutti_core::ChannelLayout;

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

    /// Declared channel count — what the WAV header says, and therefore exactly
    /// how many samples per frame [`write_interleaved`](Self::write_interleaved)
    /// must emit.
    pub fn channels(&self) -> usize {
        self.layout.count() as usize
    }

    /// Write flat interleaved frames at this sink's own declared width.
    ///
    /// Emits exactly `channels()` samples per frame — no more, no fewer. A short
    /// trailing frame is ignored; a frame wider than the header is truncated to
    /// it.
    ///
    /// This is the fix for a latent corruption: the header took the caller's
    /// full `channels` while `write` only ever emitted two samples per frame, so
    /// a >2-channel capture produced a file whose declared width and actual data
    /// disagreed. Every reader would interleave-misalign, rotating channels by
    /// `2 mod channels` each frame, and `hound` could fail to finalize on a
    /// non-integral frame count. Unreachable from the four live call sites (all
    /// pass 1 or 2) and untested above 2 — hence unnoticed.
    pub fn write_interleaved(&mut self, samples: &[f32]) {
        let ch = self.channels().max(1);
        for frame in samples.chunks_exact(ch) {
            for &s in frame {
                let ok = match self.format {
                    CaptureFormat::F32 => self.writer.write_sample(s).is_ok(),
                    CaptureFormat::I24 => self.writer.write_sample(f32_to_i24(s)).is_ok(),
                };
                if !ok {
                    return;
                }
            }
        }
    }
}

impl AudioOut for WavOut {
    /// Stereo shim over [`write_interleaved`](WavOut::write_interleaved).
    ///
    /// A mono sink drops the right channel; a sink wider than stereo zero-fills
    /// the channels this stereo-framed input cannot supply, so the data still
    /// matches the declared header width.
    fn write(&mut self, frames: &[[f32; 2]]) {
        let ch = self.channels().max(1);
        match ch {
            1 => {
                for &[left, _] in frames {
                    self.write_interleaved(&[left]);
                }
            }
            2 => self.write_interleaved(frames.as_flattened()),
            _ => {
                let mut frame = vec![0.0f32; ch];
                for &[left, right] in frames {
                    frame[0] = left;
                    frame[1] = right;
                    self.write_interleaved(&frame);
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

    /// A 6-channel sink must write SIX samples per frame, matching the header it
    /// declared. Before this, the header took `layout.count()` while `write`
    /// emitted two, so the declared width and the data disagreed: a reader
    /// interleave-misaligns and the channels rotate by `2 mod 6` every frame.
    #[test]
    fn six_channel_sink_writes_six_samples_per_frame() {
        let dir = std::env::temp_dir();
        let path = dir.join("tutti_wav_out_six_channel.wav");
        let _ = std::fs::remove_file(&path);

        let mut sink =
            WavOut::create(&path, 48_000.0, 6, CaptureFormat::F32).expect("create 6ch sink");
        assert_eq!(sink.channels(), 6);

        let frames: Vec<f32> = (0..128)
            .flat_map(|i| (0..6).map(move |c| (i * 6 + c) as f32 * 0.001))
            .collect();
        sink.write_interleaved(&frames);
        AudioOut::finalize(sink).expect("finalize");

        let reader = hound::WavReader::open(&path).expect("reopen");
        assert_eq!(reader.spec().channels, 6, "header must declare 6 channels");
        assert_eq!(
            reader.len() as usize,
            128 * 6,
            "128 frames of 6 channels must write 768 samples, not 256"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// The stereo shim must still produce a well-formed file at a wider declared
    /// width: it zero-fills the channels it cannot supply rather than emitting
    /// short frames.
    #[test]
    fn stereo_write_into_a_wide_sink_stays_frame_aligned() {
        let dir = std::env::temp_dir();
        let path = dir.join("tutti_wav_out_stereo_into_quad.wav");
        let _ = std::fs::remove_file(&path);

        let mut sink =
            WavOut::create(&path, 48_000.0, 4, CaptureFormat::F32).expect("create 4ch sink");
        let frames = [[0.25f32, -0.25]; 16];
        AudioOut::write(&mut sink, &frames);
        AudioOut::finalize(sink).expect("finalize");

        let mut reader = hound::WavReader::open(&path).expect("reopen");
        assert_eq!(reader.spec().channels, 4);
        assert_eq!(
            reader.len() as usize,
            16 * 4,
            "must be a whole number of frames"
        );
        let samples: Vec<f32> = reader.samples::<f32>().map(|s| s.unwrap()).collect();
        assert!((samples[0] - 0.25).abs() < 1e-6);
        assert!((samples[1] + 0.25).abs() < 1e-6);
        assert_eq!(samples[2], 0.0, "unsupplied channels are silent");
        assert_eq!(samples[3], 0.0);
        let _ = std::fs::remove_file(&path);
    }
}
