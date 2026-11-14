//! The reassembler of inbound chunks: the Property Exchange messages being
//! received, each collected in order until its last chunk, as a value stepped
//! by chunks and by time.
//!
//! Nothing here reads a clock or sends: [`PeReassembler::push`] takes the time
//! a chunk arrived, [`PeReassembler::expire`] the time now, and what the spec
//! says to send comes back as a [`PeSignal`].

use std::num::{NonZeroU8, NonZeroUsize};
use std::time::Instant;

use super::chunk::{ChunkNo, ChunkPosition, ChunkTotal, PeChunk, PeRequestId};
use super::flow::{PE_TIMEOUT, PeMessageKind, PeSignal, PeTerminator};
use super::header::{PeHeader, PeHeaderError, PeSubscription};
use super::limits::NotInRange;
use crate::Muid;
use crate::TimeoutWait;

/// The most messages received at once from all peers: 1 to 128, the Request IDs
/// there are (M2-103 §5.3). A message past it is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "u8", into = "u8"))]
pub struct StreamLimit(NonZeroU8);

impl StreamLimit {
    /// The most there can be, 128.
    pub const MAX: u8 = 128;

    /// The limit, or `None` for 0 or past 128.
    #[must_use]
    pub const fn new(raw: u8) -> Option<Self> {
        match NonZeroU8::new(raw) {
            Some(n) if raw <= Self::MAX => Some(Self(n)),
            _ => None,
        }
    }

    /// The limit.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0.get()
    }
}

/// The most bytes buffered across all the messages being received, Header Data
/// and Property Data together.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "usize", into = "usize"))]
pub struct BufferLimit(NonZeroUsize);

impl BufferLimit {
    /// The limit, or `None` for 0.
    #[must_use]
    pub const fn new(bytes: usize) -> Option<Self> {
        match NonZeroUsize::new(bytes) {
            Some(n) => Some(Self(n)),
            None => None,
        }
    }

    /// The limit in bytes.
    #[must_use]
    pub const fn get(self) -> usize {
        self.0.get()
    }
}

impl TryFrom<u8> for StreamLimit {
    type Error = NotInRange<u8>;

    fn try_from(raw: u8) -> Result<Self, NotInRange<u8>> {
        Self::new(raw).ok_or(NotInRange {
            what: "stream limit",
            raw,
        })
    }
}

impl From<StreamLimit> for u8 {
    fn from(limit: StreamLimit) -> u8 {
        limit.get()
    }
}

impl TryFrom<usize> for BufferLimit {
    type Error = NotInRange<usize>;

    fn try_from(raw: usize) -> Result<Self, NotInRange<usize>> {
        Self::new(raw).ok_or(NotInRange {
            what: "buffer limit",
            raw,
        })
    }
}

impl From<BufferLimit> for usize {
    fn from(limit: BufferLimit) -> usize {
        limit.get()
    }
}

/// What a [`PeReassembler`] holds at most.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PeReassemblyLimits {
    /// Messages at once.
    pub streams: StreamLimit,
    /// Bytes buffered.
    pub bytes: BufferLimit,
}

/// What a [`PeReassembler`] holds now.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct PeBuffered {
    /// Messages being received.
    pub streams: usize,
    /// Bytes buffered.
    pub bytes: usize,
}

/// Whether a transaction uses Flow Control: the Receiver acknowledges each chunk
/// before the Sender sends the next (M2-103 §15).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PeFlowControl {
    /// No acknowledgements.
    Off,
    /// An ACK for each chunk.
    On,
}

/// A message received whole.
#[derive(Clone, Debug, PartialEq)]
pub struct PeReceived {
    /// The device that sent it.
    pub peer: Muid,
    /// Which message it is.
    pub kind: PeMessageKind,
    /// Its Request ID.
    pub request_id: PeRequestId,
    /// The Header Data, read.
    pub header: PeHeader,
    /// The Property Data of all chunks, in order, still in the header's encoding.
    pub body: Vec<u8>,
}

