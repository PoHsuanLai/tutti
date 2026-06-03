//! Path-backed streaming-sample asset.
//!
//! Long audio files that shouldn't live in RAM. [`StreamingSample::probe`]
//! reads the WAV header (via `hound`, already a sampler dependency) and
//! returns a locator plus duration/format metadata. The sampler's Butler
//! thread opens its own handle for streaming playback.
//!
//! # Progress
//!
//! Every [`StreamingSample`] carries an `Arc<AtomicF32>` progress field
//! (0.0..=1.0). For a bare header probe that's effectively a no-op — the
//! loader sets it to `1.0` once done. Callers that run longer work against
//! the asset (peak generation, resampling, pre-warming caches) share the
//! same atomic and write in-flight progress there, which lets UI code poll
//! with `assets.get(&handle)?.progress.load(Ordering::Relaxed)` without any
//! events or channels.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tutti_core::{AtomicF32, Ordering};

/// Shared lock-free loading progress for a streaming sample.
///
/// Values are in `0.0..=1.0`. `1.0` means the asset is fully probed and
/// any follow-up processing (peak generation, etc.) has finished. The
/// asset loader initialises this to `0.0`, writes `1.0` on success.
#[derive(Debug, Clone, Default)]
pub struct StreamingProgress(pub Arc<AtomicF32>);

impl StreamingProgress {
    pub fn new() -> Self {
        Self(Arc::new(AtomicF32::new(0.0)))
    }

    /// Current progress value, clamped to `0.0..=1.0`.
    pub fn get(&self) -> f32 {
        self.0.load(Ordering::Relaxed).clamp(0.0, 1.0)
    }

    /// Overwrite the progress value. Intended for the producer side
    /// (loader, peak generator, etc.).
    pub fn set(&self, v: f32) {
        self.0.store(v.clamp(0.0, 1.0), Ordering::Relaxed);
    }

    /// Returns `true` once progress has reached `1.0`.
    pub fn is_done(&self) -> bool {
        self.get() >= 1.0
    }
}

#[derive(Debug, Clone, bevy_asset::Asset, bevy_reflect::TypePath)]
pub struct StreamingSample {
    pub path: PathBuf,
    pub sample_rate: u32,
    pub channels: u16,
    pub total_frames: u64,
    pub duration_seconds: f64,
    pub bits_per_sample: u16,
    /// Shared loader / post-processing progress. See [`StreamingProgress`].
    pub progress: StreamingProgress,
}

#[derive(Debug, thiserror::Error)]
pub enum StreamingSampleProbeError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("WAV decode error: {0}")]
    Wav(#[from] hound::Error),
}

impl StreamingSample {
    /// File extensions the Bevy asset loader recognises.
    pub const EXTENSIONS: &'static [&'static str] = &["wav"];

    /// Probe `path`, returning a locator + metadata. Progress is set to
    /// `1.0` on success.
    pub fn probe(path: &Path) -> Result<Self, StreamingSampleProbeError> {
        Self::probe_with_progress(path, StreamingProgress::new())
    }

    /// Like [`probe`](Self::probe), but writes progress into the supplied
    /// [`StreamingProgress`] so the caller can share it with UI code
    /// before the probe returns. Useful when the asset handle already
    /// exists (e.g. pre-created by the loader) and we want updates to
    /// land on the *same* atomic the UI is polling.
    pub fn probe_with_progress(
        path: &Path,
        progress: StreamingProgress,
    ) -> Result<Self, StreamingSampleProbeError> {
        progress.set(0.0);
        let reader = hound::WavReader::open(path)?;
        let spec = reader.spec();
        let total_frames = reader.duration() as u64;
        let sample_rate = spec.sample_rate;
        let channels = spec.channels;
        let duration_seconds = if sample_rate == 0 {
            0.0
        } else {
            total_frames as f64 / sample_rate as f64
        };
        progress.set(1.0);
        Ok(Self {
            path: path.to_path_buf(),
            sample_rate,
            channels,
            total_frames,
            duration_seconds,
            bits_per_sample: spec.bits_per_sample,
            progress,
        })
    }
}
