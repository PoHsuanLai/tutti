//! Buffer export — process and encode already-rendered stereo samples.
//!
//! Skips the render stage. Shares the whole output stage with
//! [`GraphExport`](crate::GraphExport) via a common [`Output`], minus the
//! time/transport/MIDI knobs (which are render-stage concepts).

use crate::encode;
use crate::error::Result;
use crate::options::{output_setters, AudioFormat, Output};
use crate::process;
use crate::progress::{Phase, PhaseGuard};
use crate::run::{Run, Written};
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
    let m = mastering(&b, target_rate);

    // `BufferExport` always holds a stereo (left, right) source, so master and
    // encode at `CH = 2`; the requested `channels` still shapes the *output*
    // file (mono fold / stereo / wider-as-stereo) inside `encode::rechannel`.
    let (frames, target_rate) = {
        let _phase = PhaseGuard::new(on_progress, Phase::Process);
        let planes = [b.left.clone(), b.right.clone()];
        process::master_collected::<2>(planes, &m)?
    };

    encode::encode::<2>(
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
