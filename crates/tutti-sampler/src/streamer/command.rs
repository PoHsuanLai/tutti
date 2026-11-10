//! Streamer thread command enum and the stream's identity.

use std::path::PathBuf;

use crossbeam_channel::Sender;
use tutti_types::ChannelLayout;

use super::open::Opened;
use crate::OpenError;

/// One open stream's identity: minted on the control side when the open is
/// asked for ([`DiskStreamer::open`](crate::DiskStreamer::open)), never
/// reused, and what every command about the stream names.
///
/// Opaque: the only way to hold one is to be handed it
/// ([`StreamReader::id`](crate::StreamReader::id)), so a command can only name
/// a stream that was opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StreamId(pub(crate) u64);

impl std::fmt::Display for StreamId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "stream {}", self.0)
    }
}

/// Where the streamer answers an open: one cell, written once by the streamer
/// and read once by [`Opening::poll`](crate::Opening::poll).
pub(crate) type Reply = Sender<Result<Opened, OpenError>>;

/// Command sent to the Streamer thread.
#[derive(Debug)]
pub(crate) enum StreamerCommand {
    /// Open `file_path` as stream `id`, its window starting at frame `start`,
    /// and answer on `reply`.
    Open {
        id: StreamId,
        file_path: PathBuf,
        start: u64,
        /// The voice's width, or the file's own when `None`.
        width: Option<ChannelLayout>,
        reply: Reply,
    },
    /// Stop a stream: its ring and region go.
    StopStreaming { stream: StreamId },

    /// Enable looping on a stream. The streamer builds a
    /// [`LoopConfig`](super::plan::LoopConfig) (range + crossfade, plus the
    /// fade's lead-in, captured once) into the stream's `link.loop_config`,
    /// which the refill writes the ring by, and repositions the stream at its
    /// head so the change is heard at once (`handle_set_stream_loop`). A
    /// stream the streamer no longer holds (stopped) ignores it.
    SetStreamLoop {
        stream: StreamId,
        /// `(loop_start, loop_end)` in file samples.
        range: (u64, u64),
        crossfade_frames: usize,
    },
    /// Clear looping on a stream — drop its `link.loop_config` so the
    /// stream plays through to the end without wrapping, repositioned at its
    /// head as a loop change is.
    ClearStreamLoop { stream: StreamId },

    /// Shutdown the streamer thread.
    Shutdown,
}
