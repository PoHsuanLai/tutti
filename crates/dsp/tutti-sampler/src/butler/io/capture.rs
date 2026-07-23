//! Audio capture (recording) functionality for butler thread.
//!
//! Writes WAV files for live capture — 32-bit float (default) or 24-bit int.
//! This intentionally stays a thin direct use of `hound` rather than going
//! through `tutti-export`'s `StreamingEncoder` — live capture wants the
//! simplest possible path (open → write_chunk → flush), no dither / no mono
//! downmix, and no extra crate boundary on the hot path. The export crate
//! handles the offline-render side with the full pipeline.

use super::super::command::CaptureId;
use super::super::metrics::Metrics;
use super::super::prefetch::CaptureReader;
use crate::capture::CaptureFormat;
use hound::{SampleFormat, WavSpec, WavWriter};
use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;

pub struct ActiveCapture {
    pub consumer: CaptureReader,
    pub writer: Option<WavWriter<BufWriter<File>>>,
    pub channels: usize,
    pub format: CaptureFormat,
}

/// Bytes written per interleaved sample for a given capture format.
fn bytes_per_sample(format: CaptureFormat) -> u64 {
    match format {
        CaptureFormat::F32 => 4,
        CaptureFormat::I24 => 3,
    }
}

/// Clamp an f32 sample to `[-1.0, 1.0]` and scale to a 24-bit signed integer.
#[inline]
fn f32_to_i24(sample: f32) -> i32 {
    let clamped = sample.clamp(-1.0, 1.0);
    // 24-bit signed range: [-8_388_608, 8_388_607].
    (clamped * 8_388_607.0).round() as i32
}

pub(crate) fn open_wav(
    file_path: &PathBuf,
    sample_rate: f64,
    channels: usize,
    format: CaptureFormat,
) -> Option<WavWriter<BufWriter<File>>> {
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
        match state.format {
            CaptureFormat::F32 => {
                if writer.write_sample(left).is_err() {
                    return;
                }
                if state.channels > 1 && writer.write_sample(right).is_err() {
                    return;
                }
            }
            CaptureFormat::I24 => {
                if writer.write_sample(f32_to_i24(left)).is_err() {
                    return;
                }
                if state.channels > 1 && writer.write_sample(f32_to_i24(right)).is_err() {
                    return;
                }
            }
        }
    }

    let bytes_written = read as u64 * state.channels as u64 * bytes_per_sample(state.format);
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