/// What a chunk did to the message it belongs to.
#[derive(Clone, Debug, PartialEq)]
pub enum PeStep {
    /// The chunk is taken, or is a repeat of the last one and ignored; more is
    /// expected.
    Pending {
        /// What to send, under Flow Control.
        send: Option<PeSignal>,
    },
    /// The chunk was the last and the message is whole.
    Complete {
        /// The message.
        message: Box<PeReceived>,
        /// What to send, under Flow Control.
        send: Option<PeSignal>,
    },
    /// The sender abandoned the message with a final chunk numbered 0 (M2-101
    /// §8.3): its data is dropped, and there is nothing to send.
    Abandoned,
}

/// Why a chunk is refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PeReassemblyError {
    /// A first chunk for a Request ID whose message is still being received
    /// (M2-103 §5.3: an ID is reused only after its transaction is complete).
    #[error("request id {} is in use", .0.get())]
    RequestIdInUse(PeRequestId),
    /// A chunk after the first, or an abandoning one, for a message that was
    /// never started or has ended.
    #[error("no message of request id {} is being received", .0.get())]
    NoMessage(PeRequestId),
    /// A chunk that is neither the next nor a repeat of the last.
    #[error("chunk {} arrived where {} was due", .got.get(), .expected.get())]
    OutOfSequence {
        /// The number due.
        expected: ChunkNo,
        /// The number that came.
        got: ChunkNo,
    },
    /// A Number of Chunks in Message that disagrees with the one before it, or is
    /// smaller than the chunk's own number.
    #[error("the chunk count disagrees with the message's")]
    CountMismatch,
    /// The message would be the one past the [`StreamLimit`].
    #[error("{} messages are already being received", .0.get())]
    TooManyStreams(StreamLimit),
    /// The chunk would take the buffer past its [`BufferLimit`].
    #[error("the message would take more than {} buffered bytes", .0.get())]
    Overflow(BufferLimit),
    /// The Header Data of a complete first chunk is not a header.
    #[error("the Header Data is not valid: {0}")]
    Header(#[from] PeHeaderError),
}

/// A refused chunk, and the signal the spec has the receiver send for it, if
/// any.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{error}")]
pub struct PeRefused {
    /// Why.
    pub error: PeReassemblyError,
    /// What to send.
    pub send: Option<PeSignal>,
}

/// A message whose next chunk did not come in time (M2-103 §12.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeExpired {
    /// The device sending it.
    pub peer: Muid,
    /// Which message it is.
    pub kind: PeMessageKind,
    /// What to send: [`PeSignal::TimedOut`], NAK `0x42` (§12.2.3). The message
    /// is dropped, the transaction concluded.
    pub send: PeSignal,
}

/// A message partly received: what [`PeReassembler::held`] shows of it.
#[derive(Clone, Copy, Debug)]
pub struct PeHeld<'a> {
    /// The Header Data, read from the first chunk.
    pub header: &'a PeHeader,
    /// The Property Data of the chunks so far, as sent: before its encoding is
    /// undone.
    pub data: &'a [u8],
}

/// A message being received.
#[derive(Debug)]
struct Stream {
    peer: Muid,
    kind: PeMessageKind,
    request_id: PeRequestId,
    /// The Header Data, read from the first chunk.
    header: PeHeader,
    data: Vec<u8>,
    header_len: usize,
    /// The total the first chunk declared.
    declared: ChunkTotal,
    /// The number of the last chunk taken.
    last: ChunkNo,
    deadline: Instant,
    flow: PeFlowControl,
}

impl Stream {
    fn is(&self, peer: Muid, kind: PeMessageKind, request_id: PeRequestId) -> bool {
        self.peer == peer && self.kind == kind && self.request_id == request_id
    }

    fn bytes(&self) -> usize {
        self.header_len + self.data.len()
    }
}

