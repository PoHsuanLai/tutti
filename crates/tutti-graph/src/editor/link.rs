//! The editor's end of the rings to its executor.

use std::sync::Arc;

use ringbuf::traits::{Consumer, Producer};
use tutti_rt::FaultLatch;
use tutti_types::At;

use super::CommitError;
use crate::command::{CommandId, CommandTx, ScheduleError};
use crate::event::EventKind;
use crate::exec::{Channels, Commit, Crossfade, FADE_CAPACITY, QUEUE_CAPACITY};
use crate::gain::EdgeFade;
use crate::spec::EventIn;
use crate::{EdgeId, Executor};

/// The control end of the rings to the paired executor, and the counts that
/// keep every push within them.
///
/// At most [`QUEUE_CAPACITY`] commits are out (sent and not yet handed back),
/// and the crossfades they start hold at most [`FADE_CAPACITY`] slots on the
/// fade-return ring, so a push this link makes after its check never fails.
pub(super) struct Link {
    channels: Channels,
    commands: CommandTx,
    /// Commits sent and not yet handed back.
    out: usize,
    /// Crossfades sent (started or waiting) and not yet handed back: the
    /// fade-return ring's slots in use.
    fades_out: usize,
    /// Commits sent, ever: the sequence number of the last one.
    sent: u64,
    /// Commits handed back so far: the executor applied each one before it
    /// did, and every commit is handed back once.
    acked: u64,
    /// Taps left out of the plan, each with the commit (by `sent`) that
    /// leaves them out once one was sent; their latch is raised once the
    /// executor has applied it.
    ending_taps: Vec<(Option<u64>, Arc<FaultLatch>)>,
}

impl Link {
    pub(super) fn new(channels: Channels, commands: CommandTx) -> Self {
        Self {
            channels,
            commands,
            out: 0,
            fades_out: 0,
            sent: 0,
            acked: 0,
            ending_taps: Vec::new(),
        }
    }

    /// `Backpressure` when [`QUEUE_CAPACITY`] commits are out.
    pub(super) fn check_room(&self) -> Result<(), CommitError> {
        if self.out >= QUEUE_CAPACITY {
            return Err(CommitError::Backpressure);
        }
        Ok(())
    }

    /// `Backpressure` when `fades` more crossfades would take more than
    /// [`FADE_CAPACITY`] slots on the fade-return ring.
    pub(super) fn check_fade_room(&self, fades: usize) -> Result<(), CommitError> {
        if self.fades_out + fades > FADE_CAPACITY {
            return Err(CommitError::Backpressure);
        }
        Ok(())
    }

    /// The sequence number the next commit sends with.
    pub(super) fn next_seq(&self) -> u64 {
        self.sent + 1
    }

    /// Sends `commit`, which starts `fades` crossfades: the caller checked
    /// [`check_room`](Self::check_room) and
    /// [`check_fade_room`](Self::check_fade_room). A tap ending since the
    /// last commit is left out of this one.
    pub(super) fn send(&mut self, commit: Box<Commit>, fades: usize) {
        self.fades_out += fades;
        debug_assert!(self.fades_out <= FADE_CAPACITY);
        self.push(commit);
        let sent = self.sent;
        for (at, _) in &mut self.ending_taps {
            at.get_or_insert(sent);
        }
    }

    /// Sends a re-prepare's first half: the caller checked
    /// [`check_room`](Self::check_room).
    pub(super) fn send_suspend(&mut self, suspend: Box<Commit>) {
        self.push(suspend);
    }

    fn push(&mut self, commit: Box<Commit>) {
        if self.channels.to_executor.try_push(commit).is_err() {
            unreachable!("the queue has a free slot for every credit");
        }
        self.sent += 1;
        self.out += 1;
    }

    /// The next commit the executor handed back, applied.
    pub(super) fn pop_returned(&mut self) -> Option<Box<Commit>> {
        let done = self.channels.returned.try_pop()?;
        self.out -= 1;
        self.acked += 1;
        Some(done)
    }

    /// Raises the latch of every ending tap whose last commit the executor
    /// has applied. The ring that carried the commit back orders the tap's
    /// last writes before this raise.
    pub(super) fn end_applied_taps(&mut self) {
        let acked = self.acked;
        self.ending_taps.retain(|(at, end)| match at {
            Some(sent) if *sent <= acked => {
                end.raise();
                false
            }
            _ => true,
        });
    }

    /// Ends a tap the executor holds the writer of: its latch is raised once
    /// the next commit, which leaves it out, is applied.
    pub(super) fn end_tap(&mut self, end: Arc<FaultLatch>) {
        self.ending_taps.push((None, end));
    }

    /// The next crossfade the executor handed back.
    pub(super) fn pop_faded(&mut self) -> Option<Box<Crossfade>> {
        let x = self.channels.faded.try_pop()?;
        self.fades_out -= 1;
        Some(x)
    }

    /// The next edge whose declick fade ended, by the kind that did.
    pub(super) fn pop_edge_fade(&mut self) -> Option<(EdgeId, EdgeFade)> {
        self.channels.edge_fades.try_pop()
    }

    /// The next port that lost events.
    pub(super) fn pop_loss(&mut self) -> Option<crate::EventLoss> {
        self.channels.losses.try_pop()
    }

    /// Refusals since the last call that no loss names.
    pub(super) fn take_unreported(&mut self) -> u64 {
        self.channels
            .unreported
            .swap(0, std::sync::atomic::Ordering::Relaxed)
    }

    /// The executor's play clock, as it last published it.
    pub(super) fn play_reading(&self) -> crate::PlayReading {
        self.channels.play.read()
    }

    /// Commits sent and not yet handed back.
    pub(super) fn in_flight(&self) -> usize {
        self.out
    }

    /// Crossfades sent and not yet handed back.
    pub(super) fn fades_in_flight(&self) -> usize {
        self.fades_out
    }

    /// Whether `executor` holds the other end of these rings.
    pub(super) fn is_paired_with(&self, executor: &Executor) -> bool {
        self.commands.same_pair(executor.commands())
    }

    /// Sends one checked command, stamped with the last commit sent.
    pub(super) fn schedule(
        &mut self,
        at: At,
        to: EventIn,
        kind: EventKind,
    ) -> Result<CommandId, ScheduleError> {
        self.commands.send(self.sent, at, to, kind)
    }

    /// Takes back command `id`, if it has not landed.
    pub(super) fn cancel(&mut self, id: CommandId) -> Result<(), ScheduleError> {
        self.commands.cancel(id)
    }

    /// Takes back every command that has not landed.
    pub(super) fn cancel_all(&mut self) -> Result<(), ScheduleError> {
        self.commands.cancel_all()
    }

    /// Scheduled commands sent and not yet delivered.
    pub(super) fn commands_outstanding(&self) -> usize {
        self.commands.outstanding() as usize
    }
}

impl std::fmt::Debug for Link {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Link")
            .field("out", &self.out)
            .field("fades_out", &self.fades_out)
            .field("sent", &self.sent)
            .field("acked", &self.acked)
            .finish_non_exhaustive()
    }
}
