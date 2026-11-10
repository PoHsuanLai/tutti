//! One open stream on the streamer's side: a [`Record`] in a [`Streams`]
//! table, and the pure [`step`] that decides what a command does to the table
//! (doc 025 §2.5).
//!
//! A stream's state used to be six places that had to agree (a plan entry, a
//! link, a region, a taken reader, a record, a pin). It is one value now, and
//! `Drop for Record` is the one place a stream ends: the region (the ring's
//! writer and decoder), the pin on the cache and the fork's record all go in
//! the step that removes the record, so "link gone but region kept" cannot be
//! written. The record also holds the stream's [`StatusCell`]: what the
//! control side reads of it is written here, the level (`Opening`, `Live`,
//! `Ended`) and a decode fault by the refill, and `StreamerGone` by the drop.
//!
//! Work on one record runs under [`Record::contained`]: a panic faults that
//! stream and no other (doc 025 decision 20, B5).
//!
//! [`step`] is the Voicing mould: no thread, no clock, no file, no channel. A
//! command arrives as a [`Cmd`], the table changes, and what the driver has to
//! do about it comes back as [`Effect`]s. What touches a file (building the
//! record of an open, capturing a loop) is the driver's, before and after.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use super::cache::StreamPin;
use super::command::StreamId;
use super::control::StreamRecord;
use super::decode::panic_message;
use super::io::refill::Refilled;
use super::prefetch::RegionOut;
use super::rt_state::RtState;
use super::status::{Served, StatusCell, StreamFault};
use crate::voice::LoopSetting;

/// Where a stream is in its life. A closed stream has no record, so there is
/// no `Closing`: the step that closes it removes the record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Phase {
    /// Built and in the table, not yet answered to its caller: nothing about
    /// it is refilled and only a close reaches it. Within one handler call,
    /// in production; a step table holds it as long as it likes.
    Opening,
    /// Answered (the reader is on its way to the caller): refilled, and
    /// commands apply.
    Live,
}

/// One open stream: everything the streamer holds for it.
///
/// Dropping it ends the stream (module docs).
pub(super) struct Record {
    phase: Phase,
    /// The ring's writer, the decoder and the resident wave.
    pub(super) out: RegionOut,
    /// The cell the voice shares: the streamer reads speed, direction and
    /// stretch from it to size and steer a fill.
    pub(super) rt_state: Arc<RtState>,
    /// The file's length in frames, as the stream found it: what a loop's end
    /// is clamped to, as every tier clamps it (`LoopSpan::new`).
    pub(super) file_frames: u64,
    /// What a fork of the stream's voice needs; ended when this goes.
    shared: Arc<StreamRecord>,
    /// Keeps the streamed wave pinned in the cache for exactly the stream's
    /// life, so a fully-buffered (hence cold) stream is never evicted
    /// mid-read.
    _pin: StreamPin,
    /// What the control side reads of this stream ([`Stream::status`](crate::Stream::status)).
    status: Arc<StatusCell>,
    /// Its work panicked: faulted, and removed at the end of the cycle
    /// ([`Streams::reap`]).
    dead: bool,
}

impl Record {
    pub(super) fn new(
        out: RegionOut,
        rt_state: Arc<RtState>,
        file_frames: u64,
        shared: Arc<StreamRecord>,
        pin: StreamPin,
        status: Arc<StatusCell>,
    ) -> Self {
        Self {
            phase: Phase::Opening,
            out,
            rt_state,
            file_frames,
            shared,
            _pin: pin,
            status,
            dead: false,
        }
    }

    /// Runs `work` on this stream under `catch_unwind`. A panic is this stream's
    /// fault, not the streamer's: the stream is faulted
    /// ([`StreamFault::Panicked`]) and marked for removal, and `None` comes
    /// back; every other stream goes on.
    pub(super) fn contained<R>(&mut self, work: impl FnOnce(&mut Self) -> R) -> Option<R> {
        match catch_unwind(AssertUnwindSafe(|| work(self))) {
            Ok(done) => Some(done),
            Err(payload) => {
                tracing::error!(
                    "{}: the streamer's work panicked ({}); this stream is faulted, the others go on",
                    self.id(),
                    panic_message(payload.as_ref())
                );
                self.status.fault(StreamFault::Panicked);
                self.dead = true;
                None
            }
        }
    }

    /// Publishes where a refill left the stream: a failed read is a decode
    /// fault (it stays); the file's end in the ring is `Ended`; a primed ring
    /// is `Live`. A ring not yet primed leaves the status as it was, so a
    /// stream is `Opening` until its first primed refill and `Live` after it.
    pub(super) fn settle(&self, refilled: &Refilled) {
        if !refilled.read_ok {
            self.status.fault(StreamFault::Decode);
        }
        let ended = self
            .out
            .content()
            .current
            .end()
            .is_some_and(|end| self.out.window().1 >= end);
        if ended {
            self.status.set(Served::Ended);
        } else if refilled.primed {
            self.status.set(Served::Live);
        }
    }