/// The inbound Property Exchange messages from all peers, each collected from
/// its chunks (M2-101 §8.3, M2-103 §5.2).
///
/// A message is keyed by the peer's MUID, its Request ID and its
/// [`PeMessageKind`]: the two directions of a transaction use the Request ID
/// independently, so an inquiry and a reply of the same ID are different
/// messages (M2-103 §5.3).
///
/// A message ends when its last chunk comes, when a chunk numbered 0 abandons it,
/// when a chunk is out of sequence, past a limit or malformed, and when no chunk
/// follows within [`PE_TIMEOUT`]. The reassembler holds no clock: the caller
/// drives it with [`push`](Self::push) at the time a chunk arrived and with
/// [`expire`](Self::expire) at [`next_deadline`](Self::next_deadline). Only the
/// messages that have a first chunk are held; the wait for a first chunk is the
/// transaction's.
#[derive(Debug)]
pub struct PeReassembler {
    limits: PeReassemblyLimits,
    streams: Vec<Stream>,
}

impl PeReassembler {
    /// A reassembler that holds at most `limits`.
    #[must_use]
    pub const fn new(limits: PeReassemblyLimits) -> Self {
        Self {
            limits,
            streams: Vec::new(),
        }
    }

    /// What is buffered.
    #[must_use]
    pub fn buffered(&self) -> PeBuffered {
        PeBuffered {
            streams: self.streams.len(),
            bytes: self.streams.iter().map(Stream::bytes).sum(),
        }
    }

