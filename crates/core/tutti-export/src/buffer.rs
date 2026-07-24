//! Buffer export — process and encode already-rendered stereo samples.
//!
//! Skips the render stage. Shares the whole output stage with
//! [`GraphExport`](crate::GraphExport) via a common [`Output`], minus the
//! time/transport/MIDI knobs (which are render-stage concepts).

use crate::encode;
use crate::error::Result;
use crate::options::{output_setters, AudioFormat, ChannelMode, Output};
use crate::process;
use crate::progress::{Phase, PhaseGuard};
use crate::run::{Rendered, Run, Written};
use std::path::{Path, PathBuf};

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
    // `sample_rate`, `resample_quality`, `dither`, `normalize`, `flac`, `ogg`),
    // generated from the one definition in `options` — same surface as
    // `GraphExport`. They write into `self.output`.
    output_setters!(output);

    pub fn to_file(self, path: impl AsRef<Path>) -> Run<Written> {
        let path = path.as_ref().to_path_buf();
        Run {
            job: Box::new(move |on_progress| run_to_file(self, path, on_progress)),
        }
    }

    pub fn to_buffers(self) -> Run<Rendered> {
        Run {
            job: Box::new(move |on_progress| run_to_buffers(self, on_progress)),
        }
    }
}

fn output_sample_rate(b: &BufferExport) -> u32 {
    b.output.sample_rate(b.sample_rate)
}

fn mastering(b: &BufferExport, target_rate: u32) -> process::Mastering {
    process::Mastering {
        source_sample_rate: b.sample_rate.round() as u32,
        target_sample_rate: target_rate,
        normalize: b.output.normalize,
        dither: b.output.dither,
        bit_depth: b.output.bit_depth,
        resample_quality: b.output.resample_quality,
    }
}

fn run_to_file(
    b: BufferExport,
    path: PathBuf,
    on_progress: &(dyn Fn(Phase, f32) + Send + Sync),
) -> Result<Written> {
    let target_rate = output_sample_rate(&b);
    let format = match b.output.format {
        Some(f) => f,
        None => AudioFormat::from_path(&path)?,
    };

    let (frames, target_rate) = {
        let _phase = PhaseGuard::new(on_progress, Phase::Process);
        process::master_collected(b.left.clone(), b.right.clone(), &mastering(&b, target_rate))?
    };

    encode::encode(
        &frames,
        encode::EncodeRequest {
            path: &path,
            format,
            sample_rate: target_rate,
            bit_depth: b.output.bit_depth,
            channels: b.output.channels,
            flac: b.output.flac,
            ogg: b.output.ogg,
        },
        on_progress,
    )?;

    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    Ok(Written { path, bytes })
}

fn run_to_buffers(
    b: BufferExport,
    on_progress: &(dyn Fn(Phase, f32) + Send + Sync),
) -> Result<Rendered> {
    let target_rate = output_sample_rate(&b);
    let (frames, target_rate) = {
        let _phase = PhaseGuard::new(on_progress, Phase::Process);
        process::master_collected(b.left.clone(), b.right.clone(), &mastering(&b, target_rate))?
    };
    // Deinterleave the mastered frames back to planes. Mono folds each frame
    // and duplicates it into both planes so a mono request still round-trips as
    // a (left, right) pair.
    let (left, right) = match b.output.channels {
        ChannelMode::Stereo => (
            frames.iter().map(|&[l, _]| l).collect(),
            frames.iter().map(|&[_, r]| r).collect(),
        ),
        ChannelMode::Mono => {
            let mono: Vec<f32> = frames.iter().map(|&f| process::fold_frame(f)).collect();
            (mono.clone(), mono)
        }
    };
    Ok(Rendered {
        left,
        right,
        sample_rate: target_rate as f64,
    })
}
