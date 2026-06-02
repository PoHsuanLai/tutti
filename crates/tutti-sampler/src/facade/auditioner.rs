//! Low-latency file preview player. See [`Auditioner`].

use crate::units::{SamplerUnit, StreamingSamplerUnit};
use crate::Sampler;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tutti_core::{AtomicF32, Wave};

/// Reserved channel index for auditioner streaming.
/// Uses a high value to avoid collision with track channels (0, 1, 2...).
const AUDITIONER_CHANNEL: usize = usize::MAX - 1;

/// Threshold in samples: files shorter than this use in-memory playback.
/// ~10 seconds at 48kHz.
const IN_MEMORY_THRESHOLD: usize = 480_000;

enum PreviewMode {
    InMemory(SamplerUnit),
    Streaming,
}

/// Low-latency file preview player.
///
/// Built via [`Sampler::auditioner`](super::system::Sampler::auditioner).
/// Only one preview plays at a time — calling [`preview`](Self::preview)
/// while another file is playing stops the current preview first.
///
/// # Mode selection
///
/// Files shorter than ~10 seconds (or already in the LRU cache) play
/// **in-memory**: the file is decoded once and looped from RAM with
/// automatic sample-rate conversion. Longer files **stream from disk**
/// through a reserved internal channel.
///
/// # Example
///
/// ```no_run
/// # use std::sync::Arc;
/// # use tutti_sampler::Sampler;
/// # let sampler = Arc::new(Sampler::builder(48_000.0).build().unwrap());
/// let aud = sampler.auditioner();
/// aud.preview(std::path::Path::new("clip.wav")).unwrap();
/// aud.set_speed(1.25);
/// // ... later
/// aud.stop();
/// ```
pub struct Auditioner {
    sampler: Arc<Sampler>,
    mode: parking_lot::Mutex<Option<PreviewMode>>,
    current_path: parking_lot::Mutex<Option<PathBuf>>,
    playing: AtomicBool,
    gain: AtomicF32,
    speed: AtomicF32,
    session_sample_rate: f64,
}

impl Auditioner {
    pub(crate) fn new(sampler: Arc<Sampler>) -> Self {
        let sr = sampler.sample_rate();
        Self {
            sampler,
            mode: parking_lot::Mutex::new(None),
            current_path: parking_lot::Mutex::new(None),
            playing: AtomicBool::new(false),
            gain: AtomicF32::new(1.0),
            speed: AtomicF32::new(1.0),
            session_sample_rate: sr,
        }
    }

    /// Preview a file.
    ///
    /// Stops any current preview first. Short or already-cached files
    /// play in-memory; longer files stream from disk via butler.
    pub fn preview(&self, file_path: &Path) -> crate::Result<()> {
        self.stop();

        let path = file_path.to_path_buf();
        let cache = self.sampler.butler_cache();

        if let Some(wave) = cache.get(&path) {
            self.start_in_memory(wave, &path);
        } else {
            let wave = Arc::new(
                Wave::load(file_path).map_err(|e| crate::Error::SampleNotFound(e.to_string()))?,
            );
            cache.insert(path.clone(), wave.clone());

            if wave.len() <= IN_MEMORY_THRESHOLD {
                self.start_in_memory(wave, &path);
            } else {
                self.start_streaming(&path);
            }
        }

        Ok(())
    }

    fn start_in_memory(&self, wave: Arc<Wave>, path: &Path) {
        let mut unit = SamplerUnit::with_settings(
            wave,
            self.gain.load(Ordering::Acquire),
            self.speed.load(Ordering::Acquire),
            false,
        );
        unit.set_session_sample_rate(self.session_sample_rate);
        unit.trigger();

        self.enter(PreviewMode::InMemory(unit), path);
    }

    fn start_streaming(&self, path: &Path) {
        let ch = self.sampler.channel(AUDITIONER_CHANNEL);
        ch.play(path).start();

        let speed = self.speed.load(Ordering::Acquire);
        if speed != 1.0 {
            ch.speed(speed);
        }

        self.enter(PreviewMode::Streaming, path);
    }

    /// Install a new preview mode and mark playing.
    fn enter(&self, mode: PreviewMode, path: &Path) {
        *self.mode.lock() = Some(mode);
        *self.current_path.lock() = Some(path.to_path_buf());
        self.playing.store(true, Ordering::Release);
    }

    /// Stop the current preview, if any.
    pub fn stop(&self) {
        if let Some(mode) = self.mode.lock().take() {
            match mode {
                PreviewMode::InMemory(unit) => unit.stop(),
                PreviewMode::Streaming => {
                    self.sampler.channel(AUDITIONER_CHANNEL).stop();
                }
            }
        }
        *self.current_path.lock() = None;
        self.playing.store(false, Ordering::Release);
    }

    /// `true` if a preview is currently playing.
    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Acquire)
    }

    /// Set linear playback gain. Clamped to non-negative.
    pub fn set_gain(&self, gain: f32) {
        self.gain.store(gain.max(0.0), Ordering::Release);
    }

    /// Current linear playback gain.
    pub fn gain(&self) -> f32 {
        self.gain.load(Ordering::Acquire)
    }

    /// Set playback speed. Clamped to `[0.25, 4.0]`.
    pub fn set_speed(&self, speed: f32) {
        let clamped = speed.clamp(0.25, 4.0);
        self.speed.store(clamped, Ordering::Release);
        let mode = self.mode.lock();
        if let Some(PreviewMode::Streaming) = mode.as_ref() {
            self.sampler.channel(AUDITIONER_CHANNEL).speed(clamped);
        }
    }

    /// Current playback speed.
    pub fn speed(&self) -> f32 {
        self.speed.load(Ordering::Acquire)
    }

    /// Path of the file currently being previewed, if any.
    pub fn current_path(&self) -> Option<PathBuf> {
        self.current_path.lock().clone()
    }

    /// Duration of the current preview, in seconds.
    pub fn duration(&self) -> Option<f64> {
        let mode = self.mode.lock();
        match mode.as_ref()? {
            PreviewMode::InMemory(unit) => Some(unit.duration_seconds()),
            PreviewMode::Streaming => {
                let path = self.current_path.lock();
                let path = path.as_ref()?;
                let wave = self.sampler.butler_cache().get(path)?;
                Some(wave.duration())
            }
        }
    }

    /// Clone of the in-memory `SamplerUnit` for graph integration.
    /// `None` if the current preview is streaming from disk.
    pub fn in_memory_unit(&self) -> Option<SamplerUnit> {
        let mode = self.mode.lock();
        match mode.as_ref()? {
            PreviewMode::InMemory(unit) => Some(unit.clone()),
            PreviewMode::Streaming => None,
        }
    }

    /// `StreamingSamplerUnit` for graph integration when the preview is
    /// streaming from disk. `None` if the current preview is in-memory.
    pub fn streaming_unit(&self) -> Option<StreamingSamplerUnit> {
        let mode = self.mode.lock();
        match mode.as_ref()? {
            PreviewMode::Streaming => self.sampler.channel(AUDITIONER_CHANNEL).streaming_unit(),
            PreviewMode::InMemory(_) => None,
        }
    }
}