    /// The instant the earliest message times out, or `None` when none is held.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Instant> {
        self.streams.iter().map(|s| s.deadline).min()
    }

    /// Takes a chunk that arrived at `now`.
    ///
    /// The chunks of a message come in order, from 1, and the first carries the
    /// header. Chunk `n` of a message that has `n - 1` is taken. A repeat of the
    /// last chunk is ignored and, under Flow Control, acknowledged again, since
    /// the sender repeats a chunk whose ACK it did not get (M2-103 §15.5). The
    /// chunk that ends the message is the one whose Number of Chunks in Message
    /// equals its number; a message that declared a count may end earlier by
    /// naming a smaller one, and one that declared 0 (unknown) ends by naming its
    /// count (M2-101 §8.3). A message ends abandoned at a chunk numbered 0.
    ///
    /// `reply_flow` says whether the transaction uses Flow Control, for the first
    /// chunk of a reply, whose header does not say; an inquiry's header says,
    /// with `flowControl`, and either one turns it on (M2-103 §15). Under Flow
    /// Control each chunk is acknowledged with [`PeSignal::SendNext`], and the
    /// last too when it is a Get reply, which no other message acknowledges
    /// (§15.2, §15.3).
    ///
    /// A message whose next chunk is due by `now` is dropped first, so its Request
    /// ID is free again.
    ///
    /// # Errors
    ///
    /// A [`PeRefused`]: the message the chunk belongs to is dropped when the
    /// chunk is out of sequence, has a count that disagrees, passes the buffer
    /// limit or has a bad header, and is kept when the chunk is a first chunk for
    /// an ID in use or comes past the [`StreamLimit`].
    pub fn push(
        &mut self,
        peer: Muid,
        kind: PeMessageKind,
        chunk: PeChunk,
        reply_flow: PeFlowControl,
        now: Instant,
    ) -> Result<PeStep, PeRefused> {
        let id = chunk.request_id();
        self.streams
            .retain(|s| !(s.is(peer, kind, id) && s.deadline <= now));
        let at = self.streams.iter().position(|s| s.is(peer, kind, id));
        let refuse = |error, send| Err(PeRefused { error, send });
        let number = match chunk.position() {
            ChunkPosition::Abandoned => {
                return match at {
                    Some(at) => {
                        self.streams.remove(at);
                        Ok(PeStep::Abandoned)
                    }
                    None => refuse(
                        PeReassemblyError::NoMessage(id),
                        Some(PeSignal::OutOfSequence { request_id: id }),
                    ),
                };
            }
            ChunkPosition::Number(n) => n,
        };
        let at = match (at, number == ChunkNo::FIRST) {
            (None, true) => self.open(peer, kind, &chunk, reply_flow, now)?,
            (Some(at), true) => {
                let stream = &self.streams[at];
                // A repeat of the first chunk, before a second came.
                return if stream.last == ChunkNo::FIRST {
                    Ok(Self::repeat(stream))
                } else {
                    refuse(PeReassemblyError::RequestIdInUse(id), None)
                };
            }
            (None, false) => {
                return refuse(
                    PeReassemblyError::NoMessage(id),
                    Some(PeSignal::OutOfSequence { request_id: id }),
                );
            }
            (Some(at), false) => {
                let stream = &self.streams[at];
                if number == stream.last {
                    return Ok(Self::repeat(stream));
                }
                let last = stream.last;
                let expected = last.next();
                if expected != Some(number) {
                    self.streams.remove(at);
                    return refuse(
                        PeReassemblyError::OutOfSequence {
                            expected: expected.unwrap_or(last),
                            got: number,
                        },
                        Some(PeSignal::OutOfSequence { request_id: id }),
                    );
                }
                at
            }
        };
        self.take(at, number, &chunk, now)
    }

    /// Opens the message a first chunk starts, and returns its place.
    fn open(
        &mut self,
        peer: Muid,
        kind: PeMessageKind,
        chunk: &PeChunk,
        reply_flow: PeFlowControl,
        now: Instant,
    ) -> Result<usize, PeRefused> {
        let id = chunk.request_id();
        if self.streams.len() >= usize::from(self.limits.streams.get()) {
            return Err(PeRefused {
                error: PeReassemblyError::TooManyStreams(self.limits.streams),
                send: None,
            });
        }
        let header = PeHeader::parse(chunk.header()).map_err(|e| PeRefused {
            error: e.into(),
            send: Some(PeSignal::Malformed { request_id: id }),
        })?;
        let asks = match &header {
            PeHeader::Request(r) | PeHeader::Subscription(PeSubscription::Start(r)) => {
                r.flow_control
            }
            _ => PeFlowControl::Off,
        };
        self.streams.push(Stream {
            peer,
            kind,
            request_id: id,
            header,
            data: Vec::new(),
            header_len: 0,
            declared: chunk.total(),
            last: ChunkNo::FIRST,
            deadline: now + PE_TIMEOUT,
            flow: if reply_flow == PeFlowControl::On {
                PeFlowControl::On
            } else {
                asks
            },
        });
        Ok(self.streams.len() - 1)
    }

    /// The step for a chunk the message already has.
    fn repeat(stream: &Stream) -> PeStep {
        PeStep::Pending {
            send: (stream.flow == PeFlowControl::On).then_some(PeSignal::SendNext {
                request_id: stream.request_id,
                chunk: stream.last,
            }),
        }
    }

    /// Adds chunk `number`, the next of the message at `at`.
    fn take(
        &mut self,
        at: usize,
        number: ChunkNo,
        chunk: &PeChunk,
        now: Instant,
    ) -> Result<PeStep, PeRefused> {
        let id = chunk.request_id();
        let drop_with = |this: &mut Self, error, send| {
            this.streams.remove(at);
            Err(PeRefused { error, send })
        };
        let stream = &self.streams[at];
        // The chunk is final when it names itself as the last; otherwise it
        // repeats the count the message began with.
        let last = match chunk.total() {
            ChunkTotal::Known(total) if total == number => true,
            ChunkTotal::Known(total) if total > number && stream.declared == chunk.total() => false,
            ChunkTotal::Unknown if stream.declared == ChunkTotal::Unknown => false,
            _ => {
                return drop_with(
                    self,
                    PeReassemblyError::CountMismatch,
                    Some(PeSignal::OutOfSequence { request_id: id }),
                );
            }
        };
        let others: usize = self
            .streams
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != at)
            .map(|(_, s)| s.bytes())
            .sum();
        let stream = &self.streams[at];
        let new_len = stream.bytes() + chunk.header().len() + chunk.data().len();
        if others + new_len > self.limits.bytes.get() {
            let by = if stream.kind.reply {
                PeTerminator::Initiator
            } else {
                PeTerminator::Responder
            };
            return drop_with(
                self,
                PeReassemblyError::Overflow(self.limits.bytes),
                Some(PeSignal::Terminate { request_id: id, by }),
            );
        }
        let stream = &mut self.streams[at];
        stream.header_len += chunk.header().len();
        stream.data.extend_from_slice(chunk.data());
        stream.last = number;
        stream.deadline = now + PE_TIMEOUT;
        let acknowledge =
            stream.flow == PeFlowControl::On && (!last || stream.kind == PeMessageKind::GET_REPLY);
        let send = acknowledge.then_some(PeSignal::SendNext {
            request_id: id,
            chunk: number,
        });
        if !last {
            return Ok(PeStep::Pending { send });
        }
        let stream = self.streams.remove(at);
        Ok(PeStep::Complete {
            message: Box::new(PeReceived {
                peer: stream.peer,
                kind: stream.kind,
                request_id: stream.request_id,
                header: stream.header,
                body: stream.data,
            }),
            send,
        })
    }

    /// Drops the messages whose next chunk is due by `now` and returns what to
    /// send for each.
    ///
    /// A chunk is due [`PE_TIMEOUT`] after the one before it; the time between
    /// chunks is under that, so a chunk that arrives at the deadline is late
    /// (M2-103 §12.2).
    pub fn expire(&mut self, now: Instant) -> Vec<PeExpired> {
        let mut expired = Vec::new();
        self.streams.retain(|s| {
            let due = s.deadline <= now;
            if due {
                expired.push(PeExpired {
                    peer: s.peer,
                    kind: s.kind,
                    send: PeSignal::TimedOut {
                        request_id: s.request_id,
                    },
                });
            }
            !due
        });
        expired
    }

    /// Extends the timeout of a message by the wait its sender asked for in a
    /// Timeout Wait ACK, added to the time that is left (M2-103 §12.2.1). Returns
    /// whether the message is held.
    pub fn extend(
        &mut self,
        peer: Muid,
        kind: PeMessageKind,
        request_id: PeRequestId,
        wait: TimeoutWait,
    ) -> bool {
        match self.find(peer, kind, request_id) {
            Some(stream) => {
                wait.extend(&mut stream.deadline);
                true
            }
            None => false,
        }
    }

    /// What has come of a message still being received: its Header Data and the
    /// Property Data so far, as sent. `None` when no such message is held.
    #[must_use]
    pub fn held(
        &self,
        peer: Muid,
        kind: PeMessageKind,
        request_id: PeRequestId,
    ) -> Option<PeHeld<'_>> {
        self.streams
            .iter()
            .find(|s| s.is(peer, kind, request_id))
            .map(|s| PeHeld {
                header: &s.header,
                data: &s.data,
            })
    }

    /// Drops a message its sender ended with a Terminate NAK (M2-103 §12.1).
    /// Returns whether it was held.
    pub fn terminate(&mut self, peer: Muid, kind: PeMessageKind, request_id: PeRequestId) -> bool {
        let before = self.streams.len();
        self.streams.retain(|s| !s.is(peer, kind, request_id));
        self.streams.len() != before
    }

    /// The NAK that asks the sender for the most recent chunk again, for a chunk
    /// the caller could not read; it names the last chunk this message took
    /// (M2-103 §15.4). `None` when no such message is held.
    #[must_use]
    pub fn resend_request(
        &self,
        peer: Muid,
        kind: PeMessageKind,
        request_id: PeRequestId,
    ) -> Option<PeSignal> {
        let stream = self.streams.iter().find(|s| s.is(peer, kind, request_id))?;
        Some(PeSignal::Resend {
            request_id,
            last_good: Some(stream.last),
        })
    }

    fn find(
        &mut self,
        peer: Muid,
        kind: PeMessageKind,
        request_id: PeRequestId,
    ) -> Option<&mut Stream> {
        self.streams
            .iter_mut()
            .find(|s| s.is(peer, kind, request_id))
    }
}

#[cfg(test)]
mod tests;
