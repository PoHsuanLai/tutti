//! Buffer export — process and encode already-rendered stereo samples.
//!
//! Skips the render stage. Shares the whole output stage with
//! [`GraphExport`](crate::GraphExport) via a common [`Output`], minus the
//! time/transport/MIDI knobs (which are render-stage concepts).

use crate::encode;
use crate::error::{Error, Result};
use crate::options::{output_setters, AudioFormat, Output};
use crate::process;
use crate::progress::{Phase, PhaseGuard};
use crate::run::{Rendered, Run, Written};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Debug)]
pub struct BufferExport {
    left: Vec<f32>,
    right: Vec<f32>,
    sample_rate: f64,
    output: Output,
}

impl BufferExport {
    pub(crate) fn new(left: Vec<f32>, right: Vec<f32>, sample_rate: f64) -> Self {
        Self {
            left,
            right,
            sample_rate,
            output: Output::default(),
        }
    }

    // The shared output/mastering setters (`format`, `bit_depth`, `channels`,
    // `sample_rate`, `resample_quality`, `dither`, `normalize`, `flac`, `ogg`,
    // `bwav`), generated from the one definition in `options` — same surface as
    // `GraphExport`. They write into `self.output`.
    output_setters!(output);

    pub fn to_file(self, path: impl AsRef<Path>) -> Run<Written> {
        let path = path.as_ref().to_path_buf();
        Run {
            job: Box::new(move |on_progress, cancel| run_to_file(self, path, on_progress, cancel)),
        }
    }

    pub fn to_buffers(self) -> Run<Rendered> {
        Run {
            job: Box::new(move |on_progress, cancel| run_to_buffers(self, on_progress, cancel)),
        }
    }
}

fn output_sample_rate(b: &BufferExport) -> u32 {
    b.output.sample_rate(b.sample_rate)
}

fn run_to_file(
    b: BufferExport,
    path: PathBuf,
    on_progress: &(dyn Fn(Phase, f32) + Send + Sync),
    cancel: &Arc<AtomicBool>,
) -> Result<Written> {
    if cancel.load(Ordering::Relaxed) {
        return Err(Error::Cancelled);
    }

    let target_rate = output_sample_rate(&b);
    let format = match b.output.format {
        Some(f) => f,
        None => AudioFormat::from_path(&path)?,
    };

    let (processed, target_rate) = {
        let _phase = PhaseGuard::new(on_progress, Phase::Process);
        process::master_collected(
            b.left.clone(),
            b.right.clone(),
            b.sample_rate.round() as u32,
            target_rate,
            b.output.normalize,
            b.output.resample_quality,
            b.output.dither,
            b.output.bit_depth,
            b.output.channels,
        )?
    };

    if cancel.load(Ordering::Relaxed) {
        return Err(Error::Cancelled);
    }

    encode::encode(
        processed,
        encode::EncodeRequest {
            path: &path,
            format,
            sample_rate: target_rate,
            bit_depth: b.output.bit_depth,
            flac: b.output.flac,
            ogg: b.output.ogg,
            bwav_metadata: b.output.bwav.as_ref(),
        },
        on_progress,
    )?;

    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    Ok(Written { path, bytes })
}

fn run_to_buffers(
    b: BufferExport,
    on_progress: &(dyn Fn(Phase, f32) + Send + Sync),
    _cancel: &Arc<AtomicBool>,
) -> Result<Rendered> {
    let target_rate = output_sample_rate(&b);
    let (processed, target_rate) = {
        let _phase = PhaseGuard::new(on_progress, Phase::Process);
        process::master_collected(
            b.left.clone(),
            b.right.clone(),
            b.sample_rate.round() as u32,
            target_rate,
            b.output.normalize,
            b.output.resample_quality,
            b.output.dither,
            b.output.bit_depth,
            b.output.channels,
        )?
    };
    let (left, right) = match processed {
        process::Chunk::Stereo { left, right } => (left, right),
        process::Chunk::Mono(samples) => (samples.clone(), samples),
    };
    Ok(Rendered {
        left,
        right,
        sample_rate: target_rate as f64,
    })
}
