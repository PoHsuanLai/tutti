//! Audio capture (recording) — the write side of the sampler.
//!
//! Playback pulls frames from a [`SampleSource`](crate::playback::SampleSource)
//! into the graph; recording pushes frames the other way, into a file. This
//! module owns that write side as a [`SampleSink`]: `write` incrementally, then
//! `finalize`.
//!
//! The live WAV sink writes 32-bit float (default) or 24-bit int, INCREMENTALLY
//! as frames arrive off the capture ring — a recording is minutes long and never
//! held resident. That is the deliberate contrast with `tutti-export`'s offline
//! `FileSink`, which buffers a whole signal before encoding (inherent for
//! compressed formats). Same "push frames → file" concept, two impls; they share
//! the vocabulary, not the implementation.
//!
//! Kept a thin direct use of `hound` rather than routing through
//! `tutti-export`'s `StreamingEncoder`: live capture wants the simplest possible
//! path (open → write_chunk → finalize), no dither / no mono downmix, and no
//! extra crate boundary on the flush path.

use super::super::command::CaptureId;
use super::super::metrics::Metrics;
use super::super::prefetch::CaptureReader;
use crate::capture::CaptureFormat;
use hound::{SampleFormat, WavSpec, WavWriter};
use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;

/// The write-side twin of [`SampleSource`](crate::playback::SampleSource):
/// "where captured frames GO". A sink is fed stereo frames incrementally with
/// [`write`](SampleSink::write) and closed once with
/// [`finalize`](SampleSink::finalize), which yields whatever the write produced
/// (for a file sink, the header is back-patched here).
///
/// Not an RT interface — the butler thread drives it after draining the capture
/// ring. The audio thread only pushes into that lock-free ring
/// ([`CaptureWriter`](crate::butler::CaptureWriter)); it never touches a sink.
pub trait SampleSink {
    /// Append `frames` to the sink. Called repeatedly as capture progresses;
    /// implementations write incrementally and never buffer the whole recording.
    fn write(&mut self, frames: &[(f32, f32)]);

    /// Close the sink, flushing any buffered bytes and committing the result.
    /// For the WAV sink this back-patches the RIFF/`data` chunk sizes in the
    /// header, so a failure here means the file is left unreadable — surface it.
    fn finalize(self) -> std::io::Result<()>;
}

/// Live WAV [`SampleSink`]. Owns the `hound` writer plus the channel count and
/// on-disk format needed to encode each frame.
pub struct WavSink {
    writer: WavWriter<BufWriter<File>>,
    channels: usize,
    format: CaptureFormat,
}

impl WavSink {
    /// Create the file and WAV header for `file_path`. Returns `None` if the
    /// file can't be created or the header can't be written.
    pub(crate) fn create(
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

    /// Bytes written per interleaved sample for this sink's format.
    fn bytes_per_sample(&self) -> u64 {
        match self.format {
            CaptureFormat::F32 => 4,
            CaptureFormat::I24 => 3,
        }
    }
}

impl SampleSink for WavSink {
    fn write(&mut self, frames: &[(f32, f32)]) {
        for &(left, right) in frames {
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

/// One in-flight capture: the ring the audio thread pushes into, and the sink
/// the butler drains it into. `sink` is `None` if the file couldn't be opened.
pub struct ActiveCapture {
    pub consumer: CaptureReader,
    pub sink: Option<WavSink>,
}

/// Clamp an f32 sample to `[-1.0, 1.0]` and scale to a 24-bit signed integer.
#[inline]
fn f32_to_i24(sample: f32) -> i32 {
    let clamped = sample.clamp(-1.0, 1.0);
    // 24-bit signed range: [-8_388_608, 8_388_607].
    (clamped * 8_388_607.0).round() as i32
}

pub(crate) fn flush_capture(state: &mut ActiveCapture, metrics: &Metrics, max_samples: usize) {
    let Some(sink) = state.sink.as_mut() else {
        return;
    };

    let available = state.consumer.available();
    let to_read = available.min(max_samples);

    if to_read == 0 {
        return;
    }

    let mut buffer = vec![(0.0f32, 0.0f32); to_read];
    let read = state.consumer.read_into(&mut buffer);

    sink.write(&buffer[..read]);

    let bytes_written = read as u64 * sink.channels as u64 * sink.bytes_per_sample();
    metrics.record_write(bytes_written);

    state.consumer.add_frames_written(read as u64);
}

pub(crate) fn flush_all(
    capture_consumers: &mut std::collections::HashMap<CaptureId, ActiveCapture>,
    metrics: &Metrics,
    threshold: usize,
    force: bool,
) {
    for state in capture_consumers.values_mut() {
        let available = state.consumer.available();

        if force || available >= threshold {
            flush_capture(state, metrics, if force { usize::MAX } else { threshold });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sink writes INCREMENTALLY: feeding frames across many `write` calls
    /// (as the butler does, one flushed ring-block at a time) and finalizing must
    /// yield a valid WAV whose frame count is the sum of every block — the sink
    /// never has to see the whole recording at once. Guards the plan's core
    /// contract that live capture is a streaming sink, not a buffering one.
    #[test]
    fn wav_sink_writes_incrementally_and_finalizes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("capture.wav");

        let mut sink =
            WavSink::create(&path, 48_000.0, 2, CaptureFormat::F32).expect("sink should open");

        // Feed 5 separate blocks — the sink sees only one block at a time, never
        // the full signal, exactly as the incremental flush loop drives it.
        let block: Vec<(f32, f32)> = (0..256)
            .map(|i| (i as f32 / 256.0, -(i as f32) / 256.0))
            .collect();
        let blocks = 5;
        for _ in 0..blocks {
            sink.write(&block);
        }
        sink.finalize()
            .expect("finalize should back-patch the header");

        // The finalized file must be readable and hold every frame from every
        // block (2 channels → interleaved sample count = frames * 2).
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
        let frames = vec![(0.5f32, 0.9f32); 128];
        sink.write(&frames);
        sink.finalize().unwrap();

        let reader = hound::WavReader::open(&path).unwrap();
        assert_eq!(reader.spec().channels, 1);
        assert_eq!(reader.len() as usize, frames.len());
    }
}
