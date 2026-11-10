//! Opening a stream: the request ([`Placement`]), the reply ([`Opening`],
//! [`Open`], [`OpenError`]) and what the reply carries: the control side's
//! [`Stream`] and the audio side's one [`StreamReader`].
//!
//! The ring's width comes from the file's header, so the ring is built on the
//! streamer's thread after the probe, and a file that cannot seek is decoded
//! whole before it can be. The reader therefore cannot be the return value of
//! `open`; it is the **reply** of one open: a cell the streamer writes once
//! and [`Opening::poll`] reads once, consuming the `Opening`. There is nothing
//! to look a reader up by, so a reader cannot be taken twice (`poll` takes
//! `self`), cannot be taken before the stream exists (`Pending` carries the
//! `Opening` back), and cannot be another open's (the reply is this open's;
//! doc 025 §1.3, B3).

use std::sync::Arc;

use crossbeam_channel::{Receiver, TryRecvError};
use tutti_rt::{PosReader, Reader};
use tutti_types::{ChannelLayout, SamplePosition, SampleRate};

use super::command::StreamId;
use super::control::StreamOrigin;
use super::loops::RingMap;
use super::prefetch::SharedReader;
use super::rt_state::RtState;
use super::stream::{Stream, StreamerPort};
use crate::PlaceError;
use crate::voice::interp::Place;

/// Where in the file a stream's reader starts, in file frames.
///
/// A position is refused, never clamped: a negative or non-finite one would
/// play the wrong audio with no error. A fractional position is floored to the
/// frame it falls in when the stream's window is placed.
///
/// # Raw
///
/// `new` and `get` speak [`SamplePosition`]; serde and `TryFrom` speak the bare
/// `f64` it wraps (a serde boundary, where the unit type stops).
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(try_from = "f64", into = "f64")
)]
pub struct Placement(SamplePosition);

impl Placement {
    /// The head of the file.
    pub const HEAD: Self = Self(SamplePosition(0.0));

    /// A start at `start` frames into the file; `None` when `start` is
    /// negative or not finite.
    #[must_use]
    pub fn new(start: SamplePosition) -> Option<Self> {
        (start.get().is_finite() && start.get() >= 0.0).then_some(Self(start))
    }

    /// The position, as given.
    #[must_use]
    pub fn get(self) -> SamplePosition {
        self.0
    }

    /// The frame the stream's window starts at: the position floored.
    pub(crate) fn frame(self) -> u64 {
        // Finite and non-negative by construction; a huge value saturates,
        // and the ring parks a window at its position limit.
        self.0.get().floor() as u64
    }
}

/// A position [`Placement`] refuses, carrying the refused raw.
#[derive(Clone, Copy, Debug, PartialEq, thiserror::Error)]
#[error("{0} is not a position in a file: it must be finite and not negative")]
pub struct RefusedPlacement(pub f64);

impl TryFrom<f64> for Placement {
    type Error = RefusedPlacement;

    fn try_from(raw: f64) -> Result<Self, RefusedPlacement> {
        Self::new(SamplePosition(raw)).ok_or(RefusedPlacement(raw))
    }
}

impl From<Placement> for f64 {
    fn from(placement: Placement) -> f64 {
        placement.0.get()
    }
}

/// Why an open failed: what each used to leave as a stream that never started.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum OpenError {
    /// The file could not be opened or read.
    #[error("the file could not be opened: {0}")]
    Io(std::io::ErrorKind),
    /// The file is not audio this build can decode.
    #[error("the file is not audio this build can decode")]
    Format,
    /// The file's channels cannot be placed into the voice's width: a channel
    /// no speaker names (a second LFE, a wide or high speaker), or a
    /// narrowing with an unordered end (a 5.1 file into 5 channels).
    #[error("the file cannot be placed: {0}")]
    Placement(#[source] PlaceError),
    /// The streamer thread is gone, so the open was never answered.
    #[error("the streamer thread is gone")]
    StreamerGone,
}

/// What the streamer answers an open with: everything the audio side's
/// [`StreamReader`] is built from but the registered map reader, which the
/// control thread registers ([`Opening::poll`]).
pub(crate) struct Opened {
    pub(crate) id: StreamId,
    pub(crate) ring: SharedReader,
    pub(crate) pos: PosReader,
    pub(crate) rt_state: Arc<RtState>,
    pub(crate) file_rate: SampleRate,
    pub(crate) origin: StreamOrigin,
    pub(crate) place: Place,
    pub(crate) width: ChannelLayout,
}