    pub(super) fn id(&self) -> StreamId {
        self.out.region_id()
    }

    #[cfg(test)]
    pub(super) fn phase(&self) -> Phase {
        self.phase
    }

    /// Whether the refill follows this stream's reader.
    pub(super) fn is_live(&self) -> bool {
        self.phase == Phase::Live && !self.dead
    }

    /// Records the loop the stream now runs, as applied, for a fork to read.
    /// The one place a loop changes, so the streamer's loop and a fork's
    /// cannot disagree.
    pub(super) fn set_loop(&self, applied: LoopSetting) {
        self.shared.set_loop(applied);
    }
}

/// The stream is over the moment its record goes, so a voice still holding the
/// fork's record, on a ring the streamer no longer feeds, forks to silence as
/// it plays live, and a handle that outlives the streamer reads
/// `Faulted(StreamerGone)` (a fault already there stays). This is also how a
/// panic that escapes the per-stream containment tells every stream: it drops
/// the whole table.
impl Drop for Record {
    fn drop(&mut self) {
        self.status.fault(StreamFault::StreamerGone);
        self.shared.end();
    }
}

/// A loop the control side asked for: `(start, end)` in file frames and the
/// crossfade in frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct LoopAsk {
    pub(super) range: (u64, u64),
    pub(super) crossfade_frames: usize,
}

/// A command as the pure step sees it: data, with the file work already done.
pub(super) enum Cmd {
    /// A stream the driver has built, in [`Phase::Opening`].
    Open(Box<Record>),
    /// The open's reader reached its caller: the stream is [`Phase::Live`].
    Opened(StreamId),
    /// End the stream.
    Close(StreamId),
    /// Set (`Some`) or clear (`None`) the stream's loop.
    Loop(StreamId, Option<LoopAsk>),
}

/// What the driver does after a step. Nothing here is the table's own change:
/// that has happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Effect {
    /// The stream went live (an open's reader was delivered).
    Live(StreamId),
    /// The stream's record is gone, and its region with it.
    Freed(StreamId),
    /// Capture this loop into the stream's ring and record it as applied
    /// (`handle_loop`): it reads the file.
    Loop(StreamId, Option<LoopAsk>),
}

/// Every open stream, one [`Record`] each, found by id.
///
/// A `Vec`, not a map: the parallel refill hands each worker a disjoint
/// `&mut Record` (`par_iter_mut` needs `Send`, not `Sync`), a session has tens
/// of streams, and a command finds its stream by a scan of ids that are
/// minted once and never reused.
pub(super) struct Streams {
    records: Vec<Record>,
    /// What the last step asked of the driver.
    effects: Vec<Effect>,
}

impl Streams {
    pub(super) fn new() -> Self {
        Self {
            records: Vec::new(),
            effects: Vec::with_capacity(2),
        }
    }

    pub(super) fn len(&self) -> usize {
        self.records.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    fn position(&self, id: StreamId) -> Option<usize> {
        self.records.iter().position(|r| r.id() == id)
    }

    pub(super) fn get(&self, id: StreamId) -> Option<&Record> {
        self.records.iter().find(|r| r.id() == id)
    }

    pub(super) fn get_mut(&mut self, id: StreamId) -> Option<&mut Record> {
        self.records.iter_mut().find(|r| r.id() == id)
    }

    pub(super) fn records(&self) -> &[Record] {
        &self.records
    }

    pub(super) fn records_mut(&mut self) -> &mut [Record] {
        &mut self.records
    }

    /// Removes every stream whose work panicked: dropping its record frees its
    /// region, and its fault (already published) is what its handle reads.
    pub(super) fn reap(&mut self) {
        self.records.retain(|record| !record.dead);
    }
}

/// Applies `cmd` to `streams` and says what the driver does about it.
///
/// A command for a stream the table no longer holds changes nothing and asks
/// for nothing: commands race the streams they name, and an id is minted once
/// and never reused, so a late command cannot reach another stream (B4).
pub(super) fn step(streams: &mut Streams, cmd: Cmd) -> &[Effect] {
    streams.effects.clear();
    match cmd {
        Cmd::Open(record) => {
            // Minted once: a duplicate never happens, and is dropped (and
            // freed) rather than shadowing the stream already held.
            if streams.position(record.id()).is_none() {
                streams.records.push(*record);
            }
        }
        Cmd::Opened(id) => {
            if let Some(record) = streams.get_mut(id)
                && record.phase == Phase::Opening
            {
                record.phase = Phase::Live;
                streams.effects.push(Effect::Live(id));
            }
        }
        Cmd::Close(id) => {
            if let Some(at) = streams.position(id) {
                // Dropping the record is the free.
                streams.records.swap_remove(at);
                streams.effects.push(Effect::Freed(id));
            }
        }
        Cmd::Loop(id, ask) => {
            if streams.get(id).is_some_and(Record::is_live) {
                streams.effects.push(Effect::Loop(id, ask));
            }
        }
    }
    &streams.effects
}

#[cfg(test)]
pub(super) mod tests;
