//! Non-blocking audio file import with progress polling.

use crossbeam_channel::Receiver;
use std::path::Path;
use std::sync::Arc;
use std::thread::JoinHandle;
use tutti_core::Wave;

/// Level-0 waveform peaks (256 samples per peak, min/max pairs).
pub type PeakData = Vec<(f32, f32)>;

/// Result type for background import thread completion.
type ImportResult = JoinHandle<std::result::Result<(Arc<Wave>, PeakData), String>>;

/// Metadata about the audio file, available before decode completes.
#[derive(Debug, Clone, Copy)]
pub struct WaveMetadata {
    pub total_frames: usize,
    pub sample_rate: u32,
}

pub enum ImportStatus {
    /// In progress. `peaks` contains accumulated level-0 peaks so far (may be None if no new peaks).
    /// `sample_delta` contains new ch-0 samples since last poll (for incremental STFT).
    Running {
        progress: f32,
        peaks: Option<PeakData>,
        metadata: Option<WaveMetadata>,
        sample_delta: Option<Vec<f32>>,
    },
    /// Decode complete. Includes the wave and final level-0 peaks.
    Complete {
        wave: Arc<Wave>,
        peaks: PeakData,
    },
    Failed(String),
    Pending,
}

/// Handle to a background audio file import.
/// Poll with [`Self::progress()`] each frame.
pub struct ImportHandle {
    progress_rx: Receiver<f32>,
    peaks_rx: Receiver<PeakData>,
    metadata_rx: Receiver<WaveMetadata>,
    samples_rx: Receiver<Vec<f32>>,
    thread: Option<ImportResult>,
    last_progress: Option<f32>,
    last_peaks: Option<PeakData>,
    metadata: Option<WaveMetadata>,
}

impl ImportHandle {
    pub(crate) fn new(
        progress_rx: Receiver<f32>,
        peaks_rx: Receiver<PeakData>,
        metadata_rx: Receiver<WaveMetadata>,
        samples_rx: Receiver<Vec<f32>>,
        thread: ImportResult,
    ) -> Self {
        Self {
            progress_rx,
            peaks_rx,
            metadata_rx,
            samples_rx,
            thread: Some(thread),
            last_progress: None,
            last_peaks: None,
            metadata: None,
        }
    }

    /// Poll for the latest import progress (non-blocking).
    pub fn progress(&mut self) -> ImportStatus {
        // Drain progress channel
        while let Ok(p) = self.progress_rx.try_recv() {
            self.last_progress = Some(p);
        }

        // Drain peaks channel — keep only the latest snapshot
        let mut new_peaks = false;
        while let Ok(peaks) = self.peaks_rx.try_recv() {
            self.last_peaks = Some(peaks);
            new_peaks = true;
        }

        // Drain metadata channel (sent once early in decode)
        while let Ok(meta) = self.metadata_rx.try_recv() {
            self.metadata = Some(meta);
        }

        // Drain samples channel — concatenate all deltas into one Vec
        let mut sample_delta: Option<Vec<f32>> = None;
        while let Ok(chunk) = self.samples_rx.try_recv() {
            sample_delta
                .get_or_insert_with(Vec::new)
                .extend_from_slice(&chunk);
        }

        // Check if thread finished
        if let Some(ref thread) = self.thread {
            if thread.is_finished() {
                let thread = self.thread.take().unwrap();
                return match thread.join() {
                    Ok(Ok((wave, peaks))) => ImportStatus::Complete { wave, peaks },
                    Ok(Err(e)) => ImportStatus::Failed(e),
                    Err(_) => ImportStatus::Failed("Import thread panicked".to_string()),
                };
            }
        } else {
            return ImportStatus::Failed("Import already consumed".to_string());
        }

        match self.last_progress {
            Some(p) => ImportStatus::Running {
                progress: p,
                peaks: if new_peaks {
                    self.last_peaks.take()
                } else {
                    None
                },
                metadata: self.metadata,
                sample_delta,
            },
            None => ImportStatus::Pending,
        }
    }

    pub fn wait(mut self) -> std::result::Result<Arc<Wave>, String> {
        if let Some(thread) = self.thread.take() {
            match thread.join() {
                Ok(Ok((wave, _peaks))) => Ok(wave),
                Ok(Err(e)) => Err(e),
                Err(_) => Err("Import thread panicked".to_string()),
            }
        } else {
            Err("Import already consumed".to_string())
        }
    }

    pub fn is_done(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }

    /// Start a background wave import on a dedicated thread.
    /// Decodes audio and streams waveform peaks incrementally.
    pub fn start(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_path_buf();
        let (progress_tx, progress_rx) = crossbeam_channel::bounded(64);
        let (peaks_tx, peaks_rx) = crossbeam_channel::bounded(64);
        let (metadata_tx, metadata_rx) = crossbeam_channel::bounded(1);
        let (samples_tx, samples_rx) = crossbeam_channel::bounded(64);

        let thread = std::thread::Builder::new()
            .name("tutti-load-wave".into())
            .spawn(move || {
                use thread_priority::{set_current_thread_priority, ThreadPriority};
                let _ = set_current_thread_priority(ThreadPriority::Max);
                let (wave, peaks) = Wave::load_with_peaks(
                    &path,
                    |p| {
                        let _ = progress_tx.try_send(p);
                    },
                    |peaks| {
                        let _ = peaks_tx.try_send(peaks);
                    },
                    |total_frames, sample_rate| {
                        let _ = metadata_tx.try_send(WaveMetadata {
                            total_frames,
                            sample_rate,
                        });
                    },
                    |samples| {
                        let _ = samples_tx.try_send(samples.to_vec());
                    },
                )
                .map_err(|e| e.to_string())?;
                Ok((Arc::new(wave), peaks))
            })
            .expect("failed to spawn wave load thread");

        Self::new(progress_rx, peaks_rx, metadata_rx, samples_rx, thread)
    }

    /// Immediately resolves with a cached wave (no peaks needed — they're already on disk).
    pub fn from_cached(wave: Arc<Wave>) -> Self {
        let (progress_tx, progress_rx) = crossbeam_channel::bounded(1);
        let (_peaks_tx, peaks_rx) = crossbeam_channel::bounded(1);
        let (_metadata_tx, metadata_rx) = crossbeam_channel::bounded(1);
        let (_samples_tx, samples_rx) = crossbeam_channel::bounded(1);
        let _ = progress_tx.send(1.0);
        let thread = std::thread::Builder::new()
            .name("tutti-load-wave".into())
            .spawn(move || Ok((wave, Vec::new())))
            .expect("failed to spawn thread");
        Self::new(progress_rx, peaks_rx, metadata_rx, samples_rx, thread)
    }
}
