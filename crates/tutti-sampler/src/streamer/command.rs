//! Streamer thread command enum and the stream's identity.

use std::path::PathBuf;
use std::sync::Arc;

use crossbeam_channel::Sender;
use tutti_codec::Wave;
use tutti_types::ChannelLayout;

use super::open::Opened;
use crate::OpenError;

/// One open stream's identity: minted on the control side when the open is
/// asked for ([`DiskStreamer::open`](crate::DiskStreamer::open)), never
/// reused, and what every command about the stream names.
///
/// Opaque: the only way to hold one is to be handed it
/// ([`Stream::id`](crate::Stream::id), [`StreamReader::id`](crate::StreamReader::id)),
/// so a command can only name a stream that was opened.
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

/// What the decode thread made of a file: the whole wave, or why there is none.
pub(crate) struct Decoded(pub(crate) Result<Arc<Wave>, OpenError>);

impl std::fmt::Debug for Decoded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            Ok(wave) => write!(f, "Decoded({} frames)", wave.len()),
            Err(error) => write!(f, "Decoded({error})"),
        }
    }
}

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
    /// The decode thread finished the whole-file decode an open of a file that
    /// cannot seek waits on: the streamer builds the stream's ring from it and
    /// answers the open (doc 025 decision 21). Sent by the decode thread; an
    /// id the streamer holds no pending open for is ignored.
    #[cfg_attr(
        not(codec),
        allow(dead_code, reason = "sent only by the codec-gated decode lane")
    )]
    Decoded { id: StreamId, wave: Decoded },
    /// End a stream: its record, and with it its ring and region, goes in the
    /// step that handles this. Sent by `Drop for Stream`. A stream the
    /// streamer no longer holds ignores it.
    Close { stream: StreamId },

    /// Enable looping on a stream. The streamer captures the loop (range and
    /// crossfade, plus the fade's lead-in, read once) into the stream's ring
    /// mapping and records the loop as applied (`handle_loop`). A stream the
    /// streamer no longer holds ignores it.
    SetStreamLoop {
        stream: StreamId,
        /// `(loop_start, loop_end)` in file samples.
        range: (u64, u64),
        crossfade_frames: usize,
    },
    /// Clear looping on a stream, so it plays through to the end without
    /// wrapping, repositioned as a loop change is. A stream the streamer no
    /// longer holds ignores it.
    ClearStreamLoop { stream: StreamId },

    /// Shutdown the streamer thread.
    Shutdown,
}
