//! What the control side can observe of a stream: its [`StreamStatus`], a
//! level the streamer writes and [`Stream::status`](crate::Stream::status)
//! reads (doc 025 §2.6).
//!
//! A level, not a queue: a slow reader loses nothing it needs (it sees the
//! current state), nothing allocates and there is no overflow policy to decide.
//! The phase and the fault are one word, so a reader never sees a fault set
//! and the phase not yet moved, or the reverse. The word is written by the
//! streamer alone (its thread, or the caller's step for a hand-stepped one,
//! and a record's drop on the same side); the audio side never writes it. It
//! is a scalar atomic, so it needs no structure of its own.
//!
//! A fault is **sticky**: the first one a stream meets is the one it reports,
//! and no later phase replaces it. A decode fault the stream's reader later
//! seeks away from is still the stream's history, and `StreamerGone` must not
//! hide the `Panicked` that ended the stream first.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use crate::voice::LoopSetting;

/// Why a stream stopped being served.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum StreamFault {
    /// The file could not be read at some position (a damaged file, a cut one,
    /// a seek the decoder refused). The frames before the fault were kept; the
    /// ring holds silence from it, which is what the voice plays.
    Decode,
    /// The streamer's work for this stream panicked. It was contained: this
    /// stream is no longer served, the others are unaffected.
    Panicked,
    /// The streamer is gone (its thread exited, or it was dropped), so nothing
    /// feeds the ring any more.
    StreamerGone,
}

/// Where a stream is, as far as the control side can tell.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum StreamPhase {
    /// Open, but the ring is not yet primed: the streamer has not filled it
    /// to its refill threshold (or to the file's end) since the stream
    /// opened. A voice that starts now would underrun. A stream leaves
    /// `Opening` once and never returns to it, so this is the readiness to
    /// wait on: a status that is not `Opening` is primed (or faulted).
    Opening,
    /// Primed and served: the streamer keeps the ring filled ahead of the
    /// reader.
    Live,
    /// The whole file, to its end, is in the ring: nothing more will be read
    /// from the file for a reader that plays on (past the end it reads
    /// silence). A looped stream has no end and is never `Ended`. A reader
    /// that jumps back out of the window moves the window and the stream
    /// is `Live` again; it is a level, not an event.
    Ended,
    /// The stream is not served, for the reason it carries. Permanent.
    Faulted(StreamFault),
}

/// A stream's status: one level read ([`Stream::status`](crate::Stream::status)).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StreamStatus {
    /// Where the stream is.
    pub phase: StreamPhase,
    /// The loop the stream runs, as the streamer applied it. It can differ from
    /// the one asked for ([`Stream::set_loop`](crate::Stream::set_loop)): a
    /// lead-in that cannot be read plays the loop hard, with no crossfade, and
    /// this says so. It is the loop an export fork of the stream plays.
    /// [`LoopSetting::Off`] until the streamer has applied one.
    pub loop_: LoopSetting,
    /// Frames the voice needed and the ring could not supply, since the stream
    /// opened: each was read as silence, and the next sounding frames ramp in.
    pub underruns: u64,
}

/// The cell behind a [`StreamPhase`]: one atomic word, shared by the
/// streamer's [`Record`](super::record::Record), the stream's handle and its
/// voice (which asks whether the record is [`closed`](Self::closed)).
#[derive(Debug)]
pub(crate) struct StatusCell {
    word: AtomicU8,
    /// The stream's record is gone: closed by its handle, faulted away or
    /// dropped with the streamer. Apart from the word because a fault is sticky
    /// and a stream that met a decode fault can still be closed.
    closed: AtomicBool,
}

const OPENING: u8 = 0;
const LIVE: u8 = 1;
const ENDED: u8 = 2;
// Every value from here is a fault: sticky.
const DECODE: u8 = 3;
const PANICKED: u8 = 4;
const GONE: u8 = 5;

impl StatusCell {
    pub(crate) fn new() -> Self {
        Self {
            word: AtomicU8::new(OPENING),
            closed: AtomicBool::new(false),
        }
    }

    /// Streamer: the stream's record is gone.
    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }

    /// Whether the stream's record is gone: nothing feeds its ring any more,
    /// and an export fork of its voice has no stream to describe.
    pub(crate) fn closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Streamer: move to a phase that is not a fault. Ignored once faulted.
    pub(crate) fn set(&self, phase: Served) {
        let word = match phase {
            Served::Live => LIVE,
            Served::Ended => ENDED,
        };
        let _ = self
            .word
            .try_update(Ordering::Release, Ordering::Relaxed, |now| {
                (now < DECODE).then_some(word)
            });
    }

    /// Streamer: fault the stream. The first fault stays.
    pub(crate) fn fault(&self, fault: StreamFault) {
        let word = match fault {
            StreamFault::Decode => DECODE,
            StreamFault::Panicked => PANICKED,
            StreamFault::StreamerGone => GONE,
        };
        let _ = self
            .word
            .try_update(Ordering::Release, Ordering::Relaxed, |now| {
                (now < DECODE).then_some(word)
            });
    }

    /// Control side: the level now.
    pub(crate) fn phase(&self) -> StreamPhase {
        match self.word.load(Ordering::Acquire) {
            OPENING => StreamPhase::Opening,
            LIVE => StreamPhase::Live,
            ENDED => StreamPhase::Ended,
            DECODE => StreamPhase::Faulted(StreamFault::Decode),
            PANICKED => StreamPhase::Faulted(StreamFault::Panicked),
            _ => StreamPhase::Faulted(StreamFault::StreamerGone),
        }
    }
}

/// A phase the streamer can move a healthy stream to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Served {
    Live,
    Ended,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first fault stays, and nothing moves a faulted stream back to a
    /// phase: a `StreamerGone` after a `Panicked` does not hide it, and a
    /// refill that finds the file's end after a decode fault does not clear
    /// it.
    ///
    /// Mutation (run): `fault` overwriting unconditionally -> the second
    /// fault replaces the first -> fails. Mutation (run): `set` overwriting
    /// unconditionally -> a faulted stream reads `Ended` -> fails.
    #[test]
    fn the_first_fault_stays() {
        let cell = StatusCell::new();
        assert_eq!(cell.phase(), StreamPhase::Opening);
        assert!(!cell.closed());
        cell.set(Served::Live);
        assert_eq!(cell.phase(), StreamPhase::Live);
        cell.set(Served::Ended);
        assert_eq!(cell.phase(), StreamPhase::Ended);
        cell.set(Served::Live);
        assert_eq!(cell.phase(), StreamPhase::Live, "a level moves back");

        cell.fault(StreamFault::Decode);
        assert_eq!(cell.phase(), StreamPhase::Faulted(StreamFault::Decode));
        cell.set(Served::Ended);
        cell.fault(StreamFault::StreamerGone);
        cell.fault(StreamFault::Panicked);
        assert_eq!(
            cell.phase(),
            StreamPhase::Faulted(StreamFault::Decode),
            "the first fault is the one reported"
        );
        // Closing is a fact of its own: a faulted stream is closed all the same.
        cell.close();
        assert!(cell.closed());
        assert_eq!(cell.phase(), StreamPhase::Faulted(StreamFault::Decode));
    }
}