impl std::fmt::Debug for Opened {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Opened")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// An open the streamer has not answered yet.
///
/// [`poll`](Self::poll) consumes it: the stream and its reader are the reply,
/// and a reply is received once.
///
/// ```compile_fail
/// # use tutti_sampler::Opening;
/// # fn twice(opening: Opening) {
/// let _ = opening.poll();
/// let _ = opening.poll(); // `poll` took `opening` by value
/// # }
/// ```
///
/// Its compiling twin polls the `Opening` it is handed back instead:
///
/// ```
/// # use tutti_sampler::{Open, Opening};
/// # fn again(opening: Opening) {
/// if let Open::Pending(opening) = opening.poll() {
///     let _ = opening.poll();
/// }
/// # }
/// ```
///
/// Dropping an `Opening` abandons the open: the streamer closes the stream it
/// opened.
#[derive(Debug)]
#[must_use = "an open nobody polls is abandoned"]
pub struct Opening {
    id: StreamId,
    reply: Receiver<Result<Opened, OpenError>>,
    /// The streamer's command queue, which the stream it answers with sends
    /// on.
    port: StreamerPort,
}

impl Opening {
    pub(crate) fn new(
        id: StreamId,
        reply: Receiver<Result<Opened, OpenError>>,
        port: StreamerPort,
    ) -> Self {
        Self { id, reply, port }
    }

    /// The stream this open is for.
    pub fn id(&self) -> StreamId {
        self.id
    }

    /// Whether the streamer has answered. Never blocks; control thread.
    ///
    /// `Ready` registers the reader's control cell before it returns (that
    /// allocates, so it is here and not on the audio thread).
    pub fn poll(self) -> Open {
        match self.reply.try_recv() {
            Ok(Ok(opened)) => {
                Open::Ready(Stream::new(opened.id, self.port), StreamReader::new(opened))
            }
            Ok(Err(error)) => Open::Failed(error),
            Err(TryRecvError::Empty) => Open::Pending(self),
            // The streamer dropped the reply unanswered: its thread is gone.
            Err(TryRecvError::Disconnected) => Open::Failed(OpenError::StreamerGone),
        }
    }
}

/// An open's answer.
#[derive(Debug)]
#[must_use = "a dropped `Pending` abandons the open; a dropped `Stream` closes it"]
pub enum Open {
    /// Not answered yet: poll the `Opening` again.
    Pending(Opening),
    /// The stream is open: the handle is the control thread's, and the reader
    /// is the voice's, by value. Dropping the [`Stream`] ends the stream.
    Ready(Stream, StreamReader),
    /// The stream did not open, and will not.
    Failed(OpenError),
}

/// The audio side's one reader of a stream: the ring's position reader and
/// its control cell, with what places the file into a voice.
///
/// By value and not [`Clone`]: it is moved into the voice
/// ([`DiskVoice::from_reader`](crate::DiskVoice::from_reader)), and a
/// stream has no other.
///
/// ```compile_fail
/// # use tutti_sampler::StreamReader;
/// # fn two(reader: StreamReader) {
/// let second = reader.clone(); // a stream has one reader
/// # }
/// ```
///
/// Its compiling twin only borrows it:
///
/// ```
/// # use tutti_sampler::StreamReader;
/// # fn one(reader: StreamReader) {
/// let _again = &reader;
/// # }
/// ```
pub struct StreamReader {
    id: StreamId,
    pub(crate) ring: SharedReader,
    pub(crate) pos: PosReader,
    /// Registered on the control side ([`Opening::poll`]): registering
    /// allocates.
    pub(crate) map: Reader<RingMap>,
    pub(crate) rt_state: Arc<RtState>,
    pub(crate) file_rate: SampleRate,
    pub(crate) origin: StreamOrigin,
    pub(crate) place: Place,
    pub(crate) width: ChannelLayout,
}

impl std::fmt::Debug for StreamReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamReader")
            .field("id", &self.id)
            .field("width", &self.width)
            .finish_non_exhaustive()
    }
}

impl StreamReader {
    fn new(opened: Opened) -> Self {
        Self {
            id: opened.id,
            map: opened.ring.map_reader(),
            ring: opened.ring,
            pos: opened.pos,
            rt_state: opened.rt_state,
            file_rate: opened.file_rate,
            origin: opened.origin,
            place: opened.place,
            width: opened.width,
        }
    }

    /// The stream this reads: the ring of its own open, never another's.
    pub fn id(&self) -> StreamId {
        self.id
    }

    /// The file's own sample rate, from its header.
    pub fn file_rate(&self) -> SampleRate {
        self.file_rate
    }

    /// The width of the voice this reader feeds.
    pub fn channels(&self) -> ChannelLayout {
        self.width
    }
}

#[cfg(test)]
mod tests;
