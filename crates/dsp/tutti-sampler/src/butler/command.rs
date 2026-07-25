//! Butler thread command enum and ID types.

use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct RegionId(pub u64);

use tutti_core::PlaybackRate;

use crate::clip::track_clip_reader::Direction;

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

    /// Reposition a live stream to an absolute file sample offset (timeline
    /// seek). `file_position` is the absolute file sample offset; the handler
    /// applies the channel's `pdc_preroll` before repositioning. Mirrors the
    /// PDC/loop-wrap click-free reposition (flush + seek + crossfade).
    SeekStream {
        channel_index: usize,
        file_position: u64,
    },

    /// Set varispeed (direction and speed) for a channel.
    ///
    /// [`PlaybackRate::UNITY`] is normal speed. Carrying the bounded type rather
    /// than a raw `f32` means the range is enforced where the value is built,
    /// not re-checked (or forgotten) in each handler.
    SetVarispeed {
        channel_index: usize,
        direction: Direction,
        speed: PlaybackRate,
    },

    /// Shutdown the butler thread.
    Shutdown,
}
