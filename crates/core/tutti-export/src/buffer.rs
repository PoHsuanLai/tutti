//! Buffer export — process and encode already-rendered stereo samples.
//!
//! Skips the render stage. Same configuration surface as [`GraphExport`]
//! minus the time/transport/MIDI knobs (which are render-stage concepts).
//! Same terminals minus `stream_to_file`.

use crate::encode;
use crate::error::{Error, Result};
use crate::options::{
    AudioFormat, BitDepth, BroadcastWavMetadata, ChannelMode, Dither, Flac, Normalize, Ogg,
};
use crate::process::{self, ResampleQuality};
use crate::progress::{Phase, PhaseGuard};
use crate::run::{Rendered, Run, Written};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub struct BufferExport {
    left: Vec<f32>,
    right: Vec<f32>,
    sample_rate: f64,
    format: Option<AudioFormat>,
    bit_depth: BitDepth,
    channels: ChannelMode,
    target_sample_rate: Option<u32>,
    resample_quality: ResampleQuality,
    dither: Dither,
    normalize: Normalize,
    flac: Flac,
    ogg: Ogg,
    bwav: Option<BroadcastWavMetadata>,
}

impl BufferExport {
    pub(crate) fn new(left: Vec<f32>, right: Vec<f32>, sample_rate: f64) -> Self {
        Self {
            left,
            right,
            sample_rate,
            format: None,
            bit_depth: BitDepth::default(),
            channels: ChannelMode::default(),
            target_sample_rate: None,
            resample_quality: ResampleQuality::default(),
            dither: Dither::default(),
            normalize: Normalize::default(),
            flac: Flac::default(),
            ogg: Ogg::default(),
            bwav: None,
        }
    }

    pub fn format(mut self, f: AudioFormat) -> Self {
        self.format = Some(f);
        self
    }
    pub fn bit_depth(mut self, b: BitDepth) -> Self {
        self.bit_depth = b;
        self
    }
    pub fn channels(mut self, c: ChannelMode) -> Self {
        self.channels = c;
        self
    }
    pub fn sample_rate(mut self, rate: u32) -> Self {
        self.target_sample_rate = Some(rate);
        self
    }
    pub fn resample_quality(mut self, q: ResampleQuality) -> Self {
        self.resample_quality = q;
        self
    }
    pub fn dither(mut self, d: Dither) -> Self {
        self.dither = d;
        self
    }
    pub fn normalize(mut self, mode: Normalize) -> Self {
        self.normalize = mode;
        self
    }
    pub fn flac(mut self, opts: Flac) -> Self {
        self.flac = opts;
        self
    }
    pub fn ogg(mut self, opts: Ogg) -> Self {
        self.ogg = opts;
        self
    }
    pub fn bwav(mut self, meta: BroadcastWavMetadata) -> Self {
        self.bwav = Some(meta);
        self
    }

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
    b.target_sample_rate
        .unwrap_or_else(|| b.sample_rate.round() as u32)
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
    let format = match b.format {
        Some(f) => f,
        None => AudioFormat::from_path(&path)?,
    };

    let processed = {
        let _phase = PhaseGuard::new(on_progress, Phase::Process);
        process::process(process::ProcessRequest {
            left: &b.left,
            right: &b.right,
            source_sample_rate: b.sample_rate.round() as u32,
            target_sample_rate: target_rate,
            normalize: b.normalize,
            dither: b.dither,
            bit_depth: b.bit_depth,
            channels: b.channels,
            resample_quality: b.resample_quality,
        })?
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
            bit_depth: b.bit_depth,
            flac: b.flac,
            ogg: b.ogg,
            bwav_metadata: b.bwav.as_ref(),
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
    let processed = {
        let _phase = PhaseGuard::new(on_progress, Phase::Process);
        process::process(process::ProcessRequest {
            left: &b.left,
            right: &b.right,
            source_sample_rate: b.sample_rate.round() as u32,
            target_sample_rate: target_rate,
            normalize: b.normalize,
            dither: b.dither,
            bit_depth: b.bit_depth,
            channels: b.channels,
            resample_quality: b.resample_quality,
        })?
    };
    let (left, right) = match processed {
        process::ProcessedAudio::Stereo { left, right } => (left, right),
        process::ProcessedAudio::Mono(samples) => (samples.clone(), samples),
    };
    Ok(Rendered {
        left,
        right,
        sample_rate: target_rate as f64,
    })
}
