//! Butler thread command enum and ID types.

use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct RegionId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CaptureId(pub u64);

/// Monotonic CaptureId generator scoped to a `Sampler` instance.
///
/// Cloneable — hand a clone to any subsystem that needs to mint capture IDs
/// on behalf of the same system (e.g. `Recorder`).
#[derive(Debug, Clone)]
pub(crate) struct CaptureIdGen {
    next: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl CaptureIdGen {
    pub(crate) fn new() -> Self {
        Self {
            next: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1)),
        }
    }

    pub(crate) fn mint(&self) -> CaptureId {
        CaptureId(self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
    }
}

use super::prefetch::CaptureReader;
use super::varispeed::PlayDirection;

/// Command sent to the Butler thread.
#[derive(Debug)]
pub(crate) enum ButlerCommand {
    /// Stream an audio file to a channel buffer.
    StreamAudioFile {
        channel_index: usize,
        file_path: PathBuf,
        offset_samples: usize,
    },
    /// Stop streaming for a channel.
    StopStreaming { channel_index: usize },

    /// Enable looping on a streaming channel. The butler builds a
    /// [`LoopConfig`](super::plan::LoopConfig) (range + crossfade, plus a
    /// pre-captured fadein buffer) into the channel's `link.loop_config`, which
    /// the refill/loop-wrap machinery then respects.
    SetStreamLoop {
        channel_index: usize,
        /// `(loop_start, loop_end)` in file samples.
        range: (u64, u64),
        crossfade_samples: usize,
    },
    /// Clear looping on a streaming channel — drop its `link.loop_config` so the
    /// stream plays through to the end without wrapping.
    ClearStreamLoop { channel_index: usize },

    /// Set varispeed (direction and speed) for a channel. `speed = 1.0` is normal.
    SetVarispeed {
        channel_index: usize,
        direction: PlayDirection,
        speed: f32,
    },

    /// Register a capture buffer consumer (Butler will read from this and write to disk).
    RegisterCapture {
        capture_id: CaptureId,
        consumer: CaptureReader,
        file_path: PathBuf,
        sample_rate: f64,
        channels: usize,
        format: crate::capture::CaptureFormat,
    },
    /// Remove a capture buffer (finalize and close file).
    RemoveCapture(CaptureId),
    /// Flush a single capture buffer to disk.
    Flush(CaptureId),

    /// Shutdown the butler thread.
    Shutdown,
}
