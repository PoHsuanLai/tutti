//! Audio capture (recording) functionality for butler thread.
//!
//! Writes 32-bit float WAV files for live capture. This intentionally stays
//! a thin direct use of `hound` rather than going through `tutti-export`'s
//! `StreamingEncoder` — live capture wants the simplest possible path
//! (open → write_chunk → flush), no dither / no bit-depth choice / no
//! mono downmix, and no extra crate boundary on the hot path. The export
//! crate handles the offline-render side with the full pipeline.

use super::super::command::CaptureId;
use super::super::metrics::Metrics;
use super::super::prefetch::CaptureReader;
use hound::{SampleFormat, WavSpec, WavWriter};
use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;

pub struct ActiveCapture {
    pub consumer: CaptureReader,
    pub writer: Option<WavWriter<BufWriter<File>>>,
    pub channels: usize,
}

pub(crate) fn open_wav(
    file_path: &PathBuf,
    sample_rate: f64,
    channels: usize,
) -> Option<WavWriter<BufWriter<File>>> {
    let spec = WavSpec {
        channels: channels as u16,
        sample_rate: sample_rate as u32,
        bits_per_sample: 32,
        sample_format: SampleFormat::Float,
    };

    let file = File::create(file_path).ok()?;
    let buf_writer = BufWriter::new(file);
    WavWriter::new(buf_writer, spec).ok()
}

pub(crate) fn flush_capture(state: &mut ActiveCapture, metrics: &Metrics, max_samples: usize) {
    let Some(writer) = state.writer.as_mut() else {
        return;
    };

    let available = state.consumer.available();
    let to_read = available.min(max_samples);

    if to_read == 0 {
        return;
    }

    let mut buffer = vec![(0.0f32, 0.0f32); to_read];
    let read = state.consumer.read_into(&mut buffer);

    for &(left, right) in &buffer[..read] {
        if writer.write_sample(left).is_err() {
            return;
        }
        if state.channels > 1 && writer.write_sample(right).is_err() {
            return;
        }
    }

    let bytes_written = read as u64 * state.channels as u64 * 4;
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
