//! Butler thread command enum and ID types.

use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct RegionId(pub u64);

use tutti_core::PlaybackRate;

use crate::voice::types::Direction;

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
    /// [`LoopConfig`](super::plan::LoopConfig) (range + crossfade, plus the
    /// fade's lead-in, captured once) into the channel's `link.loop_config`,
    /// which the refill writes the ring by, and repositions the stream at its
    /// head so the change is heard at once (`handle_set_stream_loop`).
    SetStreamLoop {
        channel_index: usize,
        /// `(loop_start, loop_end)` in file samples.
        range: (u64, u64),
        crossfade_frames: usize,
    },
    /// Clear looping on a streaming channel — drop its `link.loop_config` so the
    /// stream plays through to the end without wrapping, repositioned at its
    /// head as a loop change is.
    ClearStreamLoop { channel_index: usize },

    /// Reposition a live stream to an absolute file sample offset (timeline
    /// seek). `file_position` is the absolute file sample offset; the handler
    /// applies the channel's `pdc_preroll` before repositioning. Mirrors the
    /// PDC and loop-change click-free reposition (flush + seek + crossfade).
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
