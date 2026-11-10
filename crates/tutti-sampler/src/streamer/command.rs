//! Streamer thread command enum and ID types.

use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct RegionId(pub u64);

/// Command sent to the Streamer thread.
#[derive(Debug)]
pub(crate) enum StreamerCommand {
    /// Stream an audio file to a channel buffer.
    StreamAudioFile {
        channel_index: usize,
        file_path: PathBuf,
        offset_samples: usize,
    },
    /// Stop streaming for a channel.
    StopStreaming { channel_index: usize },

    /// Enable looping on a streaming channel. The streamer builds a
    /// [`LoopConfig`](super::plan::LoopConfig) (range + crossfade, plus the
    /// fade's lead-in, captured once) into the channel's `link.loop_config`,
    /// which the refill writes the ring by, and repositions the stream at its
    /// head so the change is heard at once (`handle_set_stream_loop`).
    SetStreamLoop {
        channel_index: usize,
        /// The stream it is meant for, when the sender knows it (a voice
        /// pool's handle, from the voice's ring): a channel streaming
        /// anything else (its voice's stream stopped, the channel restarted
        /// on another file) ignores it. `None` loops whatever the channel
        /// streams, as every other command addresses it.
        stream: Option<RegionId>,
        /// `(loop_start, loop_end)` in file samples.
        range: (u64, u64),
        crossfade_frames: usize,
    },
    /// Clear looping on a streaming channel — drop its `link.loop_config` so the
    /// stream plays through to the end without wrapping, repositioned at its
    /// head as a loop change is.
    ClearStreamLoop {
        channel_index: usize,
        /// As `SetStreamLoop`'s.
        stream: Option<RegionId>,
    },

    /// Shutdown the streamer thread.
    Shutdown,
}
