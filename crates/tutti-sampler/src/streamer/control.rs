//! What a stream plays ([`StreamFile`], what an export fork reads), and the error
//! a command send reports.
//!
//! The control side of a stream is [`Stream`](crate::Stream); the reader it
//! hands its voice is the reply of its open ([`Opening`](crate::Opening)).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tutti_codec::Wave;
use tutti_types::SampleRate;

/// The streamer thread is gone, so a stream-control command was discarded.
///
/// There is one failure mode and it is permanent: a blocking send waits
/// out a full queue and fails only once the queue is closed, which happens
/// when the streamer thread exits or its streamer is dropped. Every later
/// command fails the same way, so a caller that sees this should stop rather
/// than retry.
///
/// It is an error type rather than a silent `let _ =` because swallowing it
/// leaves a dead streamer accepting `Stream`/`Loop`/`Stop` indefinitely and
/// doing nothing — indistinguishable, from the caller's side, from working
/// playback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamerGone;

impl core::fmt::Display for StreamerGone {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "streamer thread is gone; the command was discarded")
    }
}

impl std::error::Error for StreamerGone {}

/// The file a stream plays: where it is, its own rate and, when the streamer
/// holds it decoded whole, the wave.
///
/// Immutable, built once when the stream opens and shared by the
/// [`Stream`](crate::Stream) ([`Stream::file`](crate::Stream::file)) and the
/// stream's [`StreamReader`](crate::StreamReader), so a fork of the voice (made
/// from the node, which holds the reader and not the handle) plays the same
/// material with no streamer at all. What changes while a stream runs is not
/// here: the loop as applied and the direction are the ring's published
/// control, and a stream that has ended is its status.
///
/// A live voice may be the last holder, and its drop can land where the graph
/// frees units, never the audio thread; what it frees is a path and a wave the
/// streamer's writer shares.
#[derive(Clone)]
pub struct StreamFile {
    path: PathBuf,
    file_rate: SampleRate,
    resident: Option<Arc<Wave>>,
}

impl StreamFile {
    pub(crate) fn new(path: PathBuf, file_rate: SampleRate, resident: Option<Arc<Wave>>) -> Self {
        Self {
            path,
            file_rate,
            resident,
        }
    }

    /// The file the stream was opened on. A fork re-opens it by this path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The file's own sample rate, from its header.
    #[must_use]
    pub fn file_rate(&self) -> SampleRate {
        self.file_rate
    }

    /// The whole file, when the streamer holds it decoded: a file that cannot
    /// seek (mp3, ogg) is decoded whole to be streamed, and a fork reads that
    /// wave in place rather than decode it a second time. `None` for a file the
    /// streamer reads a piece at a time.
    #[must_use]
    pub fn resident(&self) -> Option<&Arc<Wave>> {
        self.resident.as_ref()
    }
}

impl std::fmt::Debug for StreamFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamFile")
            .field("path", &self.path)
            .field("file_rate", &self.file_rate)
            .field("resident", &self.resident.is_some())
            .finish()
    }
}
